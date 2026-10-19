use anyhow::{Context, Result, bail};
use kmb::archive::uni::archive_name;
use kmb::uni::DataSet;
use serde::Deserialize;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub service: Service,
    #[serde(default, rename = "device")]
    pub devices: Vec<Device>,
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub envis: Option<EnvisDb>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvisDb {
    pub host: String,
    #[serde(default = "default_sql_port")]
    pub port: u16,
    #[serde(default = "default_envis_database")]
    pub database: String,
    pub user: String,
    pub password_file: PathBuf,
    #[serde(default)]
    pub trust_cert: bool,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub allow_new_definitions: bool,
}

const fn default_sql_port() -> u16 {
    1433
}

fn default_envis_database() -> String {
    "pqplusdb".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub database: PathBuf,
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    #[serde(default)]
    pub api_token_file: Option<PathBuf>,
    #[serde(default = "default_status_listen", deserialize_with = "listen_or_off")]
    pub status_listen: Option<SocketAddr>,
    #[serde(default)]
    pub status_api: bool,
    #[serde(default)]
    pub status_api_token_file: Option<PathBuf>,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_heartbeat", with = "humantime_serde")]
    pub heartbeat: Duration,
    #[serde(default = "default_describe_every", with = "humantime_serde")]
    pub describe_every: Duration,
    #[serde(default, with = "humantime_serde::option")]
    pub run_log_rotate: Option<Duration>,
    /// Whether the file sets `status_listen`; filled in by [`Config::load`].
    #[serde(skip)]
    pub status_listen_set: bool,
}

/// Shortest `run_log_rotate` accepted.
pub const MIN_RUN_LOG_ROTATE: Duration = Duration::from_secs(24 * 3600);

pub const DEFAULT_STATUS_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8651);

#[allow(clippy::unnecessary_wraps)]
const fn default_status_listen() -> Option<SocketAddr> {
    Some(DEFAULT_STATUS_LISTEN)
}

fn listen_or_off<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SocketAddr>, D::Error> {
    let text = String::deserialize(d)?;
    if text == "off" {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|e| serde::de::Error::custom(format!("{text:?}: {e}; an address or \"off\"")))
}

const fn default_concurrency() -> usize {
    4
}

const fn default_heartbeat() -> Duration {
    Duration::from_secs(300)
}

const fn default_describe_every() -> Duration {
    Duration::from_secs(6 * 3600)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_address")]
    pub address: u8,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub tls_root: Option<String>,
    #[serde(default)]
    pub tls_pins: Vec<String>,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub credentials_file: Option<PathBuf>,
    #[serde(default)]
    pub psk_file: Option<PathBuf>,
    #[serde(default = "default_connect_timeout", with = "humantime_serde")]
    pub connect_timeout: Duration,
    #[serde(default = "default_byte_timeout", with = "humantime_serde")]
    pub byte_timeout: Duration,
    #[serde(default = "default_packed")]
    pub packed: u32,
    #[serde(default)]
    pub envis_url: Option<String>,
}

const fn default_packed() -> u32 {
    6800
}

const fn default_address() -> u8 {
    1
}

const fn default_connect_timeout() -> Duration {
    kmb_tokio::CONNECT_TIMEOUT
}

const fn default_byte_timeout() -> Duration {
    kmb_tokio::BYTE_TIMEOUT
}

