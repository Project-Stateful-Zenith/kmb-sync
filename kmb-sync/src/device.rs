use crate::clock;
use crate::config::{self, Device};
use anyhow::{Context, Result};
use kmb::Identify;
use kmb::archive::uni::{self as uni_archive, Layout};
use kmb::envelope::Psk;
use kmb::uni::{ArchiveDescription, DataSet, Description, Value};
use kmb_store::{DefinitionId, IdentityInfo};
use kmb_tokio::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

pub const LOCKOUT_AFTER: u32 = 10;
pub const LOCKOUT_MS: i64 = 30_000;

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub type MeterClient = Client<Box<dyn Stream>>;

#[derive(Clone, Debug, Default)]
pub struct Traffic {
    rx: Arc<AtomicU64>,
    tx: Arc<AtomicU64>,
}

impl Traffic {
    #[must_use]
    pub fn get(&self) -> kmb_store::Traffic {
        kmb_store::Traffic {
            rx: self.rx.load(Ordering::Relaxed),
            tx: self.tx.load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        let t = self.get();
        t.rx.saturating_add(t.tx)
    }

    fn add(counter: &AtomicU64, n: usize) {
        counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

struct Counted {
    inner: Box<dyn Stream>,
    traffic: Traffic,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if r.is_ready() {
            Traffic::add(&self.traffic.rx, buf.filled().len().saturating_sub(before));
        }
        r
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = r {
            Traffic::add(&self.traffic.tx, n);
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Clone, Default)]
pub struct Secrets {
    pub credentials: Option<(String, String)>,
    pub psk: Option<Psk>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("credentials", &self.credentials.as_ref().map(|_| "<set>"))
            .field("psk", &self.psk.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Secrets {
    pub fn load(d: &Device) -> Result<Self> {
        let credentials = d
            .credentials_file
            .as_deref()
            .map(config::read_credentials)
            .transpose()?;
        let psk = d
            .psk_file
            .as_deref()
            .map(|p| -> Result<Psk> {
                let text = config::read_secret(p)?;
                Psk::from_base64(text.trim()).map_err(|e| anyhow::anyhow!("{}: {e:?}", p.display()))
            })
            .transpose()?;
        Ok(Self { credentials, psk })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IdentitySummary {
    #[serde(flatten)]
    pub info: IdentityInfo,
    pub universal: bool,
    pub encrypted: bool,
}

impl IdentitySummary {
    #[must_use]
    pub fn new(id: &Identify, encrypted: bool) -> Self {
        Self {
            info: identity_info(id),
            universal: id.is_universal(),
            encrypted,
        }
    }
}

#[must_use]
pub fn identity_info(id: &Identify) -> IdentityInfo {
    IdentityInfo {
        device_no: id.device_no,
        device_type: id.device_type,
        props_type: id.props_type,
        software_version: id.software_version,
        hardware_version: id.hardware_version,
        software_modules: id.software_modules,
        bootloader_version: id.bootloader_version,
        firmware: id.firmware_version(),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub url: String,
    pub last_ok: Option<i64>,
    pub last_error: Option<String>,
    pub connect_failures: u32,
    pub locked_until: Option<i64>,
    pub identity: Option<IdentitySummary>,
    pub clock_offset_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archives: Option<Vec<ArchiveState>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ArchiveState {
    pub archive: u8,
    pub name: String,
    pub period_ms: u64,
    pub unsupported: Option<String>,
    pub synced_at: Option<i64>,
    pub inserted: Option<usize>,
    pub newest: Option<i64>,
    pub error: Option<String>,
}

impl DeviceStatus {
    pub fn set_offered(&mut self, offered: Vec<ArchiveState>) {
        let old = self.archives.take().unwrap_or_default();
        self.archives = Some(
            offered
                .into_iter()
                .map(|mut a| {
                    if let Some(o) = old.iter().find(|o| o.archive == a.archive) {
                        a.synced_at = o.synced_at;
                        a.inserted = o.inserted;
                        a.newest = o.newest;
                        a.error.clone_from(&o.error);
                    }
                    a
                })
                .collect(),
        );
    }

    pub fn archive_synced(
        &mut self,
        archive: u8,
        inserted: usize,
        newest: Option<i64>,
        error: Option<String>,
    ) {
        let list = self.archives.get_or_insert_with(Vec::new);
        if !list.iter().any(|a| a.archive == archive) {
            list.push(ArchiveState {
                archive,
                name: uni_archive::archive_name(archive).unwrap_or("?").into(),
                ..ArchiveState::default()
            });
        }
        let Some(a) = list.iter_mut().find(|a| a.archive == archive) else {
            return;
        };
        a.synced_at = Some(clock::now_ms());
        a.inserted = Some(inserted);
        a.newest = newest.or(a.newest);
        a.error = error;
    }
}

pub struct Described {
    pub identify: Identify,
    pub config: Description,
    pub archives: Option<ArchiveDescription>,
    pub layouts: HashMap<u8, (Layout, DefinitionId)>,
    pub sets: HashMap<DataSet, Description>,
    pub at: Instant,
}

impl Described {
    #[must_use]
    pub fn header(&self, archives: &ArchiveDescription) -> (u32, usize) {
        let arch_ver = self
            .config
            .value("Message", "ArchVer")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        let max_size = self
            .config
            .value("Message", "MaxArchSize")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(uni_archive::DEFAULT_MAX_ARCH_SIZE);
        (
            uni_archive::header_version(&archives.fields, arch_ver),
            max_size,
        )
    }

    #[must_use]
    pub fn matches(&self, id: &Identify) -> bool {
        let a = &self.identify;
        (a.device_no, a.device_type, a.props_type, a.software_version)
            == (
                id.device_no,
                id.device_type,
                id.props_type,
                id.software_version,
            )
    }
}

pub struct DeviceSlot {
    pub config: Device,
    pub secrets: Secrets,
    pub session: tokio::sync::Mutex<Option<Described>>,
    pub status: Mutex<DeviceStatus>,
}

impl DeviceSlot {
    #[must_use]
    pub fn new(config: Device, secrets: Secrets) -> Arc<Self> {
        let status = DeviceStatus {
            name: config.name.clone(),
            url: config.url(),
            ..DeviceStatus::default()
        };
        Arc::new(Self {
            config,
            secrets,
            session: tokio::sync::Mutex::new(None),
            status: Mutex::new(status),
        })
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut DeviceStatus) -> R) -> R {
        let mut s = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut s)
    }

    #[must_use]
    pub fn snapshot(&self) -> DeviceStatus {
        self.update(|s| s.clone())
    }

    pub async fn connect(&self, traffic: &Traffic) -> Result<MeterClient> {
        let now = clock::now_ms();
        if let Some(until) = self.update(|s| s.locked_until.filter(|&u| u > now)) {
            anyhow::bail!(
                "connects refused until {} after {LOCKOUT_AFTER} failures",
                clock::rfc3339(until)
            );
        }
        match self.open(traffic).await {
            Ok(client) => {
                self.update(|s| {
                    s.connect_failures = 0;
                    s.locked_until = None;
                    s.last_ok = Some(clock::now_ms());
                });
                Ok(client)
            }
            Err(e) => {
                self.update(|s| {
                    if s.locked_until.is_some() {
                        s.connect_failures = 0;
                        s.locked_until = None;
                    }
                    s.connect_failures = s.connect_failures.saturating_add(1);
                    if s.connect_failures > LOCKOUT_AFTER {
                        s.locked_until = Some(clock::now_ms().saturating_add(LOCKOUT_MS));
                    }
                    s.last_error = Some(format!("{e:#}"));
                });
                Err(e)
            }
        }
    }

    async fn open(&self, traffic: &Traffic) -> Result<MeterClient> {
        let d = &self.config;
        let addr = (d.host.as_str(), d.port());
        if d.tls && !cfg!(feature = "tls") {
            anyhow::bail!("{} needs TLS: build with --features tls", d.url());
        }
        let stream = tokio::time::timeout(d.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow::anyhow!("connect to {} timed out", d.url()))?
            .with_context(|| format!("connect to {}", d.url()))?;
        stream.set_nodelay(true)?;
        let io: Box<dyn Stream> = Box::new(Counted {
            inner: Box::new(stream),
            traffic: traffic.clone(),
        });
        #[cfg(feature = "tls")]
        let io = if d.tls { self.open_tls(io).await? } else { io };
        tokio::time::sleep(kmb_tokio::SETTLE).await;
        let mut client = Client::new(io, d.address);
        client.set_byte_timeout(d.byte_timeout);
        client.set_one_frame_archives(true);
        if let Some((user, password)) = &self.secrets.credentials {
            client.set_credentials(user, password);
        }
        if let Some(psk) = self.secrets.psk.clone() {
            client.set_keys(Box::new(move |_| Some(psk.clone())));
        }
        Ok(client)
    }

    #[cfg(feature = "tls")]
    async fn open_tls(&self, io: Box<dyn Stream>) -> Result<Box<dyn Stream>> {
        use kmb_tokio::tls::{SERVER_NAME, Trust, client_config};
        use tokio_rustls::TlsConnector;
        use tokio_rustls::rustls::pki_types::ServerName;
        let d = &self.config;
        let pins = d
            .tls_pins
            .iter()
            .map(|p| config::parse_fingerprint(p))
            .collect::<Result<Vec<_>>>()?;
        let root = d.tls_root.as_deref().map(config::parse_root).transpose()?;
        let trust = match root {
            Some((x, y)) if pins.is_empty() => Trust::Root(x, y),
            Some((x, y)) => Trust::RootOrPinned(x, y, pins),
            None => Trust::Pinned(pins),
        };
        let connector = TlsConnector::from(Arc::new(client_config(trust)));
        let name = ServerName::try_from(SERVER_NAME)?;
        let stream = tokio::time::timeout(d.connect_timeout, connector.connect(name, io))
            .await
            .map_err(|_| anyhow::anyhow!("TLS handshake with {} timed out", d.url()))?
            .with_context(|| format!("TLS connect to {}", d.url()))?;
        Ok(Box::new(stream))
    }
}
