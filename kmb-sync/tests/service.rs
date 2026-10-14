#![cfg(test)]

use kmb::archive::uni::sum;
use kmb::{Frame, FrameDecoder};
use kmb_sync::config::Config;
use kmb_sync::device::Traffic;
use kmb_sync::service::{Outcome, Service};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn hex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

const IDENTIFY: &str =
    "30 39 94 00 00 30 18 D1 00 02 00 A0 01 30 00 00 00 00 00 00 00 00 00 00 02 00";
const PHASE0: &str =
    "00 00 00 02 00 02 00 00 4D 65 73 73 61 67 65 00 02 00 49 6E 73 74 61 6C 6C 00 FF";
const PHASE1: &str = "00 00 00 03 00 03 43 6E 66 67 63 68 61 6E 67 65 00 00 00 00 01 00 55 6E 6F 6D 00 02 04 00 04 01 4E 61 6D 65 00 02 12 00 08 02";
const PHASE2: &str = "00 00 00 03 00 03 0F 43 66 00 00 53 30 31 00 00 00 00 00";
const RTC: &str = "00 01 00 00 00 C4 82 39 8E 00";
const ARCH_PHASE0: &str = "00 00 00 01 00 01  06 00 00 0000 3F800000 00 4D 61 69 6E 00  FF";
const ARCH_PHASE1: &str = "00 00 00 04 00 04
    4C 65 6E 67 74 68 00 06 00 00 00 00 01 0001 01
    53 75 6D 00         06 00 00 00 00 01 0001 01
    54 69 6D 65 00      06 00 00 00 00 13 0001 01
    55 31 00            06 00 00 00 00 04 0001 01";

fn record(time: u64, u1: f32) -> Vec<u8> {
    let mut r = vec![0x00, 0x10, 0, 0];
    r.extend(time.to_be_bytes());
    r.extend(u1.to_be_bytes());
    let s = sum(&r);
    r[2..4].copy_from_slice(&s.to_be_bytes());
    r
}

fn minute(i: u64) -> u64 {
    800_000_000_000_u64
        .checked_add(i.checked_mul(60_000).unwrap())
        .unwrap()
}

#[derive(Default)]
struct Meter {
    records: Vec<Vec<u8>>,
    requests: Vec<u8>,
}

impl Meter {
    fn time(&self, i: usize) -> u64 {
        u64::from_be_bytes(self.records[i][4..12].try_into().unwrap())
    }

    fn answer(&mut self, req: &Frame) -> Vec<Frame> {
        self.requests.push(req.command);
        let a = req.address;
        let data = |p: Vec<u8>| vec![Frame::new(a, req.command, p)];
        match req.command {
            1 => data(hex(IDENTIFY)),
            17 => data(hex(RTC)),
            112 | 114 => data(hex(match req.payload[8] {
                0 => PHASE0,
                1 => PHASE1,
                _ => PHASE2,
            })),
            116 => data(hex(if req.payload[8] == 0 {
                ARCH_PHASE0
            } else {
                ARCH_PHASE1
            })),
            22 => {
                let start =
                    usize::try_from(u32::from_be_bytes(req.payload[1..5].try_into().unwrap()))
                        .unwrap();
                let count = usize::from(u16::from_be_bytes(req.payload[5..7].try_into().unwrap()));
                let n = self.records.len();
                let mut p = vec![req.payload[0], 0, 0, 0, 0, 0, 0];
                p.extend(1000u32.to_be_bytes());
                p.extend(i32::try_from(n).unwrap().to_be_bytes());
                for r in self.records.iter().skip(start).take(count) {
                    p.extend(r);
                }
                data(p)
            }
            23 => {
                let t = u64::from_be_bytes(req.payload[1..9].try_into().unwrap());
                let found = (0..self.records.len())
                    .rev()
                    .find(|&i| self.time(i) <= t)
                    .or_else(|| (!self.records.is_empty()).then_some(0));
                let mut p = Vec::new();
                if let Some(i) = found {
                    p.extend(i32::try_from(i).unwrap().to_be_bytes());
                    p.extend(self.time(i).to_be_bytes());
                } else {
                    p.extend((-1i32).to_be_bytes());
                    p.extend(0u64.to_be_bytes());
                }
                data(p)
            }
            _ => vec![Frame::new(a, req.command | 0x80, [0xEE])],
        }
    }
}