impl Device {
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(if self.tls {
            kmb_tokio::TLS_PORT
        } else {
            kmb_tokio::PORT
        })
    }

    #[must_use]
    pub fn url(&self) -> String {
        let scheme = if self.tls { "tls" } else { "tcp" };
        format!("{scheme}://{}:{}/{}", self.host, self.port(), self.address)
    }

    #[must_use]
    pub fn envis_url(&self) -> String {
        self.envis_url.clone().unwrap_or_else(|| {
            format!(
                "-PACKED:{}$-MODBUS:AUTO$tcp://{}:{}/{}/10000/10000",
                self.packed,
                self.host,
                self.port(),
                self.address
            )
        })
    }

    #[must_use]
    pub fn object_path(&self) -> Vec<String> {
        if self.path.is_empty() {
            vec![self.name.clone()]
        } else {
            self.path.clone()
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ArchiveRef {
    Number(u8),
    Name(String),
}

impl ArchiveRef {
    pub fn number(&self) -> Result<u8> {
        match self {
            Self::Number(n) => Ok(*n),
            Self::Name(name) => (0..=u8::MAX)
                .find(|&t| archive_name(t).is_some_and(|n| n.eq_ignore_ascii_case(name)))
                .with_context(|| format!("unknown archive name {name:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Archives {
    All,
    List(Vec<ArchiveRef>),
}

impl<'de> Deserialize<'de> for Archives {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            List(Vec<ArchiveRef>),
        }
        match Raw::deserialize(d)? {
            Raw::Word(w) if w == "all" => Ok(Self::All),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "archives: {w:?} is neither \"all\" nor a list"
            ))),
            Raw::List(l) => Ok(Self::List(l)),
        }
    }
}

impl Archives {
    #[must_use]
    pub fn includes(&self, n: u8) -> bool {
        match self {
            Self::All => true,
            Self::List(l) => l.iter().any(|a| a.number().ok() == Some(n)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFrom {
    All,
    Time(OffsetDateTime),
}

impl<'de> Deserialize<'de> for StartFrom {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "all" {
            return Ok(Self::All);
        }
        parse_time(&s)
            .map(Self::Time)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Task {
    Archive {
        archives: Archives,
        #[serde(default = "default_start")]
        start_from: StartFrom,
        #[serde(default)]
        max_mib: u32,
        #[serde(default)]
        max_size: Option<ByteSize>,
    },
    Values {
        set: String,
    },
    Clock {
        #[serde(default, with = "humantime_serde::option")]
        adjust_above: Option<Duration>,
    },
}

const fn default_start() -> StartFrom {
    StartFrom::All
}

impl Task {
    #[must_use]
    pub fn byte_limit(&self) -> u64 {
        match self {
            Self::Archive {
                max_mib, max_size, ..
            } => max_size.map_or_else(|| u64::from(*max_mib).saturating_mul(1 << 20), |s| s.0),
            Self::Values { .. } | Self::Clock { .. } => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl std::str::FromStr for ByteSize {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (digits, unit) = s.split_at(split);
        let n: u64 = digits
            .parse()
            .with_context(|| format!("{s:?} is not a byte size"))?;
        let factor: u64 = match unit.trim() {
            "" | "B" => 1,
            "kB" | "KB" => 1_000,
            "KiB" => 1 << 10,
            "MB" => 1_000_000,
            "MiB" => 1 << 20,
            "GB" => 1_000_000_000,
            "GiB" => 1 << 30,
            u => bail!("{s:?}: unknown unit {u:?}"),
        };
        n.checked_mul(factor)
            .map(Self)
            .with_context(|| format!("{s:?} is too large"))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bytes(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Bytes(n) => Ok(Self(n)),
            Raw::Text(t) => t.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub name: String,
    pub device: String,
    #[serde(default = "default_every", with = "humantime_serde")]
    pub every: Duration,
    #[serde(default, deserialize_with = "opt_time")]
    pub anchor: Option<OffsetDateTime>,
    #[serde(default, deserialize_with = "opt_time_of_day")]
    pub start_time: Option<jiff::civil::Time>,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default = "default_measurement")]
    pub measurement: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(flatten)]
    pub task: Task,
}

const fn default_every() -> Duration {
    Duration::from_secs(60)
}

const fn default_timeout() -> Duration {
    Duration::from_secs(600)
}

fn default_measurement() -> String {
    "default".into()
}

const fn yes() -> bool {
    true
}

fn opt_time<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<OffsetDateTime>, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    s.map(|s| parse_time(&s).map_err(serde::de::Error::custom))
        .transpose()
}

fn opt_time_of_day<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<jiff::civil::Time>, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    s.map(|s| {
        s.parse::<jiff::civil::Time>()
            .map_err(|e| serde::de::Error::custom(format!("start_time {s:?}: {e}")))
    })
    .transpose()
}

pub fn parse_time(s: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .with_context(|| format!("{s:?} is not an RFC 3339 time"))
}

pub fn data_set(name: &str) -> Result<DataSet> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "actdata" => DataSet::ActData,
        "avgdata" => DataSet::AvgData,
        "maxdata" => DataSet::MaxData,
        "mindata" => DataSet::MinData,
        "edata" => DataSet::Edata,
        "status" => DataSet::Status,
        _ => bail!("unknown value set {name:?}"),
    })
}

pub const MIN_VALUES_PERIOD: Duration = Duration::from_secs(5);

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut c: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        c.service.status_listen_set = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("service")?.get("status_listen").cloned())
            .is_some();
        c.check()?;
        Ok(c)
    }

    fn check_service(&self) -> Result<()> {
        if self.service.max_concurrency == 0 {
            bail!("service.max_concurrency must be at least 1");
        }
        if self
            .service
            .run_log_rotate
            .is_some_and(|d| d < MIN_RUN_LOG_ROTATE)
        {
            bail!("service.run_log_rotate must be at least 1d");
        }
        if let Some(zone) = self.envis.as_ref().and_then(|e| e.time_zone.as_deref()) {
            kmb_envis::pack::Calendar::new(zone)
                .with_context(|| format!("envis.time_zone {zone:?}"))?;
        }
        if let Some(addr) = self.service.listen
            && !addr.ip().is_loopback()
            && self.service.api_token_file.is_none()
        {
            bail!("service.listen {addr} is not loopback: set service.api_token_file");
        }
        if self.service.status_api && self.service.status_api_token_file.is_none() {
            bail!("service.status_api needs service.status_api_token_file");
        }
        if self.service.status_api && self.service.status_listen.is_none() {
            bail!("service.status_api needs service.status_listen");
        }
        if self.service.status_api_token_file.is_some() && !self.service.status_api {
            bail!("service.status_api_token_file is set but status_api is not true");
        }
        if let (Some(api), Some(page)) = (self.service.listen, self.service.status_listen)
            && api.port() == page.port()
            && (api.ip() == page.ip() || api.ip().is_unspecified() || page.ip().is_unspecified())
        {
            bail!("service.listen {api} and service.status_listen {page} use the same port");
        }
        Ok(())
    }

    pub fn check(&mut self) -> Result<()> {
        self.check_service()?;
        let mut names = HashSet::new();
        for d in &self.devices {
            check_name("device", &d.name)?;
            if !names.insert(d.name.as_str()) {
                bail!("device {:?} is defined twice", d.name);
            }
            if d.tls && !cfg!(feature = "tls") {
                bail!("device {:?} uses TLS: build with --features tls", d.name);
            }
            if d.tls && d.tls_root.is_none() && d.tls_pins.is_empty() {
                bail!("device {:?} uses TLS: set tls_root or tls_pins", d.name);
            }
            if let Some(root) = &d.tls_root {
                parse_root(root).with_context(|| format!("device {:?}", d.name))?;
            }
            for pin in &d.tls_pins {
                parse_fingerprint(pin).with_context(|| format!("device {:?}", d.name))?;
            }
        }
        let mut counts = std::collections::HashMap::<String, usize>::new();
        let mut jobs = HashSet::new();
        for j in &mut self.jobs {
            if !names.contains(j.device.as_str()) {
                bail!("job for unknown device {:?}", j.device);
            }
            if j.every.is_zero() {
                bail!("job {:?}: every must be above 0", j.name);
            }
            if j.timeout.is_zero() {
                bail!("job {:?}: timeout must be above 0", j.name);
            }
            if j.start_time.is_some() && j.every > Duration::from_secs(86_400) {
                bail!("job {:?}: start_time needs every of at most 24h", j.name);
            }
            if j.start_time.is_some() && j.anchor.is_some() {
                bail!("job {:?}: set start_time or anchor, not both", j.name);
            }
            if j.name.is_empty() {
                let kind = match j.task {
                    Task::Archive { .. } => "archive",
                    Task::Values { .. } => "values",
                    Task::Clock { .. } => "clock",
                };
                let n = counts.entry(format!("{}-{kind}", j.device)).or_default();
                *n = n.saturating_add(1);
                j.name = format!("{}-{kind}-{n}", j.device);
            }
            match &j.task {
                Task::Archive {
                    archives,
                    max_mib,
                    max_size,
                    ..
                } => {
                    if *max_mib > 0 && max_size.is_some() {
                        bail!("job {:?}: set max_mib or max_size, not both", j.name);
                    }
                    let list = match archives {
                        Archives::All => &[][..],
                        Archives::List(l) if l.is_empty() => {
                            bail!("job {:?} names no archives", j.name);
                        }
                        Archives::List(l) => l,
                    };
                    for a in list {
                        a.number().with_context(|| format!("job {:?}", j.name))?;
                    }
                }
                Task::Values { set } => {
                    data_set(set).with_context(|| format!("job {:?}", j.name))?;
                    if j.every < MIN_VALUES_PERIOD {
                        bail!("job {:?}: values jobs run at most every 5 s", j.name);
                    }
                }
                Task::Clock { .. } => {}
            }
            check_name("job", &j.name)?;
            if !jobs.insert(j.name.clone()) {
                bail!("job {:?} is defined twice", j.name);
            }
        }
        Ok(())
    }

    /// Settings that work but are likely not what was meant.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(addr) = self.service.status_listen
            && !self.service.status_listen_set
            && !addr.ip().is_loopback()
        {
            out.push(format!(
                "the status page listens on {addr} by default, open to the network without \
                 a password; set service.status_listen to choose"
            ));
        }
        for j in &self.jobs {
            if j.start_time.is_some() && !86_400_000_u128.is_multiple_of(j.every.as_millis().max(1))
            {
                out.push(format!(
                    "job {:?}: every does not divide 24h, so runs after start_time are not \
                     evenly spaced across midnight",
                    j.name
                ));
            }
        }
        out
    }

    #[must_use]
    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkForecast {
    pub device: String,
    pub hold_permille: u64,
    pub archive_bytes_per_day: Option<u64>,
    pub unlimited: Vec<String>,
    pub other_runs_per_day: u64,
}

