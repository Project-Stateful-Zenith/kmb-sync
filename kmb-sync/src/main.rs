use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kmb_store::{RecordQuery, Store};
use kmb_sync::api::{RecordRow, decode_rows};
use kmb_sync::clock;
use kmb_sync::config::{Config, parse_time, read_secret};
use kmb_sync::service::{Outcome, Service, stop_on_signal};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Collector service for KMB/KMP meters")]
struct Cli {
    #[arg(
        long,
        env = "KMB_SYNC_API",
        default_value = "http://127.0.0.1:8650",
        global = true
    )]
    api: String,
    #[arg(long, env = "KMB_SYNC_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(short, long)]
        config: PathBuf,
    },
    Check {
        #[arg(short, long)]
        config: PathBuf,
    },
    Once {
        #[arg(short, long)]
        config: PathBuf,
        job: String,
    },
    Export {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        measurement: i64,
        #[arg(long)]
        archive: u8,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value_t = usize::MAX)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Format::Jsonl)]
        format: Format,
    },
    Archives {
        #[arg(short, long)]
        config: PathBuf,
        device: String,
    },
    Traffic {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        job: Option<String>,
    },
    Status,
    Devices,
    Jobs,
    Runs {
        #[arg(long)]
        job: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Measurements,
    Run {
        job: String,
    },
    Pause {
        job: String,
    },
    Resume {
        job: String,
    },
    Identify {
        device: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Jsonl,
    Csv,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let client = ApiClient {
        base: cli.api.trim_end_matches('/').to_string(),
        token: cli.token_file.as_deref().map(read_secret).transpose()?,
    };
    match cli.command {
        Command::Serve { config } => serve(&config),
        Command::Check { config } => check(&config),
        Command::Once { config, job } => once(&config, &job),
        Command::Archives { config, device } => list_archives(&config, &device),
        Command::Traffic { config, since, job } => {
            traffic(&config, since.as_deref(), job.as_deref())
        }
        Command::Export {
            db,
            measurement,
            archive,
            from,
            to,
            limit,
            format,
        } => {
            let bound = |t: Option<String>| -> Result<Option<u64>> {
                t.map(|t| Ok(clock::to_kmb(clock::unix_ms(parse_time(&t)?)).0))
                    .transpose()
            };
            let query = RecordQuery {
                measurement,
                archive,
                from: bound(from)?,
                to: bound(to)?,
                limit,
                newest_first: false,
            };
            export(&db, &query, format)
        }
        Command::Devices => client.print("GET", "/api/devices"),
        Command::Jobs => client.print("GET", "/api/jobs"),
        Command::Status => client.status(),
        Command::Runs { job, limit } => {
            let job = job.map(|j| format!("&job={j}")).unwrap_or_default();
            client.print("GET", &format!("/api/runs?limit={limit}{job}"))
        }
        Command::Measurements => client.print("GET", "/api/measurements"),
        Command::Run { job } => client.print("POST", &format!("/api/jobs/{job}/run")),
        Command::Pause { job } => client.print("POST", &format!("/api/jobs/{job}/pause")),
        Command::Resume { job } => client.print("POST", &format!("/api/jobs/{job}/resume")),
        Command::Identify { device } => {
            client.print("POST", &format!("/api/devices/{device}/identify"))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn serve(path: &std::path::Path) -> Result<()> {
    let config = Config::load(path)?;
    runtime()?.block_on(async {
        let service = Service::new(config)?;
        let cancel = service.cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = stop_on_signal(cancel.clone()).await {
                tracing::error!("signal handling failed: {e:#}");
                cancel.cancel();
            }
        });
        service.run().await
    })
}

fn check(path: &std::path::Path) -> Result<()> {
    let config = Config::load(path)?;
    // the service reads these at start; fail here instead, without showing them
    for d in &config.devices {
        kmb_sync::device::Secrets::load(d).with_context(|| format!("device {:?}", d.name))?;
    }
    if let Some(e) = &config.envis {
        read_secret(&e.password_file).context("envis.password_file")?;
    }
    let service = &config.service;
    for file in [&service.api_token_file, &service.status_api_token_file]
        .into_iter()
        .flatten()
    {
        kmb_sync::config::read_token(file)?;
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "database {}", config.service.database.display())?;
    match config.service.listen {
        Some(addr) => writeln!(out, "API on {addr}")?,
        None => writeln!(out, "API off")?,
    }
    match config.service.status_listen {
        Some(addr) => {
            let api = if config.service.status_api {
                ", /api/stations with token"
            } else {
                ""
            };
            writeln!(out, "status page on http://{addr}/{api}")?;
        }
        None => writeln!(out, "status page off")?,
    }
    match config.service.run_log_rotate {
        Some(d) => writeln!(
            out,
            "run log rotates every {}, keeping the current and the previous period",
            days_text(d)
        )?,
        None => writeln!(out, "run log kept forever")?,
    }
    for d in &config.devices {
        writeln!(
            out,
            "device {} at {} path {:?}",
            d.name,
            d.url(),
            d.object_path()
        )?;
    }
    for j in &config.jobs {
        let state = if j.enabled { "" } else { " (disabled)" };
        writeln!(
            out,
            "job {} on {} every {}{state}: {:?}",
            j.name,
            j.device,
            humantime::format_duration(j.every),
            j.task
        )?;
    }
    for f in kmb_sync::config::forecast(&config) {
        writeln!(out, "{}", forecast_text(&f))?;
    }
    writeln!(out, "secret files readable")?;
    for w in config.warnings() {
        writeln!(out, "warning: {w}")?;
    }
    Ok(())
}

/// Whole days as `365d`; humantime would write `11months 30days 3h 50m 24s`.
fn days_text(d: std::time::Duration) -> String {
    const DAY: u64 = 86_400;
    let secs = d.as_secs();
    if d.subsec_nanos() == 0 && secs.is_multiple_of(DAY) {
        format!("{}d", secs / DAY)
    } else {
        humantime::format_duration(d).to_string()
    }
}

fn forecast_text(f: &kmb_sync::config::LinkForecast) -> String {
    let (whole, tenths) = (f.hold_permille / 10, f.hold_permille % 10);
    let queue = if f.hold_permille > 1000 {
        " -- more than there is: runs will queue and start late"
    } else {
        ""
    };
    let bytes = f.archive_bytes_per_day.map_or_else(
        || format!("not limited (no max_size on {})", f.unlimited.join(", ")),
        |b| format!("at most {} per day", mib(b)),
    );
    let other = if f.other_runs_per_day > 0 {
        format!(
            "; plus {} values and clock runs per day",
            f.other_runs_per_day
        )
    } else {
        String::new()
    };
    format!(
        "device {}: jobs may hold the link {whole}.{tenths}% of the time{queue}; archive downloads {bytes}{other}",
        f.device
    )
}

fn mib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / (1 << 20);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

fn list_archives(path: &std::path::Path, device: &str) -> Result<()> {
    use kmb::archive::uni::{Layout, archive_name};
    use kmb_sync::config::Task;
    use kmb_sync::device::{Described, DeviceSlot, Secrets, Traffic};
    let config = Config::load(path)?;
    let d = config
        .device(device)
        .with_context(|| format!("no device {device:?}"))?
        .clone();
    let secrets = Secrets::load(&d)?;
    let envis = config.envis.is_some();
    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.device == device && j.enabled)
        .filter_map(|j| match &j.task {
            Task::Archive { archives, .. } => Some((j.name.clone(), archives.clone())),
            _ => None,
        })
        .collect();
    runtime()?.block_on(async {
        let slot = DeviceSlot::new(d, secrets);
        let mut client = slot.connect(&Traffic::default()).await?;
        let id = client.identify().await.context("identify")?;
        let cfg = client
            .read_description(kmb::uni::DataSet::Config)
            .await
            .context("reading Config")?;
        let desc = client
            .read_archive_description()
            .await
            .context("reading archive descriptors")?;
        let described = Described {
            identify: id.clone(),
            config: cfg,
            archives: None,
            layouts: std::collections::HashMap::new(),
            sets: std::collections::HashMap::new(),
            at: std::time::Instant::now(),
        };
        let (version, max_size) = described.header(&desc);
        let mut out = std::io::stdout().lock();
        writeln!(
            out,
            "{device}: meter {} firmware {}, archives in ENVIS's order",
            id.device_no,
            id.firmware_version()
        )?;
        writeln!(
            out,
            " no  name             record    period          records  {:<52} synced by",
            "stored by kmb-sync"
        )?;
        for o in kmb_sync::envis::offered(&desc) {
            let known = archive_name(o.archive) == Some(o.name.as_str());
            let layout = known.then(|| Layout::new(&desc, o.archive, version, max_size));
            let record = match &layout {
                Some(Ok(l)) => format!("{} B", l.record_len),
                _ => "-".into(),
            };
            let period = period_text(kmb_sync::envis::period(&described.config, o.ic));
            let records = if known {
                client.probe_archive(o.archive).await.map_or_else(
                    |e| format!("probe failed: {e}"),
                    |c| format!("{} / {}", c.len(), c.max_rec),
                )
            } else {
                "-".into()
            };
            let storable = storable_text(known, layout.as_ref(), envis, o.archive);
            let synced: Vec<&str> = jobs
                .iter()
                .filter(|(_, a)| {
                    a.includes(o.archive)
                        && (storable == "yes" || !matches!(a, kmb_sync::config::Archives::All))
                })
                .map(|(n, _)| n.as_str())
                .collect();
            let synced = if synced.is_empty() {
                "-".to_string()
            } else {
                synced.join(", ")
            };
            writeln!(
                out,
                "{:>3}  {:<14} {record:>8} {period:>9} {records:>16}  {storable:<52} {synced}",
                o.archive, o.name
            )?;
        }
        Ok(())
    })
}

fn period_text(ms: u64) -> String {
    match ms {
        0 => "events".to_string(),
        ms if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        ms => format!("{} s", ms / 1000),
    }
}

fn storable_text(
    known: bool,
    layout: Option<&std::result::Result<kmb::archive::uni::Layout, kmb::DecodeError>>,
    envis: bool,
    archive: u8,
) -> String {
    if !known {
        return "no: unknown archive".into();
    }
    if let Some(Err(e)) = layout {
        return format!("no: {e}");
    }
    envis
        .then(|| kmb_sync::envis::envis_unsupported(archive))
        .flatten()
        .map_or_else(|| "yes".into(), |why| format!("no: {why}"))
}

fn traffic(path: &std::path::Path, since: Option<&str>, job: Option<&str>) -> Result<()> {
    let config = Config::load(path)?;
    let since = since
        .map(|s| parse_time(s).map(clock::unix_ms))
        .transpose()?;
    let store = kmb_store::Store::open(&config.service.database)
        .with_context(|| format!("opening {}", config.service.database.display()))?;
    let days = store.traffic(job, since)?;
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{:<10}  {:<24} {:>6} {:>12} {:>12} {:>9}",
        "day (UTC)", "job", "runs", "received", "sent", "records"
    )?;
    let (mut runs, mut rx, mut tx, mut rows) = (0_i64, 0_i64, 0_i64, 0_i64);
    for d in &days {
        writeln!(
            out,
            "{:<10}  {:<24} {:>6} {:>12} {:>12} {:>9}",
            d.day,
            d.job,
            d.runs,
            bytes(d.bytes_rx),
            bytes(d.bytes_tx),
            d.inserted
        )?;
        runs = runs.saturating_add(d.runs);
        rx = rx.saturating_add(d.bytes_rx);
        tx = tx.saturating_add(d.bytes_tx);
        rows = rows.saturating_add(d.inserted);
    }
    writeln!(
        out,
        "{:<10}  {:<24} {runs:>6} {:>12} {:>12} {rows:>9}",
        "total",
        "",
        bytes(rx),
        bytes(tx)
    )?;
    writeln!(
        out,
        "link total (received + sent): {}",
        bytes(rx.saturating_add(tx))
    )?;
    Ok(())
}