async fn start_meter(meter: Arc<Mutex<Meter>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut io, _)) = listener.accept().await else {
                return;
            };
            let meter = meter.clone();
            tokio::spawn(async move {
                let mut dec = FrameDecoder::new();
                let mut buf = [0; 4096];
                loop {
                    let n = match io.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    dec.push(&buf[..n]);
                    while let Some(Ok(req)) = dec.next_frame() {
                        let replies = meter.lock().unwrap().answer(&req);
                        for r in replies {
                            io.write_all(&r.encode().unwrap()).await.unwrap();
                        }
                    }
                }
            });
        }
    });
    port
}

fn config(db: &std::path::Path, port: u16, extra: &str) -> Config {
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

[[job]]
device = "m"
kind = "archive"
archives = ["Main"]

[[job]]
device = "m"
kind = "values"
set = "ActData"

[[job]]
device = "m"
kind = "clock"
{extra}
"#,
        db.display()
    );
    let mut c: Config = toml::from_str(&text).unwrap();
    c.check().unwrap();
    c
}

async fn run(service: &Service, job: &str) -> kmb_sync::collect::Summary {
    let slot = service.job(job).unwrap().clone();
    match service.run_once(&slot).await.unwrap().1 {
        Outcome::Done(s) => s,
        other => panic!("{job}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_sync_resumes_and_is_idempotent() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..3).map(|i| record(minute(i), 230.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();

    let first = run(&service, "m-archive-1").await;
    assert_eq!(
        (first.inserted, first.duplicates, first.rejected),
        (3, 0, 0)
    );

    meter
        .lock()
        .unwrap()
        .records
        .extend([record(minute(3), 231.0), record(minute(4), 232.0)]);
    let second = run(&service, "m-archive-1").await;
    assert_eq!(second.inserted, 2);
    assert_eq!(second.duplicates, 0);
    assert!(
        second
            .notes
            .contains(&"archive 0: read 2 slots 3..5".to_string()),
        "{second:?}"
    );

    let requests = meter.lock().unwrap().requests.len();
    let third = run(&service, "m-archive-1").await;
    assert_eq!((third.inserted, third.duplicates), (0, 0));
    assert!(third.notes.is_empty(), "{third:?}");
    let asked = meter.lock().unwrap().requests[requests..].to_vec();
    assert_eq!(asked, [1, 22, 23]);
    let runs = service.store.call(|s| s.runs(None, 1)).await.unwrap();
    assert!(runs[0].bytes_rx > 0 && runs[0].bytes_tx > 0, "{runs:?}");

    let (measurements, rows) = service
        .store
        .call(|s| {
            let m = s.measurements()?;
            let q = kmb_store::RecordQuery {
                measurement: m[0].id,
                archive: 0,
                from: None,
                to: None,
                limit: 100,
                newest_first: false,
            };
            let recs = s.records(&q)?;
            let rows = kmb_sync::api::decode_rows(s, recs, false)?;
            Ok((m, rows))
        })
        .await
        .unwrap();
    assert_eq!(measurements[0].path, ["Test", "Station"]);
    assert_eq!(measurements[0].device_no, 12345);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[4].time_kmb, minute(4));
    assert_eq!(rows[4].fields[0]["name"], "U1");
    assert_eq!(rows[4].fields[0]["value"], 232.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn damaged_records_are_recorded_not_stored() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    let mut bad = record(minute(1), 1.0);
    bad[15] ^= 0xFF;
    meter.lock().unwrap().records = vec![record(minute(0), 1.0), bad, record(minute(2), 1.0)];
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    let s = run(&service, "m-archive-1").await;
    assert_eq!((s.inserted, s.rejected), (2, 1));
    let states = service
        .store
        .call(|st| st.download_states(1, 0, 10))
        .await
        .unwrap();
    assert_eq!(states[0].reason, "sum");
    assert_eq!((states[0].slot_from, states[0].slot_to), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn ring_overrun_reads_everything_and_says_so() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..2).map(|i| record(minute(i), 1.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    assert_eq!(run(&service, "m-archive-1").await.inserted, 2);
    meter.lock().unwrap().records = (100..104).map(|i| record(minute(i), 2.0)).collect();
    let s = run(&service, "m-archive-1").await;
    assert_eq!(s.inserted, 4);
    assert!(
        s.notes.iter().any(|n| n.contains("ring overwrote")),
        "{s:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn values_and_clock_jobs() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    assert_eq!(run(&service, "m-values-1").await.inserted, 1);
    let v = service.store.call(|s| s.latest_values(1, 1)).await.unwrap();
    assert_eq!(v[0].1["Install/Unom"], 230.0);
    assert_eq!(v[0].1["Install/Name"], "S01");
    let c = run(&service, "m-clock-1").await;
    assert!(c.notes[0].starts_with("clock offset"));
    assert!(!meter.lock().unwrap().requests.contains(&18));
    assert!(service.devices["m"].snapshot().clock_offset_ms.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_lockout_after_ten_failures() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    let device = service.devices["m"].clone();
    for _ in 0..11 {
        assert!(device.connect(&Traffic::default()).await.is_err());
    }
    let s = device.snapshot();
    assert_eq!(s.connect_failures, 11);
    assert!(s.locked_until.is_some());
    let e = device
        .connect(&Traffic::default())
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("refused until"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_requires_the_token_and_serves_records() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..2).map(|i| record(minute(i), 230.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    run(&service, "m-archive-1").await;

    let app = kmb_sync::api::router(service.clone(), Some("secret".into()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (unauth, rows, jobs, stations) = tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let unauth = agent
            .get(format!("{base}/api/jobs"))
            .call()
            .unwrap()
            .status();
        let get = |path: &str| -> serde_json::Value {
            agent
                .get(format!("{base}{path}"))
                .header("Authorization", "Bearer secret")
                .call()
                .unwrap()
                .body_mut()
                .read_json()
                .unwrap()
        };
        (
            unauth,
            get("/api/records?measurement=1&archive=0&order=desc"),
            get("/api/jobs"),
            get("/api/stations"),
        )
    })
    .await
    .unwrap();
    assert_eq!(unauth, 401);
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["time_kmb"], minute(1));
    assert_eq!(jobs[0]["name"], "m-archive-1");
    assert_eq!(jobs[0]["last_outcome"], "ok");
    let st = &stations["stations"][0];
    assert_eq!(st["jobs"][0]["name"], "m-archive-1");
    assert_eq!(st["jobs"][0]["recent"][0]["outcome"], "ok");
    assert_eq!(st["archives_known"], true);
    let main = st["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["archive"] == 0)
        .unwrap();
    assert_eq!(main["jobs"][0], "m-archive-1");
    assert_eq!(main["offered"], true);
    assert_eq!(main["inserted"], 2);
    assert!(main["newest"].as_i64().is_some() && main["error"].is_null());
    let today = &st["traffic"].as_array().unwrap().last().unwrap();
    assert_eq!(today["day"], stations["today"]);
    assert_eq!(today["runs"], 1);
    assert!(today["bytes"].as_i64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_page_is_open_and_its_api_opt_in() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..2).map(|i| record(minute(i), 230.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    run(&service, "m-archive-1").await;

    let serve = |token: Option<&str>| {
        let app = kmb_sync::api::status_router(service.clone(), token.map(Into::into));
        async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            base
        }
    };
    let open = serve(None).await;
    let with_api = serve(Some("view")).await;
    let (page, statuses) = tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let call = |url: String, token: Option<&str>| {
            let mut req = agent.get(url);
            if let Some(t) = token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            req.call().unwrap()
        };
        let page = call(format!("{open}/"), None)
            .body_mut()
            .read_to_string()
            .unwrap();
        let statuses = [
            call(format!("{open}/api/stations"), None).status(),
            call(format!("{with_api}/"), None).status(),
            call(format!("{with_api}/api/stations"), None).status(),
            call(format!("{with_api}/api/stations"), Some("secret")).status(),
            call(format!("{with_api}/api/stations"), Some("view")).status(),
            call(format!("{with_api}/api/jobs"), Some("view")).status(),
            agent
                .post(format!("{with_api}/api/jobs/m-archive-1/run"))
                .header("Authorization", "Bearer view")
                .send_empty()
                .unwrap()
                .status(),
        ];
        (page, statuses)
    })
    .await
    .unwrap();
    assert!(
        page.contains(r#""name":"m-archive-1""#),
        "the page carries its data"
    );
    assert!(!page.contains("/*DATA*/"));
    assert_eq!(statuses, [404, 200, 401, 401, 200, 404, 404]);
}

#[tokio::test(flavor = "multi_thread")]
async fn byte_limit_bounds_each_run_and_resumes() {
    const LIMIT: u64 = 2048;
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..400).map(|i| record(minute(i), 1.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(&dir.path().join("t.db"), port, "");
    if let kmb_sync::config::Task::Archive { max_size, .. } = &mut c.jobs[0].task {
        *max_size = Some(kmb_sync::config::ByteSize(LIMIT));
    }
    let service = Service::new(c).unwrap();
    let slot = service.job("m-archive-1").unwrap().clone();
    let (mut inserted, mut limited_runs) = (0, 0);
    for period in 0..20 {
        if period == 1 {
            let requests = meter.lock().unwrap().requests.len();
            let (_, again) = service.run_once(&slot).await.unwrap();
            assert_eq!(again.name(), "limited");
            assert_eq!(meter.lock().unwrap().requests.len(), requests);
        }
        slot.update(|s| s.period_bytes = 0);
        let (_, outcome) = service.run_once(&slot).await.unwrap();
        let limited = outcome.name() == "limited";
        let Outcome::Done(s) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(s.duplicates, 0, "{s:?}");
        inserted += s.inserted;
        let bytes = service.store.call(|st| st.runs(None, 1)).await.unwrap()[0].clone();
        assert!(bytes.bytes_rx + bytes.bytes_tx <= 2048 + 64, "{bytes:?}");
        if !limited {
            break;
        }
        limited_runs += 1;
    }
    assert!(limited_runs >= 2, "{limited_runs}");
    assert_eq!(inserted, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_the_meter_does_not_count_toward_the_timeout() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = vec![record(minute(0), 1.0)];
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(&dir.path().join("t.db"), port, "");
    c.jobs[0].timeout = std::time::Duration::from_millis(500);
    let service = Arc::new(Service::new(c).unwrap());
    let held = service.devices["m"].session.lock().await;
    let s2 = service.clone();
    let queued = tokio::spawn(async move {
        let slot = s2.job("m-archive-1").unwrap().clone();
        s2.run_once(&slot).await.unwrap().1
    });
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    drop(held);
    match queued.await.unwrap() {
        Outcome::Done(s) => assert_eq!(s.inserted, 1),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_survey_reads_identity_and_archives() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    let device = service.devices.get("m").unwrap().clone();
    {
        let mut session = device.session.lock().await;
        kmb_sync::collect::survey(&device, &mut session, false, &Traffic::default())
            .await
            .unwrap();
        assert!(session.is_some(), "the first run reuses the descriptions");
        drop(session);
    }
    let s = device.snapshot();
    assert!(s.identity.is_some());
    let archives = s.archives.unwrap();
    assert!(archives.iter().any(|a| a.archive == 0 && a.name == "Main"));
    assert!(archives.iter().all(|a| a.synced_at.is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn meter_clock_going_back_is_an_error_not_a_silent_skip() {
    let meter = Arc::new(Mutex::new(Meter::default()));
    meter.lock().unwrap().records = (0..3).map(|i| record(minute(i), 230.0)).collect();
    let port = start_meter(meter.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), port, "")).unwrap();
    assert_eq!(run(&service, "m-archive-1").await.inserted, 3);

    meter
        .lock()
        .unwrap()
        .records
        .push(record(minute(1) + 30_000, 231.0));
    let slot = service.job("m-archive-1").unwrap().clone();
    let (_, outcome) = service.run_once(&slot).await.unwrap();
    let Outcome::Failed(e) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(e.contains("did the meter clock go back"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn control_api_refuses_browsers_and_foreign_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(config(&dir.path().join("t.db"), 1, "")).unwrap();
    let app = kmb_sync::api::router(service, None);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let statuses = tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let post = |header: Option<(&str, &str)>| {
            let mut req = agent.post(format!("{base}/api/jobs/m-archive-1/pause"));
            if let Some((k, v)) = header {
                req = req.header(k, v);
            }
            req.send_empty().unwrap().status()
        };
        [
            post(None),
            post(Some(("Origin", "https://evil.example"))),
            post(Some(("Host", "evil.example:8650"))),
        ]
    })
    .await
    .unwrap();
    assert_eq!(statuses, [200, 403, 403]);
}