const DAY_MS: u128 = 86_400_000;

#[must_use]
pub fn forecast(config: &Config) -> Vec<LinkForecast> {
    let mut out = Vec::new();
    for d in &config.devices {
        let jobs: Vec<&Job> = config
            .jobs
            .iter()
            .filter(|j| j.enabled && j.device == d.name)
            .collect();
        if jobs.is_empty() {
            continue;
        }
        let mut f = LinkForecast {
            device: d.name.clone(),
            hold_permille: 0,
            archive_bytes_per_day: Some(0),
            unlimited: Vec::new(),
            other_runs_per_day: 0,
        };
        for j in jobs {
            let every = j.every.as_millis().max(1);
            let hold = j
                .timeout
                .as_millis()
                .min(every)
                .saturating_mul(1000)
                .checked_div(every)
                .unwrap_or(0);
            let runs = u64::try_from(DAY_MS.div_ceil(every)).unwrap_or(u64::MAX);
            f.hold_permille = f
                .hold_permille
                .saturating_add(u64::try_from(hold).unwrap_or(u64::MAX));
            match &j.task {
                Task::Archive { .. } => match j.task.byte_limit() {
                    0 => {
                        f.archive_bytes_per_day = None;
                        f.unlimited.push(j.name.clone());
                    }
                    limit => {
                        f.archive_bytes_per_day = f
                            .archive_bytes_per_day
                            .map(|b| b.saturating_add(limit.saturating_mul(runs)));
                    }
                },
                Task::Values { .. } | Task::Clock { .. } => {
                    f.other_runs_per_day = f.other_runs_per_day.saturating_add(runs);
                }
            }
        }
        out.push(f);
    }
    out
}

