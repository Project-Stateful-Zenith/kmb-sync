use crate::clock;
use crate::config::parse_time;
use crate::config::{Archives, Task};
use crate::device::{ArchiveState, DeviceStatus};
use crate::service::{JobState, Service};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kmb::KmbTime;
use kmb_store::{Definition, RecordQuery};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_RECORDS: usize = 100_000;
pub const RECENT_RUNS: usize = 48;
pub const TRAFFIC_DAYS: i64 = 7;
const STATUS_PAGE: &str = include_str!("status.html");

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn not_found(what: &str, name: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} {name:?} not found"))
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

type ApiResult = Result<Json<Value>, ApiError>;

fn to_json(v: impl Serialize) -> ApiResult {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub fn router(service: Arc<Service>, token: Option<String>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/devices", get(devices))
        .route("/api/devices/{name}/identify", post(identify))
        .route("/api/jobs", get(jobs))
        .route("/api/jobs/{name}/run", post(run_job))
        .route("/api/jobs/{name}/pause", post(pause))
        .route("/api/jobs/{name}/resume", post(resume))
        .route("/api/runs", get(runs))
        .route("/api/measurements", get(measurements))
        .route("/api/records", get(records))
        .route("/api/values", get(values))
        .route("/api/download-states", get(download_states))
        .route("/api/stations", get(stations))
        .layer(middleware::from_fn_with_state(Arc::new(token), auth))
        .with_state(service)
}

async fn auth(State(token): State<Arc<Option<String>>>, req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return ApiError(StatusCode::FORBIDDEN, "browser requests are refused".into())
            .into_response();
    }
    if token.is_none() && !req.headers().get(header::HOST).is_none_or(loopback_host) {
        return ApiError(StatusCode::FORBIDDEN, "host is not a loopback name".into())
            .into_response();
    }
    if let Some(token) = token.as_deref() {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !same(given.as_bytes(), token.as_bytes()) {
            return ApiError(StatusCode::UNAUTHORIZED, "missing or wrong token".into())
                .into_response();
        }
    }
    next.run(req).await
}

