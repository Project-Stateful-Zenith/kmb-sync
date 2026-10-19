use crate::clock;
use crate::collect::{self, RunContext, Summary};
use crate::config::{Config, Job, Task};
use crate::device::{DeviceSlot, Secrets, Traffic};
use crate::schedule;
use crate::store::StoreHandle;
use anyhow::{Context, Result, anyhow};
use kmb_store::Store;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const DOWNTIME_WARN_MS: i64 = 12 * 3_600_000;
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(60);
const SIDE_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideRead {
    Startup,
    Identify,
}

impl SideRead {
    const fn job(self) -> &'static str {
        match self {
            Self::Startup => "startup-read",
            Self::Identify => "identify",
        }
    }
}
const CLOCK_STEP_MS: i64 = 60_000;
const MAX_SLEEP: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, Serialize)]
pub struct JobState {
    pub name: String,
    pub device: String,
    pub kind: &'static str,
    pub every_s: u64,
    pub enabled: bool,
    pub paused: bool,
    pub running: bool,
    pub next_run: Option<i64>,
    pub last_started: Option<i64>,
    pub last_finished: Option<i64>,
    pub last_outcome: Option<String>,
    pub last_summary: Option<Summary>,
    pub last_error: Option<String>,
    pub last_traffic: Option<kmb_store::Traffic>,
    pub period_bytes: u64,
    pub failures: u32,
    #[serde(skip)]
    pub resume_after: Option<u8>,
}

pub struct JobSlot {
    pub job: Job,
    pub state: Mutex<JobState>,
    pub trigger: Notify,
}