fn parse_hex<const N: usize>(what: &str, s: &str) -> Result<[u8; N]> {
    let digits: Vec<u8> = s.bytes().filter(|&c| c != b':').collect();
    if Some(digits.len()) != N.checked_mul(2) {
        bail!("{what} {s:?} is not {N} bytes of hex");
    }
    let mut out = [0; N];
    for (b, pair) in out.iter_mut().zip(digits.chunks(2)) {
        let text = std::str::from_utf8(pair).ok();
        *b = text
            .and_then(|t| u8::from_str_radix(t, 16).ok())
            .with_context(|| format!("{what} {s:?} is not hex"))?;
    }
    Ok(out)
}

pub fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    parse_hex("fingerprint", s)
}

pub fn parse_root(s: &str) -> Result<([u8; 32], [u8; 32])> {
    let what = "tls_root (an uncompressed P-256 point)";
    let point: [u8; 65] = parse_hex(what, s)?;
    match point.split_first() {
        Some((4, xy)) => {
            let (x, y) = xy.split_at(32);
            Ok((x.try_into()?, y.try_into()?))
        }
        _ => bail!("{what} {s:?} does not start with 04"),
    }
}

pub fn read_secret(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret file {}", path.display()))?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("{what} name {name:?}: use letters, digits, '-', '_' and '.' only");
    }
    Ok(())
}