fn loopback_host(v: &header::HeaderValue) -> bool {
    let Ok(host) = v.to_str() else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health(State(s): State<Arc<Service>>) -> ApiResult {
    Ok(Json(json!({
        "status": "ok",
        "started_at": clock::rfc3339(s.started_at),
        "devices": s.devices.len(),
        "jobs": s.jobs.len(),
    })))
}

async fn devices(State(s): State<Arc<Service>>) -> ApiResult {
    let mut list: Vec<_> = s.devices.values().map(|d| d.snapshot()).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    to_json(list)
}

pub fn status_router(service: Arc<Service>, api_token: Option<String>) -> Router {
    let router = api_token.map_or_else(Router::new, |token| {
        Router::new()
            .route("/api/stations", get(stations))
            .layer(middleware::from_fn_with_state(Arc::new(Some(token)), auth))
    });
    router.route("/", get(status_page)).with_state(service)
}

async fn status_page(State(s): State<Arc<Service>>) -> Response {
    let data = match overview(&s).await {
        Ok(data) => data,
        Err(ApiError(_, e)) => {
            tracing::error!("status page: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                "status unavailable, see the service log",
            )
                .into_response();
        }
    };
    let data = data.to_string().replace('<', "\\u003c");
    let page = STATUS_PAGE
        .replacen("/*VERSION*/", env!("CARGO_PKG_VERSION"), 1)
        .replacen("/*DATA*/", &data, 1);
    ([(header::CACHE_CONTROL, "no-store")], Html(page)).into_response()
}

#[derive(Serialize)]
struct Station {
    #[serde(flatten)]
    device: DeviceStatus,
    jobs: Vec<StationJob>,
    archives: Vec<StationArchive>,
    archives_known: bool,
    traffic: Vec<kmb_store::DeviceDay>,
}

#[derive(Serialize)]
struct StationArchive {
    #[serde(flatten)]
    state: ArchiveState,
    offered: Option<bool>,
    jobs: Vec<String>,
}

fn station_archives(s: &Service, device: &mut DeviceStatus) -> (Vec<StationArchive>, bool) {
    let selections: Vec<(&str, &Archives)> = s
        .jobs
        .iter()
        .filter(|j| j.job.device == device.name)
        .filter_map(|j| match &j.job.task {
            Task::Archive { archives, .. } => Some((j.job.name.as_str(), archives)),
            _ => None,
        })
        .collect();
    let syncing = |a: &ArchiveState| -> Vec<String> {
        selections
            .iter()
            .filter(|(_, sel)| match sel {
                Archives::All => a.unsupported.is_none(),
                Archives::List(_) => sel.includes(a.archive),
            })
            .map(|(n, _)| (*n).to_string())
            .collect()
    };
    let offered = device.archives.take();
    let known = offered.is_some();
    let mut list: Vec<StationArchive> = offered
        .unwrap_or_default()
        .into_iter()
        .map(|a| StationArchive {
            jobs: syncing(&a),
            offered: Some(true),
            state: a,
        })
        .collect();
    for (_, sel) in &selections {
        let Archives::List(l) = sel else { continue };
        for n in l.iter().filter_map(|a| a.number().ok()) {
            if list.iter().any(|a| a.state.archive == n) {
                continue;
            }
            let state = ArchiveState {
                archive: n,
                name: kmb::archive::uni::archive_name(n).unwrap_or("?").into(),
                ..ArchiveState::default()
            };
            list.push(StationArchive {
                jobs: syncing(&state),
                offered: known.then_some(false),
                state,
            });
        }
    }
    (list, known)
}

#[derive(Serialize)]
struct StationJob {
    #[serde(flatten)]
    state: JobState,
    recent: Vec<kmb_store::Run>,
}

async fn stations(State(s): State<Arc<Service>>) -> ApiResult {
    overview(&s).await.map(Json)
}

async fn overview(s: &Service) -> Result<Value, ApiError> {
    let names: Vec<String> = s.jobs.iter().map(|j| j.job.name.clone()).collect();
    let now = jiff::Zoned::now();
    let first = now
        .date()
        .checked_sub(jiff::Span::new().days(TRAFFIC_DAYS - 1))
        .and_then(|d| d.to_zoned(now.time_zone().clone()))
        .map_or(0, |z| z.timestamp().as_millisecond());
    let (mut recent, days) = s
        .store
        .call(move |st| {
            let recent = names
                .into_iter()
                .map(|n| st.runs(Some(&n), RECENT_RUNS).map(|r| (n, r)))
                .collect::<kmb_store::Result<HashMap<_, _>>>()?;
            Ok((recent, st.device_days(first)?))
        })
        .await?;
    let list: Vec<Station> = s
        .config
        .devices
        .iter()
        .filter_map(|d| s.devices.get(&d.name))
        .map(|d| {
            let mut device = d.snapshot();
            let (archives, archives_known) = station_archives(s, &mut device);
            let jobs = s
                .jobs
                .iter()
                .filter(|j| j.job.device == device.name)
                .map(|j| StationJob {
                    recent: recent.remove(&j.job.name).unwrap_or_default(),
                    state: j.snapshot(),
                })
                .collect();
            let traffic = days
                .iter()
                .filter(|t| t.device == device.name)
                .cloned()
                .collect();
            Station {
                device,
                jobs,
                archives,
                archives_known,
                traffic,
            }
        })
        .collect();
    Ok(json!({
        "now": clock::now_ms(),
        "today": now.date().to_string(),
        "started_at": s.started_at,
        "stations": to_json(list)?.0,
    }))
}

async fn identify(State(s): State<Arc<Service>>, Path(name): Path<String>) -> ApiResult {
    let device = s
        .devices
        .get(&name)
        .ok_or_else(|| not_found("device", &name))?;
    let summary = s.identify(device).await?;
    to_json(summary)
}

async fn jobs(State(s): State<Arc<Service>>) -> ApiResult {
    to_json(s.jobs.iter().map(|j| j.snapshot()).collect::<Vec<_>>())
}

async fn run_job(State(s): State<Arc<Service>>, Path(name): Path<String>) -> ApiResult {
    let job = s.job(&name).ok_or_else(|| not_found("job", &name))?;
    job.trigger.notify_one();
    Ok(Json(json!({ "triggered": name })))
}

async fn pause(State(s): State<Arc<Service>>, Path(name): Path<String>) -> ApiResult {
    let job = s.job(&name).ok_or_else(|| not_found("job", &name))?;
    job.update(|j| j.paused = true);
    Ok(Json(json!({ "paused": name })))
}

async fn resume(State(s): State<Arc<Service>>, Path(name): Path<String>) -> ApiResult {
    let job = s.job(&name).ok_or_else(|| not_found("job", &name))?;
    job.update(|j| j.paused = false);
    Ok(Json(json!({ "resumed": name })))
}

#[derive(Deserialize)]
struct RunsQuery {
    job: Option<String>,
    limit: Option<usize>,
}

async fn runs(State(s): State<Arc<Service>>, Query(q): Query<RunsQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(50).min(1000);
    let list = s
        .store
        .call(move |st| st.runs(q.job.as_deref(), limit))
        .await?;
    to_json(list)
}

async fn measurements(State(s): State<Arc<Service>>) -> ApiResult {
    to_json(s.store.call(|st| st.measurements()).await?)
}

#[derive(Deserialize)]
struct RecordsQuery {
    measurement: i64,
    archive: u8,
    from: Option<String>,
    to: Option<String>,
    limit: Option<usize>,
    order: Option<String>,
    raw: Option<bool>,
}

fn kmb_bound(text: Option<&str>) -> Result<Option<u64>, ApiError> {
    text.map(|t| {
        let ms = clock::unix_ms(parse_time(t).map_err(|e| bad_request(format!("{e:#}")))?);
        Ok(clock::to_kmb(ms).0)
    })
    .transpose()
}

#[derive(Serialize)]
pub struct RecordRow {
    pub time: String,
    pub time_kmb: u64,
    pub slot: u32,
    pub fields: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

pub fn decode_rows(
    store: &kmb_store::Store,
    records: Vec<kmb_store::StoredRecord>,
    raw: bool,
) -> kmb_store::Result<Vec<RecordRow>> {
    let mut defs: HashMap<i64, Definition> = HashMap::new();
    let mut rows = Vec::with_capacity(records.len());
    for r in records {
        let def = match defs.entry(r.definition) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(store.load_definition(r.definition)?)
            }
        };
        let fields = def.decode(&r.bytes).map_or_else(
            |e| json!({ "error": e.to_string() }),
            |f| serde_json::to_value(f).unwrap_or(Value::Null),
        );
        rows.push(RecordRow {
            time: clock::kmb_rfc3339(KmbTime(r.time)),
            time_kmb: r.time,
            slot: r.slot,
            fields,
            raw: raw.then(|| hex(&r.bytes)),
        });
    }
    Ok(rows)
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02X}");
        s
    })
}