impl JobSlot {
    fn new(job: Job) -> Arc<Self> {
        let kind = match job.task {
            Task::Archive { .. } => "archive",
            Task::Values { .. } => "values",
            Task::Clock { .. } => "clock",
        };
        let state = JobState {
            name: job.name.clone(),
            device: job.device.clone(),
            kind,
            every_s: job.every.as_secs(),
            enabled: job.enabled,
            ..JobState::default()
        };
        Arc::new(Self {
            job,
            state: Mutex::new(state),
            trigger: Notify::new(),
        })
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut JobState) -> R) -> R {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut s)
    }

    #[must_use]
    pub fn snapshot(&self) -> JobState {
        self.update(|s| s.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done(Summary),
    Failed(String),
    Timeout,
    Cancelled,
}

impl Outcome {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Done(s) if !s.errors.is_empty() => "partial",
            Self::Done(s) if s.limited => "limited",
            Self::Done(_) => "ok",
            Self::Failed(_) => "failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

pub struct Service {
    pub config: Config,
    pub store: StoreHandle,
    pub devices: HashMap<String, Arc<DeviceSlot>>,
    pub jobs: Vec<Arc<JobSlot>>,
    pub cancel: CancellationToken,
    pub started_at: i64,
    pub envis: Option<crate::envis::EnvisSlot>,
    permits: Arc<Semaphore>,
}

impl Service {
    pub fn new(config: Config) -> Result<Arc<Self>> {
        let store = Store::open(&config.service.database)
            .with_context(|| format!("opening database {}", config.service.database.display()))?;
        let mut devices = HashMap::new();
        for d in &config.devices {
            let secrets = Secrets::load(d).with_context(|| format!("device {:?}", d.name))?;
            devices.insert(d.name.clone(), DeviceSlot::new(d.clone(), secrets));
        }
        let jobs = config.jobs.iter().cloned().map(JobSlot::new).collect();
        let envis = config
            .envis
            .as_ref()
            .map(crate::envis::EnvisSlot::new)
            .transpose()
            .context("envis")?;
        Ok(Arc::new(Self {
            envis,
            permits: Arc::new(Semaphore::new(config.service.max_concurrency)),
            store: StoreHandle::new(store),
            devices,
            jobs,
            cancel: CancellationToken::new(),
            started_at: clock::now_ms(),
            config,
        }))
    }

    #[must_use]
    pub fn job(&self, name: &str) -> Option<&Arc<JobSlot>> {
        self.jobs.iter().find(|j| j.job.name == name)
    }

    pub async fn run_once(&self, slot: &JobSlot) -> Result<(i64, Outcome)> {
        let job = &slot.job;
        let device = self
            .devices
            .get(&job.device)
            .ok_or_else(|| anyhow!("unknown device {:?}", job.device))?;
        let mut session = tokio::select! {
            () = self.cancel.cancelled() => return Ok((0, Outcome::Cancelled)),
            s = device.session.lock() => s,
        };
        let permit = tokio::select! {
            () = self.cancel.cancelled() => return Ok((0, Outcome::Cancelled)),
            p = self.permits.acquire() => p.map_err(|e| anyhow!("{e}"))?,
        };
        if self.cancel.is_cancelled() {
            return Ok((0, Outcome::Cancelled));
        }
        let started = clock::now_ms();
        let (name, dev) = (job.name.clone(), job.device.clone());
        let run = self
            .store
            .call(move |s| s.begin_run(&name, &dev, started))
            .await?;
        slot.update(|s| {
            s.running = true;
            s.last_started = Some(started);
        });
        let traffic = Traffic::default();
        let limit = job.task.byte_limit();
        let budget = (limit > 0).then(|| limit.saturating_sub(slot.update(|s| s.period_bytes)));
        let cx = RunContext {
            store: &self.store,
            device: device.as_ref(),
            describe_every: self.config.service.describe_every,
            run,
            cancel: &self.cancel,
            traffic: &traffic,
            budget,
            envis: self.envis.as_ref(),
            resume_after: slot.update(|s| s.resume_after),
        };
        let work = collect::run_job(&cx, &mut session, job);
        let outcome = match tokio::time::timeout(job.timeout, work).await {
            Err(_) => Outcome::Timeout,
            Ok(Ok(summary)) => Outcome::Done(summary),
            Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
        };
        drop(permit);
        drop(session);
        let finished = clock::now_ms();
        let bytes = traffic.get();
        let detail = match &outcome {
            Outcome::Done(s) => s.detail(),
            Outcome::Failed(e) => Some(e.clone()),
            Outcome::Timeout => Some(format!("no result within {:?}", job.timeout)),
            Outcome::Cancelled => None,
        };
        let name = outcome.name();
        slot.update(|s| {
            s.running = false;
            s.last_finished = Some(finished);
            s.last_outcome = Some(name.to_string());
            s.last_traffic = Some(bytes);
            s.period_bytes = s.period_bytes.saturating_add(traffic.total());
            match &outcome {
                Outcome::Done(summary) => {
                    s.failures = 0;
                    if summary.limited_at.is_some() || !summary.limited {
                        s.resume_after = summary.limited_at;
                    }
                    s.last_summary = Some(summary.clone());
                    s.last_error = None;
                }
                Outcome::Failed(e) => {
                    s.failures = s.failures.saturating_add(1);
                    s.last_error = Some(e.clone());
                }
                Outcome::Timeout => {
                    s.failures = s.failures.saturating_add(1);
                    s.last_error = Some("timeout".into());
                }
                Outcome::Cancelled => {}
            }
        });
        self.store
            .call(move |s| s.finish_run(run, name, detail.as_deref(), bytes, finished))
            .await?;
        match &outcome {
            Outcome::Failed(e) => tracing::warn!(job = %job.name, run, "run failed: {e}"),
            Outcome::Timeout => tracing::warn!(job = %job.name, run, "run timed out"),
            Outcome::Done(s) => tracing::info!(
                job = %job.name, run, inserted = s.inserted, duplicates = s.duplicates,
                rejected = s.rejected, limited = s.limited, bytes_rx = bytes.rx,
                bytes_tx = bytes.tx, "run done"
            ),
            Outcome::Cancelled => tracing::info!(job = %job.name, run, "run cancelled"),
        }
        Ok((run, outcome))
    }

    async fn job_loop(self: Arc<Self>, slot: Arc<JobSlot>) {
        let job = &slot.job;
        let every = clock::ms(job.every);
        let anchor = self.anchor(job);
        let mut next = job
            .enabled
            .then(|| schedule::first_run(clock::now_ms(), every, &anchor));
        // a job without anchor runs at once after a start, still inside the
        // period of the runs before; their bytes count against its budget
        let mut retry = false;
        if matches!(anchor, schedule::Anchor::None) && job.task.byte_limit() > 0 {
            match self.restore_period_bytes(&slot).await {
                Ok(used) => retry = used > 0,
                Err(e) => tracing::warn!(job = %job.name, "byte budget not restored: {e:#}"),
            }
        }
        let mut planned = clock::now_ms();
        loop {
            slot.update(|s| s.next_run = next);
            let wait = next.map_or(MAX_SLEEP, |n| {
                clock::until(n, clock::now_ms()).min(MAX_SLEEP)
            });
            let manual = tokio::select! {
                () = self.cancel.cancelled() => return,
                () = slot.trigger.notified() => true,
                () = tokio::time::sleep(wait) => false,
            };
            let now = clock::now_ms();
            if now < planned.saturating_sub(CLOCK_STEP_MS) {
                tracing::warn!(job = %job.name, "clock stepped back; planning the next run again");
                next = job
                    .enabled
                    .then(|| schedule::first_run(now, every, &anchor));
                planned = now;
                continue;
            }
            if !manual {
                if next.is_none_or(|n| now < n) {
                    continue;
                }
                if slot.update(|s| s.paused) {
                    next = Some(schedule::after_success(now, now, every, &anchor));
                    planned = now;
                    retry = false;
                    continue;
                }
                if !retry {
                    slot.update(|s| s.period_bytes = 0);
                }
            }
            let started = clock::now_ms();
            let outcome = match self.run_once(&slot).await {
                Ok((_, o)) => o,
                Err(e) => {
                    tracing::error!(job = %job.name, "run log failed: {e:#}");
                    Outcome::Failed(e.to_string())
                }
            };
            if outcome == Outcome::Cancelled {
                return;
            }
            let now = clock::now_ms();
            retry = !matches!(outcome, Outcome::Done(_));
            next = job.enabled.then(|| match outcome {
                Outcome::Done(_) => schedule::after_success(now, started, every, &anchor),
                _ => schedule::after_failure(now, slot.update(|s| s.failures), every),
            });
            planned = now;
        }
    }

    /// Sets the bytes used in the current period from the runs of the last
    /// `every`, as after a restart, and returns them.
    pub async fn restore_period_bytes(&self, slot: &JobSlot) -> Result<u64> {
        let (job, since) = (
            slot.job.name.clone(),
            clock::now_ms().saturating_sub(clock::ms(slot.job.every)),
        );
        let used = self
            .store
            .call(move |s| s.job_bytes_since(&job, since))
            .await?;
        slot.update(|s| s.period_bytes = used);
        Ok(used)
    }

    fn anchor(&self, job: &Job) -> schedule::Anchor {
        if let Some(start) = job.start_time {
            let zone = self
                .envis
                .as_ref()
                .map_or_else(jiff::tz::TimeZone::system, |e| e.calendar.zone.clone());
            return schedule::Anchor::Local { start, zone };
        }
        job.anchor.map_or(schedule::Anchor::None, |a| {
            schedule::Anchor::At(clock::unix_ms(a))
        })
    }

    async fn startup_checks(&self) -> Result<()> {
        let now = clock::now_ms();
        if let Some(hb) = self.store.call(|s| s.heartbeat()).await? {
            if !hb.clean_shutdown {
                tracing::warn!(
                    last_beat = %clock::rfc3339(hb.last_beat),
                    "previous run did not shut down cleanly"
                );
            }
            if now.saturating_sub(hb.last_beat) > DOWNTIME_WARN_MS {
                tracing::warn!(
                    last_beat = %clock::rfc3339(hb.last_beat),
                    "service was down for more than 12 hours"
                );
            }
        }
        let stale = self.store.call(move |s| s.close_stale_runs(now)).await?;
        if stale > 0 {
            tracing::warn!(stale, "marked runs of the previous service as interrupted");
        }
        self.store.call(move |s| s.beat(now, false)).await
    }

    async fn survey(self: Arc<Self>, device: Arc<DeviceSlot>) {
        match self.side_read(&device, SideRead::Startup).await {
            Ok(()) => {
                tracing::info!(device = %device.config.name, "read identity and archive list");
            }
            Err(e) => tracing::warn!(device = %device.config.name, "{e:#}"),
        }
    }

    pub async fn identify(&self, device: &DeviceSlot) -> Result<crate::device::IdentitySummary> {
        self.side_read(device, SideRead::Identify).await?;
        device
            .snapshot()
            .identity
            .ok_or_else(|| anyhow!("no identity after identify"))
    }

    async fn side_read(&self, device: &DeviceSlot, what: SideRead) -> Result<()> {
        let mut session = tokio::select! {
            () = self.cancel.cancelled() => anyhow::bail!("service stopping"),
            s = device.session.lock() => s,
        };
        if what == SideRead::Startup && session.is_some() {
            return Ok(());
        }
        let _permit = tokio::select! {
            () = self.cancel.cancelled() => anyhow::bail!("service stopping"),
            p = self.permits.acquire() => p.map_err(|e| anyhow!("{e}"))?,
        };
        let (job, dev, started) = (what.job(), device.config.name.clone(), clock::now_ms());
        let run = self
            .store
            .call(move |s| s.begin_run(job, &dev, started))
            .await?;
        let traffic = Traffic::default();
        let read = async {
            match what {
                SideRead::Startup => {
                    collect::survey(device, &mut session, self.envis.is_some(), &traffic).await
                }
                SideRead::Identify => collect::identify(device, &traffic).await.map(drop),
            }
        };
        let (outcome, result) = match tokio::time::timeout(SIDE_READ_TIMEOUT, read).await {
            Ok(Ok(())) => ("ok", Ok(())),
            Ok(Err(e)) => ("failed", Err(e.context(job))),
            Err(_) => (
                "timeout",
                Err(anyhow!("{job}: no answer within {SIDE_READ_TIMEOUT:?}")),
            ),
        };
        drop(session);
        let detail = result.as_ref().err().map(|e| format!("{e:#}"));
        if let Some(e) = &detail {
            device.update(|s| s.last_error = Some(e.clone()));
        }
        let (bytes, finished) = (traffic.get(), clock::now_ms());
        self.store
            .call(move |s| s.finish_run(run, outcome, detail.as_deref(), bytes, finished))
            .await?;
        result
    }

    async fn serve(
        &self,
        tasks: &mut JoinSet<()>,
        addr: std::net::SocketAddr,
        app: axum::Router,
        what: &'static str,
    ) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {what} on {addr}"))?;
        let cancel = self.cancel.clone();
        tasks.spawn(async move {
            let r = axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await;
            if let Err(e) = r {
                tracing::error!("{what} stopped: {e}");
            }
        });
        Ok(())
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        for w in self.config.warnings() {
            tracing::warn!("{w}");
        }
        self.startup_checks().await?;
        let mut tasks = JoinSet::new();
        if let Some(addr) = self.config.service.listen {
            let token = self
                .config
                .service
                .api_token_file
                .as_deref()
                .map(crate::config::read_token)
                .transpose()?;
            tracing::info!(%addr, auth = token.is_some(), "API listening");
            let app = crate::api::router(self.clone(), token);
            self.serve(&mut tasks, addr, app, "API").await?;
        }
        if let Some(addr) = self.config.service.status_listen {
            let service = &self.config.service;
            let token = service
                .status_api_token_file
                .as_deref()
                .filter(|_| service.status_api)
                .map(crate::config::read_token)
                .transpose()?;
            tracing::info!(api = token.is_some(), "status page on http://{addr}/");
            let app = crate::api::status_router(self.clone(), token);
            self.serve(&mut tasks, addr, app, "status page").await?;
        }
        for device in self.devices.values() {
            let name = &device.config.name;
            if self
                .jobs
                .iter()
                .any(|j| j.job.enabled && j.job.device == *name)
            {
                tasks.spawn(self.clone().survey(device.clone()));
            }
        }
        for slot in &self.jobs {
            tasks.spawn(self.clone().job_loop(slot.clone()));
        }
        let me = self.clone();
        tasks.spawn(async move { me.heartbeat_loop().await });
        tracing::info!(
            devices = self.devices.len(),
            jobs = self.jobs.len(),
            "service started"
        );

        self.cancel.cancelled().await;
        tracing::info!("stopping");
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!("tasks still running after {SHUTDOWN_GRACE:?}, aborting them");
            tasks.abort_all();
        }
        let now = clock::now_ms();
        self.store.call(move |s| s.beat(now, true)).await?;
        tracing::info!("stopped");
        Ok(())
    }

    async fn heartbeat_loop(&self) {
        let mut tick = tokio::time::interval(self.config.service.heartbeat);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let now = clock::now_ms();
            if let Err(e) = self.store.call(move |s| s.beat(now, false)).await {
                tracing::error!("heartbeat failed: {e:#}");
            }
            if let Some(period) = self.config.service.run_log_rotate {
                self.rotate_run_log(now, clock::ms(period), first).await;
            }
            first = false;
        }
    }

    async fn rotate_run_log(&self, now: i64, period: i64, starting: bool) {
        match self.store.call(move |s| s.rotate_runs(now, period)).await {
            Ok(p) if p.deleted > 0 || starting => tracing::info!(
                period_start = %clock::rfc3339(p.start),
                next_rotation = %clock::rfc3339(p.start.saturating_add(period)),
                deleted = p.deleted,
                "run log"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!("rotating the run log failed: {e:#}"),
        }
    }
}

pub async fn stop_on_signal(cancel: CancellationToken) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    cancel.cancel();
    Ok(())
}
