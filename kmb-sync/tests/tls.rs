#![cfg(feature = "tls")]
#![cfg(test)]

use kmb::{Frame, FrameDecoder};
use kmb_sync::config::Config;
use kmb_sync::service::{Outcome, Service};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, PKCS_ECDSA_P256_SHA256,
};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const METER_NAME: &str = "meter 12345";
const IDENTIFY: [u8; 26] = [
    0x30, 0x39, 0x94, 0x00, 0x00, 0x30, 0x18, 0xD1, 0x00, 0x02, 0x00, 0xA0, 0x01, 0x30, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
];
const RTC: [u8; 10] = [0x00, 0x01, 0x00, 0x00, 0x00, 0xC4, 0x82, 0x39, 0x8E, 0x00];

struct Chain {
    root_point: String,
    certificates: Vec<CertificateDer<'static>>,
    leaf_key: PrivateKeyDer<'static>,
}

fn ca(name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
}

fn chain_for(leaf_name: &str) -> Chain {
    let root_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let root_params = ca("Test Root CA");
    let root = Issuer::from_params(&root_params, &root_key);
    let intermediate_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let intermediate_params = ca("Intermediate CA");
    let intermediate = intermediate_params
        .signed_by(&intermediate_key, &root)
        .unwrap();
    let intermediate_issuer = Issuer::from_params(&intermediate_params, &intermediate_key);
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut leaf_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, leaf_name);
    let leaf = leaf_params
        .signed_by(&leaf_key, &intermediate_issuer)
        .unwrap();
    let root_point = root_key
        .public_key_raw()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        });
    assert!(root_point.starts_with("04") && root_point.len() == 130);
    Chain {
        root_point,
        certificates: vec![leaf.der().clone(), intermediate.der().clone()],
        leaf_key: PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
    }
}

#[derive(Default)]
struct Meter {
    requests: Vec<u8>,
}

impl Meter {
    fn answer(&mut self, request: &Frame) -> Vec<Frame> {
        self.requests.push(request.command);
        let reply = |payload: &[u8]| vec![Frame::new(request.address, request.command, payload)];
        match request.command {
            1 => reply(&IDENTIFY),
            17 => reply(&RTC),
            18 => reply(&[0]),
            _ => vec![Frame::new(request.address, request.command | 0x80, [0xEE])],
        }
    }
}

async fn serve_connection<S>(mut io: S, meter: Arc<Mutex<Meter>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut decoder = FrameDecoder::new();
    let mut buf = [0; 4096];
    loop {
        let n = match io.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        decoder.push(&buf[..n]);
        while let Some(Ok(request)) = decoder.next_frame() {
            let replies = meter.lock().unwrap().answer(&request);
            for reply in replies {
                io.write_all(&reply.encode().unwrap()).await.unwrap();
            }
            io.flush().await.unwrap();
        }
    }
}

async fn start_tls_meter(chain: &Chain, meter: Arc<Mutex<Meter>>) -> u16 {
    let server = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain.certificates.clone(), chain.leaf_key.clone_key())
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((io, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let meter = meter.clone();
            tokio::spawn(async move {
                if let Ok(stream) = acceptor.accept(io).await {
                    serve_connection(stream, meter).await;
                }
            });
        }
    });
    port
}

fn config(dir: &std::path::Path, port: u16, device: &str, job: &str) -> Config {
    let text = format!(
        r#"
[service]
database = "{}"

[[device]]
name = "m"
host = "127.0.0.1"
port = {port}
path = ["Test", "Station"]
byte_timeout = "2s"
tls = true
{device}

[[job]]
device = "m"
kind = "clock"
{job}
"#,
        dir.join("t.db").display()
    );
    let mut c: Config = toml::from_str(&text).unwrap();
    c.check().unwrap();
    c
}

async fn run_clock(service: &Service) -> Outcome {
    let slot = service.job("m-clock-1").unwrap().clone();
    service.run_once(&slot).await.unwrap().1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clock_job_runs_over_tls_to_the_meter_with_the_expected_common_name() {
    let chain = chain_for(METER_NAME);
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_tls_meter(&chain, meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let device = format!(
        "tls_root = \"{}\"\ntls_common_name = \"{METER_NAME}\"",
        chain.root_point
    );
    let service = Service::new(config(dir.path(), port, &device, "")).unwrap();
    let Outcome::Done(summary) = run_clock(&service).await else {
        panic!("the clock job fails over TLS");
    };
    assert!(summary.notes[0].starts_with("clock offset"), "{summary:?}");
    assert_eq!(meter.lock().unwrap().requests, [1, 17]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_with_another_common_name_is_refused() {
    let chain = chain_for("meter 54321");
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_tls_meter(&chain, meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let device = format!(
        "tls_root = \"{}\"\ntls_common_name = \"{METER_NAME}\"",
        chain.root_point
    );
    let service = Service::new(config(dir.path(), port, &device, "")).unwrap();
    let outcome = run_clock(&service).await;
    let Outcome::Failed(text) = outcome else {
        panic!("a certificate for another meter is accepted: {outcome:?}");
    };
    assert!(text.contains("certificate is for another meter"), "{text}");
    assert_eq!(meter.lock().unwrap().requests, Vec::<u8>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_from_another_root_is_refused() {
    let chain = chain_for(METER_NAME);
    let other = chain_for(METER_NAME);
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_tls_meter(&chain, meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let device = format!(
        "tls_root = \"{}\"\ntls_common_name = \"{METER_NAME}\"",
        other.root_point
    );
    let service = Service::new(config(dir.path(), port, &device, "")).unwrap();
    let outcome = run_clock(&service).await;
    let Outcome::Failed(text) = outcome else {
        panic!("a certificate from another root is accepted: {outcome:?}");
    };
    assert!(text.contains("InvalidSignatureForPublicKey"), "{text}");
    assert_eq!(meter.lock().unwrap().requests, Vec::<u8>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn refuse_cleartext_pin_sends_the_pin_over_tls() {
    let chain = chain_for(METER_NAME);
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_tls_meter(&chain, meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("m.cred");
    std::fs::write(&credentials, "user\npassword\n").unwrap();
    let device = format!(
        "tls_root = \"{}\"\ntls_common_name = \"{METER_NAME}\"\ncredentials_file = \"{}\"\n\
         refuse_cleartext_pin = true",
        chain.root_point,
        credentials.display()
    );
    let service = Service::new(config(dir.path(), port, &device, "adjust_above = \"1s\"")).unwrap();
    let outcome = run_clock(&service).await;
    let Outcome::Done(summary) = outcome else {
        panic!("the PIN is refused over TLS: {outcome:?}");
    };
    assert!(
        summary.notes.iter().any(|n| n.starts_with("clock set to")),
        "{summary:?}"
    );
    assert!(meter.lock().unwrap().requests.contains(&18));
}