async fn records(State(s): State<Arc<Service>>, Query(q): Query<RecordsQuery>) -> ApiResult {
    let query = RecordQuery {
        measurement: q.measurement,
        archive: q.archive,
        from: kmb_bound(q.from.as_deref())?,
        to: kmb_bound(q.to.as_deref())?,
        limit: q.limit.unwrap_or(1000).min(MAX_RECORDS),
        newest_first: match q.order.as_deref() {
            None | Some("asc") => false,
            Some("desc") => true,
            Some(o) => return Err(bad_request(format!("order {o:?} is not asc or desc"))),
        },
    };
    let raw = q.raw.unwrap_or(false);
    let rows = s
        .store
        .call(move |st| {
            let recs = st.records(&query)?;
            decode_rows(st, recs, raw)
        })
        .await?;
    to_json(rows)
}

#[derive(Deserialize)]
struct ValuesQuery {
    measurement: i64,
    limit: Option<usize>,
}

async fn values(State(s): State<Arc<Service>>, Query(q): Query<ValuesQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(1).min(10_000);
    let list = s
        .store
        .call(move |st| st.latest_values(q.measurement, limit))
        .await?;
    to_json(
        list.into_iter()
            .map(|(t, v)| json!({ "time": clock::kmb_rfc3339(KmbTime(t)), "values": v }))
            .collect::<Vec<_>>(),
    )
}

#[derive(Deserialize)]
struct StatesQuery {
    measurement: i64,
    archive: u8,
    limit: Option<usize>,
}

async fn download_states(State(s): State<Arc<Service>>, Query(q): Query<StatesQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(100).min(10_000);
    to_json(
        s.store
            .call(move |st| st.download_states(q.measurement, q.archive, limit))
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::same;
    use axum::http::header;

    #[test]
    fn loopback_hosts() {
        let ok = |h: &str| super::loopback_host(&header::HeaderValue::from_str(h).unwrap());
        assert!(
            ok("127.0.0.1:8650") && ok("localhost:8650") && ok("[::1]:8650") && ok("127.0.0.1")
        );
        assert!(
            !ok("evil.example:8650") && !ok("192.168.1.2:8650") && !ok("localhost.evil.example")
        );
    }

    #[test]
    fn token_compare() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