pub fn read_token(path: &Path) -> Result<String> {
    let t = read_secret(path)?;
    if t.trim().is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(t)
}

pub fn read_credentials(path: &Path) -> Result<(String, String)> {
    let s = read_secret(path)?;
    let mut lines = s.lines();
    let user = lines.next().unwrap_or_default().to_string();
    let password = lines.next().unwrap_or_default().to_string();
    if user.is_empty() {
        bail!("credentials file {} has no user line", path.display());
    }
    Ok((user, password))
}

mod humantime_serde {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }

    pub mod option {
        use super::{Deserialize, Deserializer, Duration};

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
            let s = Option::<String>::deserialize(d)?;
            s.map(|s| humantime::parse_duration(&s).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[service]
database = "/var/lib/kmb-sync/sync.db"
listen = "127.0.0.1:8650"

[[device]]
name = "station-a"
host = "192.0.2.10"
path = ["Example", "Station A"]

[[job]]
device = "station-a"
kind = "archive"
archives = ["Main", 3, "elmeter"]
every = "15m"
anchor = "2026-01-01T00:00:00Z"

[[job]]
device = "station-a"
kind = "values"
set = "ActData"
every = "1m"

[[job]]
name = "clock"
device = "station-a"
kind = "clock"
every = "1h"
"#;

    #[test]
    fn example_parses() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.check().unwrap();
        assert_eq!(c.devices[0].port(), 2101);
        assert_eq!(c.devices[0].url(), "tcp://192.0.2.10:2101/1");
        assert_eq!(c.jobs[0].name, "station-a-archive-1");
        assert_eq!(c.jobs[0].every, Duration::from_secs(900));
        match &c.jobs[0].task {
            Task::Archive {
                archives,
                start_from,
                ..
            } => {
                let Archives::List(archives) = archives else {
                    panic!("{archives:?}")
                };
                let n: Vec<u8> = archives.iter().map(|a| a.number().unwrap()).collect();
                assert_eq!(n, [0, 3, 6]);
                assert_eq!(*start_from, StartFrom::All);
            }
            t => panic!("{t:?}"),
        }
        assert_eq!(c.jobs[1].name, "station-a-values-1");
        assert_eq!(c.jobs[2].task, Task::Clock { adjust_above: None });
        assert_eq!(c.service.heartbeat, Duration::from_secs(300));
    }

    #[test]
    fn status_listen() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(c.service.status_listen, Some(DEFAULT_STATUS_LISTEN));
        let with = |v: &str| {
            let text = EXAMPLE.replace(
                "listen = \"127.0.0.1:8650\"",
                &format!("listen = \"127.0.0.1:8650\"\nstatus_listen = \"{v}\""),
            );
            toml::from_str::<Config>(&text).map(|c| c.service.status_listen)
        };
        assert_eq!(with("off").unwrap(), None);
        assert_eq!(
            with("192.0.2.1:80").unwrap(),
            Some("192.0.2.1:80".parse().unwrap())
        );
        assert!(with("nowhere").is_err());
    }

    #[test]
    fn run_log_rotate() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(c.service.run_log_rotate, None);
        let with = |v: &str| {
            let text = EXAMPLE.replace(
                "listen = \"127.0.0.1:8650\"",
                &format!("listen = \"127.0.0.1:8650\"\nrun_log_rotate = \"{v}\""),
            );
            let mut c: Config = toml::from_str(&text).unwrap();
            c.check().map(|()| c.service.run_log_rotate)
        };
        assert_eq!(
            with("365d").unwrap(),
            Some(Duration::from_secs(365 * 86_400))
        );
        assert!(with("12h").is_err());
    }

    #[test]
    fn rejects_bad_references() {
        let bad = EXAMPLE.replace(
            "device = \"station-a\"\nkind = \"values\"",
            "device = \"nope\"\nkind = \"values\"",
        );
        let mut c: Config = toml::from_str(&bad).unwrap();
        assert!(c.check().is_err());
        let open = EXAMPLE.replace("127.0.0.1:8650", "0.0.0.0:8650");
        let mut c: Config = toml::from_str(&open).unwrap();
        assert!(
            c.check()
                .unwrap_err()
                .to_string()
                .contains("api_token_file")
        );
        let fast = EXAMPLE.replace("every = \"1m\"", "every = \"1s\"");
        let mut c: Config = toml::from_str(&fast).unwrap();
        assert!(c.check().is_err());
    }

    #[cfg(feature = "tls")]
    #[test]
    fn tls_needs_a_root_or_pins() {
        let with = |extra: &str| {
            let text = EXAMPLE.replace(
                "host = \"192.0.2.10\"",
                &format!("host = \"192.0.2.10\"\ntls = true\n{extra}"),
            );
            let mut c: Config = toml::from_str(&text).unwrap();
            c.check().map_err(|e| format!("{e:#}"))
        };
        let root = format!("tls_root = \"04{}\"", "11".repeat(64));
        assert!(with("").unwrap_err().contains("tls_root or tls_pins"));
        assert!(with(&root).is_ok());
        assert!(with(&format!("tls_pins = [\"{}\"]", "AB".repeat(32))).is_ok());
        assert!(with("tls_root = \"04AB\"").unwrap_err().contains("P-256"));
    }

    #[test]
    fn rejects_settings_that_fail_later() {
        let fails = |from: &str, to: &str, why: &str| {
            assert!(EXAMPLE.contains(from), "{from}");
            let mut c: Config = toml::from_str(&EXAMPLE.replace(from, to)).unwrap();
            let e = c.check().unwrap_err().to_string();
            assert!(e.contains(why), "{to}: {e}");
        };
        let values = "kind = \"values\"\nset = \"ActData\"\nevery = \"1m\"";
        fails(values, &format!("{values}\ntimeout = \"0s\""), "timeout");
        fails(
            values,
            "kind = \"values\"\nset = \"ActData\"\nevery = \"48h\"\nstart_time = \"10:00\"",
            "at most 24h",
        );
        fails(
            "listen = \"127.0.0.1:8650\"",
            "listen = \"127.0.0.1:8651\"",
            "same port",
        );
        fails(
            "listen = \"127.0.0.1:8650\"",
            "listen = \"127.0.0.1:8650\"\nstatus_listen = \"off\"\nstatus_api = true\n\
             status_api_token_file = \"/t\"",
            "status_listen",
        );
        fails(
            "listen = \"127.0.0.1:8650\"",
            "listen = \"127.0.0.1:8650\"\nstatus_api_token_file = \"/t\"",
            "status_api is not true",
        );
        fails(
            "name = \"station-a\"",
            "name = \"station a/b\"",
            "letters, digits",
        );
        fails("name = \"clock\"", "name = \"clock&x\"", "letters, digits");
    }

    #[test]
    fn warnings() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.check().unwrap();
        assert!(
            c.warnings()[0].contains("open to the network"),
            "{:?}",
            c.warnings()
        );
        c.service.status_listen_set = true;
        assert_eq!(c.warnings(), Vec::<String>::new());
        let odd = EXAMPLE.replace("every = \"1h\"", "every = \"7h\"\nstart_time = \"10:00\"");
        let mut c: Config = toml::from_str(&odd).unwrap();
        c.check().unwrap();
        c.service.status_listen_set = true;
        assert!(
            c.warnings()[0].contains("does not divide 24h"),
            "{:?}",
            c.warnings()
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            EXAMPLE.replace("[[device]]", "status_listen = \"off\"\n\n[[device]]"),
        )
        .unwrap();
        assert!(Config::load(&file).unwrap().service.status_listen_set);
    }

    #[test]
    fn all_archives() {
        let all = EXAMPLE.replace(
            "archives = [\"Main\", 3, \"elmeter\"]",
            "archives = \"all\"",
        );
        let mut c: Config = toml::from_str(&all).unwrap();
        c.check().unwrap();
        match &c.jobs[0].task {
            Task::Archive { archives, .. } => {
                assert_eq!(*archives, Archives::All);
                assert!(archives.includes(26));
            }
            t => panic!("{t:?}"),
        }
        let bad = EXAMPLE.replace(
            "archives = [\"Main\", 3, \"elmeter\"]",
            "archives = \"most\"",
        );
        assert!(toml::from_str::<Config>(&bad).is_err());
    }

    #[test]
    fn byte_sizes() {
        let size = |s: &str| s.parse::<ByteSize>().map(|b| b.0);
        assert_eq!(size("512").unwrap(), 512);
        assert_eq!(size("2KiB").unwrap(), 2048);
        assert_eq!(size("5 MiB").unwrap(), 5 << 20);
        assert_eq!(size("3MB").unwrap(), 3_000_000);
        assert!(size("5 mb").is_err());
        assert!(size("MiB").is_err());
        let limited = EXAMPLE.replace("\"elmeter\"]", "\"elmeter\"]\nmax_size = \"64KiB\"");
        let mut c: Config = toml::from_str(&limited).unwrap();
        c.check().unwrap();
        assert_eq!(c.jobs[0].task.byte_limit(), 64 << 10);
        let both = limited.replace("max_size", "max_mib = 1\nmax_size");
        let mut c: Config = toml::from_str(&both).unwrap();
        assert!(c.check().is_err());
    }

    #[test]
    fn link_forecast() {
        use std::fmt::Write;
        let mut text = String::from(
            "[service]\ndatabase = \"x.db\"\n[[device]]\nname = \"m\"\nhost = \"h\"\n",
        );
        for a in [0, 25, 6, 3, 5, 4] {
            writeln!(
                text,
                "[[job]]\ndevice = \"m\"\nkind = \"archive\"\narchives = [{a}]\nevery = \"1h\"\ntimeout = \"55m\"\nmax_mib = 5"
            )
            .unwrap();
        }
        let mut c: Config = toml::from_str(&text).unwrap();
        c.check().unwrap();
        let f = &forecast(&c)[0];
        assert_eq!(f.hold_permille, 6 * 916);
        assert_eq!(f.archive_bytes_per_day, Some(6 * 24 * (5 << 20)));
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.check().unwrap();
        let f = &forecast(&c)[0];
        assert_eq!(f.archive_bytes_per_day, None);
        assert_eq!(f.unlimited, ["station-a-archive-1"]);
        assert_eq!(f.other_runs_per_day, 1440 + 24);
        assert_eq!(f.hold_permille, 666 + 1000 + 166);
    }

    #[test]
    fn fingerprints() {
        let fp = "AB:".repeat(31) + "AB";
        assert_eq!(parse_fingerprint(&fp).unwrap(), [0xAB; 32]);
        assert!(parse_fingerprint("AB").is_err());
    }
}