fn local_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(
        |_| "-".to_string(),
        |t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M:%S")
                .to_string()
        },
    )
}

fn bytes(n: i64) -> String {
    let n = u64::try_from(n).unwrap_or(0);
    for (unit, shift) in [("GiB", 30_u32), ("MiB", 20), ("KiB", 10)] {
        if n >= 1 << shift {
            let tenths = n.saturating_mul(10) >> shift;
            return format!("{}.{} {unit}", tenths / 10, tenths % 10);
        }
    }
    format!("{n} B")
}

fn once(path: &std::path::Path, job: &str) -> Result<()> {
    let config = Config::load(path)?;
    runtime()?.block_on(async {
        let service = Service::new(config)?;
        let slot = service
            .job(job)
            .with_context(|| format!("no job {job:?}"))?
            .clone();
        let (run, outcome) = service.run_once(&slot).await?;
        let device = service.devices.get(&slot.job.device).map(|d| d.snapshot());
        let body = serde_json::json!({
            "run": run,
            "outcome": outcome.name(),
            "summary": match &outcome { Outcome::Done(s) => serde_json::to_value(s)?, _ => Value::Null },
            "error": match &outcome { Outcome::Failed(e) => Value::from(e.as_str()), _ => Value::Null },
            "device": device,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
        if matches!(outcome, Outcome::Done(_)) {
            Ok(())
        } else {
            bail!("job {job} did not complete: {}", outcome.name())
        }
    })
}

fn export(db: &std::path::Path, query: &RecordQuery, format: Format) -> Result<()> {
    let store = Store::open(db)?;
    let rows = decode_rows(&store, store.records(query)?, false)?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    match format {
        Format::Jsonl => {
            for r in &rows {
                serde_json::to_writer(&mut out, r)?;
                writeln!(out)?;
            }
        }
        Format::Csv => write_csv(&mut out, &rows)?,
    }
    out.flush()?;
    Ok(())
}

fn flatten(row: &RecordRow) -> Vec<(String, String)> {
    let mut cells = Vec::new();
    if let Value::Array(fields) = &row.fields {
        for f in fields {
            let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
            let stat = f.get("stat").and_then(Value::as_str);
            let unit = f.get("unit").and_then(Value::as_str).unwrap_or_default();
            let mut col = stat.map_or_else(|| name.to_string(), |s| format!("{name}_{s}"));
            if !unit.is_empty() {
                col = format!("{col} [{unit}]");
            }
            match f.get("value") {
                Some(Value::Array(items)) => {
                    for (i, v) in items.iter().enumerate() {
                        cells.push((format!("{col}[{i}]"), cell(v)));
                    }
                }
                Some(v) => cells.push((col, cell(v))),
                None => cells.push((col, String::new())),
            }
        }
    } else {
        cells.push(("error".into(), row.fields.to_string()));
    }
    cells
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => quote(s),
        other => other.to_string(),
    }
}

fn quote(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn write_csv(out: &mut impl Write, rows: &[RecordRow]) -> Result<()> {
    let mut header: Option<Vec<String>> = None;
    for r in rows {
        let cells = flatten(r);
        let names: Vec<String> = cells.iter().map(|(n, _)| quote(n)).collect();
        if header.as_ref() != Some(&names) {
            writeln!(out, "time,slot,{}", names.join(","))?;
            header = Some(names);
        }
        let values: Vec<&str> = cells.iter().map(|(_, v)| v.as_str()).collect();
        writeln!(out, "{},{},{}", r.time, r.slot, values.join(","))?;
    }
    Ok(())
}

struct ApiClient {
    base: String,
    token: Option<String>,
}

impl ApiClient {
    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut req = ureq::get(&url);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut response = req.call().with_context(|| format!("GET {url}"))?;
        let body = response.body_mut().read_to_string()?;
        serde_json::from_str(&body).with_context(|| format!("GET {url}: not JSON"))
    }

    fn status(&self) -> Result<()> {
        let jobs = self.get("/api/jobs")?;
        let devices = self.get("/api/devices")?;
        let mut out = std::io::stdout().lock();
        let time = |v: &Value| v.as_i64().map_or_else(|| "-".to_string(), local_time);
        writeln!(
            out,
            "{:<20} {:<10} {:<8} {:<19} {:<8} {:>7} {:>10} {:<19}",
            "job", "device", "state", "last run", "outcome", "rows", "received", "next run"
        )?;
        for j in jobs.as_array().into_iter().flatten() {
            let state = if !j["enabled"].as_bool().unwrap_or(false) {
                "disabled"
            } else if j["running"].as_bool().unwrap_or(false) {
                "running"
            } else if j["paused"].as_bool().unwrap_or(false) {
                "paused"
            } else {
                "waiting"
            };
            let rows = j
                .pointer("/last_summary/inserted")
                .and_then(Value::as_i64)
                .map_or_else(|| "-".to_string(), |n| n.to_string());
            let rx = j
                .pointer("/last_traffic/rx")
                .and_then(Value::as_i64)
                .map_or_else(|| "-".to_string(), bytes);
            writeln!(
                out,
                "{:<20} {:<10} {:<8} {:<19} {:<8} {:>7} {:>10} {:<19}",
                j["name"].as_str().unwrap_or("?"),
                j["device"].as_str().unwrap_or("?"),
                state,
                time(&j["last_started"]),
                j["last_outcome"].as_str().unwrap_or("-"),
                rows,
                rx,
                time(&j["next_run"]),
            )?;
            if let Some(e) = j["last_error"].as_str() {
                let failures = j["failures"].as_i64().unwrap_or(0);
                writeln!(out, "    {failures} failure(s) in a row, last: {e}")?;
            }
        }
        writeln!(out)?;
        for d in devices.as_array().into_iter().flatten() {
            let id = &d["identity"];
            let meter = id["device_no"]
                .as_u64()
                .map_or_else(|| "?".to_string(), |n| n.to_string());
            writeln!(
                out,
                "device {:<10} {:<40} meter {meter} fw {}  last contact {}{}",
                d["name"].as_str().unwrap_or("?"),
                d["url"].as_str().unwrap_or("?"),
                id["firmware"].as_str().unwrap_or("?"),
                time(&d["last_ok"]),
                d["last_error"]
                    .as_str()
                    .map(|e| format!("  last error: {e}"))
                    .unwrap_or_default(),
            )?;
        }
        Ok(())
    }

    fn print(&self, method: &str, path: &str) -> Result<()> {
        let url = format!("{}{path}", self.base);
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        let response = if method == "POST" {
            let mut req = ureq::post(&url);
            if let Some(a) = &auth {
                req = req.header("Authorization", a);
            }
            req.config()
                .http_status_as_error(false)
                .build()
                .send_empty()
        } else {
            let mut req = ureq::get(&url);
            if let Some(a) = &auth {
                req = req.header("Authorization", a);
            }
            req.config().http_status_as_error(false).build().call()
        };
        let mut response = response.with_context(|| format!("{method} {url}"))?;
        let status = response.status();
        let body = response.body_mut().read_to_string()?;
        let pretty = serde_json::from_str::<Value>(&body)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or(body);
        println!("{pretty}");
        if !status.is_success() {
            bail!("{method} {path}: {status}");
        }
        Ok(())
    }
}
