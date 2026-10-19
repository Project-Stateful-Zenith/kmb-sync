use crate::clock;
use crate::config::{self, Job, StartFrom, Task};
use crate::device::{
    Described, DeviceSlot, IdentitySummary, MeterClient, Traffic, definition, identity_info,
};
use crate::store::StoreHandle;
use anyhow::{Context, Result, anyhow, bail};
use kmb::KmbTime;
use kmb::archive::uni::archive_name;
use kmb::archive::uni::{Layout, Reject};
use kmb::uni::{DataSet, Description};
use kmb_store::{Batch, DefinitionId, MeasurementId, NewRecord, Rejected};
use std::collections::HashSet;
use std::ops::Range;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub const BATCHES_PER_COMMIT: u32 = 8;
pub const MISSING_RETRIES: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Summary {
    pub inserted: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub limited: bool,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
    #[serde(skip)]
    pub limited_at: Option<u8>,
}

impl Summary {
    const fn add(&mut self, r: kmb_store::IngestReport) {
        self.inserted = self.inserted.saturating_add(r.inserted);
        self.duplicates = self.duplicates.saturating_add(r.duplicates);
        self.rejected = self.rejected.saturating_add(r.rejected);
    }

    #[must_use]
    pub fn detail(&self) -> Option<String> {
        let all: Vec<&str> = self
            .notes
            .iter()
            .chain(&self.errors)
            .map(String::as_str)
            .collect();
        (!all.is_empty()).then(|| all.join("; "))
    }
}

pub struct RunContext<'a> {
    pub store: &'a StoreHandle,
    pub device: &'a DeviceSlot,
    pub describe_every: std::time::Duration,
    pub run: i64,
    pub cancel: &'a CancellationToken,
    pub traffic: &'a Traffic,
    pub budget: Option<u64>,
    pub envis: Option<&'a crate::envis::EnvisSlot>,
    pub resume_after: Option<u8>,
}

enum Sink<'s> {
    Local {
        measurement: MeasurementId,
        definition: DefinitionId,
    },
    Feeders {
        db: &'s mut kmb_envis::db::Envis,
        calendar: &'s kmb_envis::pack::Calendar,
        meas: i32,
        outputs: Vec<FeederOut>,
        layout: kmb_envis::emi::LbLayout,
        time_at: usize,
        period: u64,
    },
    Envis {
        db: &'s mut kmb_envis::db::Envis,
        calendar: &'s kmb_envis::pack::Calendar,
        meas: i32,
        def: i32,
        period: u64,
        remove: usize,
        row_len: usize,
    },
}

struct EnvisRun<'s> {
    slot: &'s crate::envis::EnvisSlot,
    conn: crate::envis::Conn<'s>,
    meas: i32,
    object: String,
    url: String,
}

struct FeederOut {
    name: String,
    meas: i32,
    archive: u8,
    def: i32,
    slot: u32,
    props: Vec<kmb_envis::definition::Prop>,
}

pub async fn run_job(
    cx: &RunContext<'_>,
    session: &mut Option<Described>,
    job: &Job,
) -> Result<Summary> {
    if cx.budget == Some(0) {
        return Ok(Summary {
            limited: true,
            notes: vec!["byte limit of this period used up; not connected".into()],
            ..Summary::default()
        });
    }
    let mut client = cx.device.connect(cx.traffic).await?;
    let id = client.identify().await.context("identify")?;
    let summary = IdentitySummary::new(&id, client.is_encrypted());
    cx.device.update(|s| s.identity = Some(summary));
    if session
        .as_ref()
        .is_some_and(|d| !d.matches(&id) || d.at.elapsed() > cx.describe_every)
    {
        *session = None;
    }

    let d = &cx.device.config;
    let identity = {
        let (path, info, url) = (d.object_path(), identity_info(&id), d.url());
        let now = clock::now_ms();
        cx.store
            .call(move |s| s.upsert_identity(&path, &info, &url, now))
            .await?
    };
    let measurement = {
        let name = job.measurement.clone();
        cx.store
            .call(move |s| s.measurement(identity, &name))
            .await?
    };

    let result = match &job.task {
        Task::Clock { adjust_above } => clock_job(cx, &mut client, *adjust_above).await,
        Task::Archive { .. } | Task::Values { .. } if !id.is_universal() => Err(anyhow!(
            "meter {} is not a universal-config meter; classic meters are not supported",
            id.device_no
        )),
        Task::Archive {
            archives,
            start_from,
            ..
        } => {
            let described = describe(session, &mut client, &id).await?;
            archive_job(
                cx,
                &mut client,
                &id,
                described,
                measurement,
                archives,
                *start_from,
            )
            .await
        }
        Task::Values { set } => {
            let described = describe(session, &mut client, &id).await?;
            values_job(
                cx,
                &mut client,
                described,
                measurement,
                config::data_set(set)?,
            )
            .await
        }
    };
    if let Err(e) = &result {
        cx.device.update(|s| s.last_error = Some(format!("{e:#}")));
    }
    result
}

const fn unsupported(envis: bool, archive: u8) -> Option<&'static str> {
    if envis {
        crate::envis::envis_unsupported(archive)
    } else {
        None
    }
}

fn offered_states(
    described: &Described,
    offered: &[crate::envis::Offered],
    envis: bool,
) -> Vec<crate::device::ArchiveState> {
    offered
        .iter()
        .map(|o| crate::device::ArchiveState {
            archive: o.archive,
            name: o.name.clone(),
            period_ms: crate::envis::period(&described.config, o.ic),
            unsupported: if archive_name(o.archive) == Some(o.name.as_str()) {
                unsupported(envis, o.archive).map(Into::into)
            } else {
                Some("unknown archive".into())
            },
            ..Default::default()
        })
        .collect()
}

pub async fn survey(
    device: &DeviceSlot,
    session: &mut Option<Described>,
    envis: bool,
    traffic: &Traffic,
) -> Result<()> {
    let mut client = device.connect(traffic).await?;
    let id = client.identify().await.context("identify")?;
    let summary = IdentitySummary::new(&id, client.is_encrypted());
    device.update(|s| s.identity = Some(summary));
    if !id.is_universal() {
        return Ok(());
    }
    if session.as_ref().is_some_and(|d| !d.matches(&id)) {
        *session = None;
    }
    let described = describe(session, &mut client, &id).await?;
    let offered = crate::envis::offered(archive_descriptors(&mut client, described).await?);
    let states = offered_states(described, &offered, envis);
    device.update(|s| s.set_offered(states));
    Ok(())
}

pub async fn identify(device: &DeviceSlot, traffic: &Traffic) -> Result<IdentitySummary> {
    let mut client = device.connect(traffic).await?;
    let id = client.identify().await.context("identify")?;
    let summary = IdentitySummary::new(&id, client.is_encrypted());
    device.update(|s| s.identity = Some(summary.clone()));
    Ok(summary)
}

async fn archive_job(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    id: &kmb::Identify,
    described: &mut Described,
    measurement: MeasurementId,
    archives: &config::Archives,
    start_from: StartFrom,
) -> Result<Summary> {
    let mut envis = match cx.envis {
        Some(slot) => Some(envis_run(cx, slot, client, id, described).await?),
        None => None,
    };
    let mut out = Summary::default();
    let list = resume_order(
        archive_list(cx, client, described, archives, &mut out).await?,
        cx.resume_after,
    );
    for &archive in &list {
        let before = out.inserted;
        let mut newest = None;
        let r = sync_archive(
            cx,
            client,
            described,
            measurement,
            archive,
            start_from,
            &mut out,
            envis.as_mut(),
            &mut newest,
        )
        .await;
        let error = r.as_ref().err().map(|e| format!("{e:#}"));
        let newest = newest.and_then(|t| KmbTime(t).to_unix_ms());
        let inserted = out.inserted.saturating_sub(before);
        cx.device
            .update(|s| s.archive_synced(archive, inserted, newest, error));
        if let Err(e) = r {
            if let Some(run) = envis.as_mut()
                && connection_broken(&e)
            {
                run.conn.mark_broken();
            }
            out.errors.push(format!("archive {archive}: {e:#}"));
        }
        if cx.cancel.is_cancelled() || out.limited {
            break;
        }
    }
    if !out.errors.is_empty() && out.errors.len() == list.len() {
        bail!("{}", out.errors.join("; "));
    }
    if let Some(run) = envis {
        run.conn.release();
    }
    Ok(out)
}

// only SQL errors can leave the connection mid-transaction; the other ENVIS
// errors (no definition, row length, time) happen before SQL or after a rollback
fn connection_broken(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<kmb_envis::Error>(),
            Some(kmb_envis::Error::Sql(_))
        )
    })
}

fn resume_order(mut list: Vec<u8>, after: Option<u8>) -> Vec<u8> {
    if let Some(i) = after.and_then(|a| list.iter().position(|&x| x == a)) {
        list.rotate_left(i.saturating_add(1));
    }
    list
}

async fn archive_list(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    described: &mut Described,
    archives: &config::Archives,
    out: &mut Summary,
) -> Result<Vec<u8>> {
    let offered = crate::envis::offered(archive_descriptors(client, described).await?);
    let unsupported = |a: u8| unsupported(cx.envis.is_some(), a);
    let states = offered_states(described, &offered, cx.envis.is_some());
    cx.device.update(|s| s.set_offered(states));
    match archives {
        config::Archives::All => {
            let mut list = Vec::new();
            for o in &offered {
                match (
                    archive_name(o.archive) == Some(o.name.as_str()),
                    unsupported(o.archive),
                ) {
                    (false, _) => out.notes.push(format!(
                        "archive {:?} (group {}) not synced: unknown archive",
                        o.name, o.ic
                    )),
                    (true, Some(why)) => out.notes.push(format!(
                        "archive {} ({}) not synced: {why}",
                        o.archive, o.name
                    )),
                    (true, None) => list.push(o.archive),
                }
            }
            Ok(list)
        }
        config::Archives::List(l) => {
            let list = l
                .iter()
                .map(config::ArchiveRef::number)
                .collect::<Result<Vec<u8>>>()?;
            for o in offered.iter().filter(|o| !list.contains(&o.archive)) {
                out.notes.push(format!(
                    "archive {} ({}) offered by the meter but not in the job",
                    o.archive, o.name
                ));
            }
            Ok(list)
        }
    }
}

async fn envis_run<'s>(
    cx: &RunContext<'_>,
    slot: &'s crate::envis::EnvisSlot,
    client: &mut MeterClient,
    id: &kmb::Identify,
    described: &mut Described,
) -> Result<EnvisRun<'s>> {
    let (object, meas_name) = crate::envis::notebook(client, cx.device.config.address).await?;
    client
        .read_values(kmb::uni::DataSet::Config, &mut described.config)
        .await
        .context("reading Config values")?;
    let archives = archive_descriptors(client, described).await?;
    let row = crate::envis::identity_row(id, archives);
    let url = cx.device.config.envis_url();
    let config_time = described
        .config
        .value("Message", "CfgSetTime")
        .and_then(kmb::uni::Value::as_u64);
    let blobs = config_time.map(|t| {
        (
            t,
            kmb_envis::config::try_description_blob(&described.config),
            kmb_envis::config::data_blob(&described.config),
        )
    });
    let mut conn = slot.acquire().await?;
    let db = conn.db()?;
    let stored = async {
        let m = db.measurement(&object, &meas_name, &row, &url).await?;
        if let Some((t, desc, data)) = &blobs {
            match (desc, data) {
                (Ok(desc), Ok(data)) => db.store_config(m.id, *t, desc, data).await?,
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!("configuration snapshot not stored: {e}");
                }
            }
        }
        Ok::<_, kmb_envis::Error>(m)
    }
    .await;
    let m = match stored {
        Ok(m) => m,
        Err(e) => {
            conn.mark_broken();
            return Err(e.into());
        }
    };
    Ok(EnvisRun {
        slot,
        conn,
        meas: m.id,
        object,
        url,
    })
}

async fn archive_descriptors<'d>(
    client: &mut MeterClient,
    described: &'d mut Described,
) -> Result<&'d kmb::uni::ArchiveDescription> {
    if described.archives.is_none() {
        let a = client
            .read_archive_description()
            .await
            .context("reading archive descriptors")?;
        described.archives = Some(a);
    }
    described
        .archives
        .as_ref()
        .ok_or_else(|| anyhow!("archive descriptors missing"))
}

async fn describe<'s>(
    session: &'s mut Option<Described>,
    client: &mut MeterClient,
    id: &kmb::Identify,
) -> Result<&'s mut Described> {
    if session.is_none() {
        let config = client
            .read_description(DataSet::Config)
            .await
            .context("reading Config")?;
        *session = Some(Described {
            identify: id.clone(),
            config,
            archives: None,
            layouts: std::collections::HashMap::new(),
            sets: std::collections::HashMap::new(),
            at: Instant::now(),
        });
    }
    session
        .as_mut()
        .ok_or_else(|| anyhow!("description cache is empty"))
}

async fn layout(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    described: &mut Described,
    archive: u8,
) -> Result<(Layout, DefinitionId)> {
    if let Some(found) = described.layouts.get(&archive) {
        return Ok(found.clone());
    }
    archive_descriptors(client, described).await?;
    let desc = described
        .archives
        .as_ref()
        .ok_or_else(|| anyhow!("archive descriptors missing"))?;
    let (version, max_size) = described.header(desc);
    let layout = Layout::new(desc, archive, version, max_size)
        .map_err(|e| anyhow!("no usable layout: {e}"))?;
    let def = definition(&layout, desc);
    let id = cx.store.call(move |s| s.definition(&def)).await?;
    described.layouts.insert(archive, (layout.clone(), id));
    Ok((layout, id))
}

async fn plan(
    client: &mut MeterClient,
    archive: u8,
    cursor: Option<u64>,
    start_from: StartFrom,
    notes: &mut Vec<String>,
) -> Result<(Vec<Range<u32>>, Option<u64>)> {
    let ring = client.probe_archive(archive).await.context("probe")?;
    if ring.is_empty() {
        return Ok((Vec::new(), None));
    }
    let (from, keep_from) = match (cursor, start_from) {
        (Some(c), _) => (KmbTime(c), None),
        (None, StartFrom::All) => return Ok((ring.runs(), None)),
        (None, StartFrom::Time(t)) => {
            let t = clock::to_kmb(clock::unix_ms(t));
            (t, Some(t.0))
        }
    };
    let at = client
        .record_at_time(archive, from)
        .await
        .context("record lookup")?;
    if let Some(slot) = at.slot {
        let mut runs = ring.runs_from(slot);
        if !runs.is_empty() {
            if cursor.is_some() {
                if at.time > from {
                    notes.push(gap_note(archive, at.time));
                } else if let Some(first) = runs.first_mut() {
                    first.start = first.start.saturating_add(1);
                }
            }
            runs.retain(|r| !r.is_empty());
            if cursor.is_some() && at.time < from && runs.is_empty() {
                bail!(
                    "the meter's newest record ({}) is older than the newest stored one ({}): \
                     did the meter clock go back? Records are read again once the meter's \
                     time passes the stored one",
                    clock::kmb_rfc3339(at.time),
                    clock::kmb_rfc3339(from)
                );
            }
            return Ok((runs, keep_from));
        }
    }
    let first = client
        .record_at_time(archive, kmb::archive::FIRST_RECORD_TIME)
        .await
        .context("first record lookup")?;
    if first.slot.is_some() && first.time > from {
        if cursor.is_some() {
            notes.push(gap_note(archive, first.time));
        }
        return Ok((ring.runs(), keep_from));
    }
    Ok((Vec::new(), None))
}

fn gap_note(archive: u8, oldest: KmbTime) -> String {
    format!(
        "archive {archive}: ring overwrote records after the cursor; oldest record read is from {}",
        clock::kmb_rfc3339(oldest)
    )
}

const fn reason(r: Reject) -> &'static str {
    match r {
        Reject::Erased => "erased",
        Reject::Ecc => "ecc",
        Reject::Sum => "sum",
        Reject::Length => "length",
        Reject::TimeZero => "time-zero",
        Reject::TimeInvalid => "time-invalid",
        Reject::TimeAfter2100 => "time-after-2100",
    }
}

fn ranges_note(archive: u8, read: &[Range<u32>]) -> String {
    const SHOWN: usize = 5;
    let total: u32 = read.iter().map(|r| r.end.saturating_sub(r.start)).sum();
    let mut text: Vec<String> = read
        .iter()
        .take(SHOWN)
        .map(|r| format!("{}..{}", r.start, r.end))
        .collect();
    if read.len() > SHOWN {
        text.push(format!("{} more", read.len().saturating_sub(SHOWN)));
    }
    format!("archive {archive}: read {total} slots {}", text.join(", "))
}

fn push_range(read: &mut Vec<Range<u32>>, part: Range<u32>) {
    match read.last_mut() {
        Some(last) if last.end == part.start => last.end = part.end,
        _ => read.push(part),
    }
}

fn sort_out(
    layout: &Layout,
    got: Vec<kmb_tokio::Record>,
    part: Range<u32>,
    keep_from: Option<u64>,
    cursor: Option<u64>,
    older: &mut usize,
) -> (Vec<NewRecord>, Vec<Rejected>) {
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    let mut rejected = Vec::new();
    for r in got {
        seen.insert(r.slot);
        match r.bytes {
            Ok(bytes) => {
                let Some(time) = layout.time(&bytes) else {
                    rejected.push(Rejected {
                        slot: r.slot,
                        reason: "time".into(),
                    });
                    continue;
                };
                if keep_from.is_some_and(|k| time.0 < k) {
                    continue;
                }
                if cursor.is_some_and(|c| time.0 < c) {
                    *older = older.saturating_add(1);
                }
                records.push(NewRecord {
                    slot: r.slot,
                    time: time.0,
                    bytes,
                });
            }
            Err(Reject::Erased) => {}
            Err(e) => rejected.push(Rejected {
                slot: r.slot,
                reason: reason(e).into(),
            }),
        }
    }
    rejected.extend(part.filter(|s| !seen.contains(s)).map(|slot| Rejected {
        slot,
        reason: "missing".into(),
    }));
    (records, rejected)
}

/// Drops what was read from the first slot the meter left out, so that slot is
/// read again instead of the cursor moving past it, and returns the slot to
/// continue from. After `MISSING_RETRIES` tries without progress the slot is
/// kept as missing and skipped.
fn hold_back_missing(
    part: &Range<u32>,
    records: &mut Vec<NewRecord>,
    rejected: &mut Vec<Rejected>,
    retries: &mut u32,
) -> u32 {
    let missing = rejected
        .iter()
        .filter(|r| r.reason == "missing")
        .map(|r| r.slot)
        .min();
    match missing {
        Some(m) if *retries < MISSING_RETRIES => {
            *retries = if m > part.start {
                0
            } else {
                retries.saturating_add(1)
            };
            records.retain(|r| r.slot < m);
            rejected.retain(|r| r.slot < m);
            m
        }
        _ => {
            *retries = 0;
            part.end
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_archive(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    described: &mut Described,
    measurement: MeasurementId,
    archive: u8,
    start_from: StartFrom,
    out: &mut Summary,
    envis: Option<&mut EnvisRun<'_>>,
    newest: &mut Option<u64>,
) -> Result<()> {
    let (layout, definition) = layout(cx, client, described, archive).await?;
    let (mut sink, cursor) = match envis {
        None => {
            let cursor = cx
                .store
                .call(move |s| s.cursor(measurement, archive))
                .await?
                .map(|c| c.last_time);
            (
                Sink::Local {
                    measurement,
                    definition,
                },
                cursor,
            )
        }
        Some(run) if layout.group.name == "LB" => feeder_sink(run, described, &layout).await?,
        Some(_) if crate::envis::envis_unsupported(archive).is_some() => {
            bail!(
                "{}",
                crate::envis::envis_unsupported(archive).unwrap_or_default()
            )
        }
        Some(run) => envis_sink(run, described, &layout, archive).await?,
    };
    *newest = cursor;
    let (runs, keep_from) = plan(client, archive, cursor, start_from, &mut out.notes).await?;
    let geometry = client.geometry(&described.config, &layout);
    let per_request = u32::from(geometry.per_frame).saturating_mul(u32::from(geometry.frames));
    let record_len = u64::try_from(layout.record_len).unwrap_or(u64::MAX).max(1);
    let mut read = Vec::new();
    let mut older = 0usize;
    let mut retries = 0u32;
    let result = async {
        for run in runs {
            let mut rest = run;
            while !rest.is_empty() {
                if cx.cancel.is_cancelled() {
                    out.notes
                        .push(format!("archive {archive}: stopped by shutdown"));
                    return Ok(());
                }
                let mut size = per_request.saturating_mul(BATCHES_PER_COMMIT);
                if let Some(budget) = cx.budget {
                    let left = budget.saturating_sub(cx.traffic.total());
                    if left == 0 {
                        out.limited = true;
                        out.limited_at = Some(archive);
                        out.notes.push(format!(
                            "archive {archive}: byte limit reached, {} or more slots left for the next run",
                            rest.end.saturating_sub(rest.start)
                        ));
                        return Ok(());
                    }
                    let fit = u32::try_from(left.div_ceil(record_len)).unwrap_or(u32::MAX);
                    size = size.min(fit);
                }
                let part = rest.start..rest.start.saturating_add(size.max(1)).min(rest.end);
                rest.start = part.end;
                let got = client
                    .read_records(&layout, part.clone(), geometry)
                    .await
                    .with_context(|| format!("reading slots {}..{}", part.start, part.end))?;
                let (mut records, mut rejected) =
                    sort_out(&layout, got, part.clone(), keep_from, cursor, &mut older);
                rest.start = hold_back_missing(&part, &mut records, &mut rejected, &mut retries);
                if part.start < rest.start {
                    push_range(&mut read, part.start..rest.start);
                }
                let top = records.iter().map(|r| r.time).max();
                let report = sink.store(cx, archive, records, rejected).await?;
                *newest = (*newest).max(top);
                out.add(report);
            }
        }
        Ok(())
    }
    .await;
    if !read.is_empty() {
        out.notes.push(ranges_note(archive, &read));
    }
    if older > 0 {
        out.notes.push(format!(
            "archive {archive}: {older} records older than the cursor; did the meter clock step back?"
        ));
    }
    result
}

async fn envis_sink<'s>(
    run: &'s mut EnvisRun<'_>,
    described: &Described,
    layout: &Layout,
    archive: u8,
) -> Result<(Sink<'s>, Option<u64>)> {
    let desc = described
        .archives
        .as_ref()
        .ok_or_else(|| anyhow!("archive descriptors missing"))?;
    let (fields, remove) =
        kmb_envis::definition::TypeDescription::from_descriptors(desc, layout.group.idx)?;
    let row_len = layout.record_len.saturating_sub(remove);
    let (slot, meas) = (run.slot, run.meas);
    let db = run.conn.db()?;
    let def = db
        .definition_for(meas, archive, &fields, slot.allow_new)
        .await?;
    let cursor = db.last_time(meas, archive).await?;
    let period = crate::envis::period(&described.config, layout.group.idx);
    Ok((
        Sink::Envis {
            db,
            calendar: &slot.calendar,
            meas,
            def,
            period,
            remove,
            row_len,
        },
        cursor,
    ))
}

async fn feeder_sink<'s>(
    run: &'s mut EnvisRun<'_>,
    described: &Described,
    layout: &Layout,
) -> Result<(Sink<'s>, Option<u64>)> {
    use kmb_envis::emi;
    let desc = described
        .archives
        .as_ref()
        .ok_or_else(|| anyhow!("archive descriptors missing"))?;
    let ic = layout.group.idx;
    let lb = emi::LbLayout::new(desc, ic).map_err(|e| anyhow!(e))?;
    let (_, time_at) = kmb_envis::definition::TypeDescription::from_descriptors(desc, ic)?;
    let feeders = emi::feeders(&described.config).map_err(|e| anyhow!(e))?;
    let period = crate::envis::period(&described.config, ic);
    let cfg_time = described
        .config
        .value("Message", "CfgSetTime")
        .and_then(kmb::uni::Value::as_u64);
    let unom = described
        .config
        .value("Install", "Unom")
        .and_then(kmb::uni::Value::as_f64)
        .map_or(230.0, |u| {
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let u = u as f32;
            u
        });
    let (slot, main) = (run.slot, run.meas);
    let (object, url) = (run.object.clone(), run.url.clone());
    let db = run.conn.db()?;
    let mut outputs = Vec::new();
    let mut resume: Option<u64> = None;
    let mut any_empty = false;
    for f in &feeders {
        let m = db.measurement(&object, &f.name, &f.identity, &url).await?;
        if let Some(t) = cfg_time {
            let (d, v) = emi::feeder_config(t, unom, f);
            db.store_config(m.id, t, &d, &v).await?;
        }
        for archive in [0_u8, 6] {
            match db.last_time(m.id, archive).await? {
                Some(t) => resume = Some(resume.map_or(t, |r| r.min(t))),
                None => any_empty = true,
            }
            outputs.push(FeederOut {
                name: f.name.clone(),
                meas: m.id,
                archive,
                def: db.newest_definition(m.id, archive).await?.unwrap_or(0),
                slot: f.slot,
                props: Vec::new(),
            });
        }
    }
    for archive in [0_u8, 6] {
        let mut known = outputs
            .iter()
            .find(|o| o.archive == archive && o.def != 0)
            .map(|o| o.def);
        if known.is_none() && outputs.iter().any(|o| o.archive == archive) {
            known = db
                .definitions()
                .await?
                .iter()
                .filter(|d| lb.fits(archive, &d.fields.props))
                .map(|d| d.id)
                .max();
        }
        for o in outputs
            .iter_mut()
            .filter(|o| o.archive == archive && o.def == 0)
        {
            o.def = known.ok_or_else(|| {
                anyhow!(
                    "feeder {}: no ENVIS definition for archive {archive} is stored; ENVIS must store this meter's feeders once",
                    o.name
                )
            })?;
        }
    }
    for o in &mut outputs {
        let def = db
            .stored_definition(o.def)
            .await?
            .ok_or_else(|| anyhow!("ENVIS definition {} cannot be read", o.def))?;
        o.props = def.fields.props;
    }
    if any_empty && resume.is_some() {
        tracing::info!("some feeders have no rows yet; they start where the others are");
    }
    Ok((
        Sink::Feeders {
            db,
            calendar: &slot.calendar,
            meas: main,
            outputs,
            layout: lb,
            time_at,
            period,
        },
        resume,
    ))
}

impl Sink<'_> {
    async fn store(
        &mut self,
        cx: &RunContext<'_>,
        archive: u8,
        records: Vec<NewRecord>,
        rejected: Vec<Rejected>,
    ) -> Result<kmb_store::IngestReport> {
        match self {
            Self::Local {
                measurement,
                definition,
            } => {
                let batch = Batch {
                    measurement: *measurement,
                    archive,
                    definition: *definition,
                    records,
                    rejected,
                    run: Some(cx.run),
                };
                let now = clock::now_ms();
                Ok(cx.store.call(move |s| s.ingest(&batch, now)).await?)
            }
            Self::Feeders {
                db,
                calendar,
                meas,
                outputs,
                layout,
                time_at,
                period,
            } => {
                let target = FeederTarget {
                    calendar,
                    meas: *meas,
                    outputs,
                    layout,
                    time_at: *time_at,
                    period: *period,
                };
                let report = store_feeders(db, &target, archive, &records, &rejected).await?;
                let (run, r) = (cx.run, report);
                cx.store.call(move |s| s.add_run_counts(run, &r)).await?;
                Ok(report)
            }
            Self::Envis {
                db,
                calendar,
                meas,
                def,
                period,
                remove,
                row_len,
            } => {
                let target = EnvisTarget {
                    calendar,
                    meas: *meas,
                    def: *def,
                    period: *period,
                    remove: *remove,
                    row_len: *row_len,
                };
                let report = store_envis(db, &target, archive, &records, &rejected).await?;
                let (run, r) = (cx.run, report);
                cx.store.call(move |s| s.add_run_counts(run, &r)).await?;
                Ok(report)
            }
        }
    }
}

struct EnvisTarget<'a> {
    calendar: &'a kmb_envis::pack::Calendar,
    meas: i32,
    def: i32,
    period: u64,
    remove: usize,
    row_len: usize,
}

struct FeederTarget<'a> {
    calendar: &'a kmb_envis::pack::Calendar,
    meas: i32,
    outputs: &'a [FeederOut],
    layout: &'a kmb_envis::emi::LbLayout,
    time_at: usize,
    period: u64,
}

type ReadSlots = (Option<Range<u32>>, Vec<(u32, u64)>, Vec<(u32, u8)>);

fn read_slots(records: &[NewRecord], rejected: &[Rejected]) -> ReadSlots {
    let good: Vec<(u32, u64)> = records.iter().map(|r| (r.slot, r.time)).collect();
    let bad: Vec<(u32, u8)> = rejected
        .iter()
        .map(|r| (r.slot, envis_reason(&r.reason)))
        .collect();
    let read = good
        .iter()
        .map(|g| g.0)
        .chain(bad.iter().map(|b| b.0))
        .fold(None, |acc: Option<(u32, u32)>, s| {
            Some(acc.map_or((s, s), |(a, b)| (a.min(s), b.max(s))))
        })
        .map(|(first, last)| first..last.saturating_add(1));
    (read, good, bad)
}

async fn record_dstate(
    db: &mut kmb_envis::db::Envis,
    meas: i32,
    archive: u8,
    records: &[NewRecord],
    rejected: &[Rejected],
) -> Result<()> {
    let (read, good, bad) = read_slots(records, rejected);
    if let Some(range) = read {
        let now = kmb_envis::pack::from_timestamp(jiff::Timestamp::now());
        let runs = kmb_envis::db::bad_runs(range.clone(), &good, &bad, now);
        db.update_dstate(meas, archive, range, &runs).await?;
    }
    Ok(())
}

async fn store_envis(
    db: &mut kmb_envis::db::Envis,
    t: &EnvisTarget<'_>,
    archive: u8,
    records: &[NewRecord],
    rejected: &[Rejected],
) -> Result<kmb_store::IngestReport> {
    let mut a = kmb_envis::append::Appender::begin(
        db, t.calendar, t.meas, archive, t.def, t.period, t.row_len,
    )
    .await?;
    for r in records {
        let row = r.bytes.get(t.remove..).unwrap_or_default();
        if let Err(e) = a.add(row).await {
            a.rollback().await?;
            return Err(e.into());
        }
    }
    let report = a.commit().await?;
    record_dstate(db, t.meas, archive, records, rejected).await?;
    if report.refused > 0 {
        tracing::warn!(
            archive,
            refused = report.refused,
            "rows fall in an ENVIS pack of another definition or period"
        );
    }
    Ok(kmb_store::IngestReport {
        inserted: report.inserted,
        duplicates: report.duplicates,
        rejected: rejected.len().saturating_add(report.refused),
        cursor: None,
    })
}

async fn store_feeders(
    db: &mut kmb_envis::db::Envis,
    t: &FeederTarget<'_>,
    archive: u8,
    records: &[NewRecord],
    rejected: &[Rejected],
) -> Result<kmb_store::IngestReport> {
    let mut total = kmb_envis::append::Report::default();
    for o in t.outputs {
        let row_len = 8_usize.saturating_add(o.props.len().saturating_mul(4));
        let mut a = kmb_envis::append::Appender::begin(
            db, t.calendar, o.meas, o.archive, o.def, t.period, row_len,
        )
        .await?;
        for r in records {
            let Some(row) = t.layout.row(&r.bytes, t.time_at, o.slot, &o.props) else {
                a.rollback().await?;
                bail!(
                    "feeder {}: the LB record has no field for its definition",
                    o.name
                );
            };
            if let Err(e) = a.add(&row).await {
                a.rollback().await?;
                return Err(e.into());
            }
        }
        let r = a.commit().await?;
        total.inserted = total.inserted.saturating_add(r.inserted);
        total.duplicates = total.duplicates.saturating_add(r.duplicates);
        total.refused = total.refused.saturating_add(r.refused);
    }
    record_dstate(db, t.meas, archive, records, rejected).await?;
    Ok(kmb_store::IngestReport {
        inserted: total.inserted,
        duplicates: total.duplicates,
        rejected: rejected.len().saturating_add(total.refused),
        cursor: None,
    })
}

fn envis_reason(reason: &str) -> u8 {
    match reason {
        "missing" => 1,
        "time" => 10,
        "time-zero" => Reject::TimeZero.reason(),
        "time-invalid" => Reject::TimeInvalid.reason(),
        "time-after-2100" => Reject::TimeAfter2100.reason(),
        "length" => Reject::Length.reason(),
        _ => Reject::Sum.reason(),
    }
}

async fn values_job(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    described: &mut Described,
    measurement: MeasurementId,
    set: DataSet,
) -> Result<Summary> {
    let desc = if let Some(d) = described.sets.get_mut(&set) {
        client.read_values(set, d).await.context("reading values")?;
        d
    } else {
        let d = client
            .read_description(set)
            .await
            .context("reading value set")?;
        described.sets.entry(set).or_insert(d)
    };
    let body = values_json(desc);
    let time = clock::to_kmb(clock::now_ms()).0;
    let inserted = cx
        .store
        .call(move |s| s.store_values(measurement, time, &body))
        .await?;
    Ok(Summary {
        inserted: usize::from(inserted),
        ..Summary::default()
    })
}

#[must_use]
pub fn values_json(d: &Description) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (field, value) in d.fields.iter().zip(&d.values) {
        let group = d
            .groups
            .iter()
            .find(|g| g.idx == field.ic)
            .map_or("?", |g| g.name.as_str());
        let v = value.as_ref().map_or(serde_json::Value::Null, |v| {
            kmb_store::definition::value_json(v, 1.0)
        });
        map.insert(format!("{group}/{}", field.name), v);
    }
    serde_json::Value::Object(map)
}

async fn clock_job(
    cx: &RunContext<'_>,
    client: &mut MeterClient,
    adjust_above: Option<std::time::Duration>,
) -> Result<Summary> {
    let before = clock::now_ms();
    let rtc = client.read_clock().await.context("reading clock")?;
    let after = clock::now_ms();
    let local = before.saturating_add(after.saturating_sub(before).div_euclid(2));
    let meter = rtc
        .time
        .to_unix_ms()
        .ok_or_else(|| anyhow!("meter clock is invalid"))?;
    let offset = meter.saturating_sub(local);
    cx.device.update(|s| s.clock_offset_ms = Some(offset));
    let mut out = Summary {
        notes: vec![format!("clock offset {offset} ms")],
        ..Summary::default()
    };
    if let Some(limit) = adjust_above
        && offset.unsigned_abs() > u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)
    {
        let sent = client.adjust_clock().await.context("adjusting clock")?;
        tracing::warn!(
            device = %cx.device.config.name,
            offset_ms = offset,
            set_to = %clock::kmb_rfc3339(sent),
            "meter clock adjusted"
        );
        out.notes
            .push(format!("clock set to {}", clock::kmb_rfc3339(sent)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limited_run_resumes_after_the_archive_it_stopped_at() {
        let list = vec![0, 25, 6, 3, 4];
        assert_eq!(resume_order(list.clone(), None), list);
        assert_eq!(resume_order(list.clone(), Some(25)), [6, 3, 4, 0, 25]);
        assert_eq!(resume_order(list.clone(), Some(4)), list);
        assert_eq!(resume_order(list.clone(), Some(9)), list);
    }

    #[test]
    fn only_sql_errors_drop_the_envis_connection() {
        let sql = anyhow::Error::from(kmb_envis::Error::Sql("timeout".into()));
        assert!(connection_broken(&sql.context("archive 0")));
        let none = anyhow::Error::from(kmb_envis::Error::NoDefinition(3));
        assert!(!connection_broken(&none));
        assert!(!connection_broken(&anyhow!("meter did not answer")));
    }

    #[test]
    fn ranges() {
        let mut read = Vec::new();
        push_range(&mut read, 5..10);
        push_range(&mut read, 10..12);
        push_range(&mut read, 0..3);
        assert_eq!(read, [5..12, 0..3]);
        assert_eq!(
            ranges_note(0, &read),
            "archive 0: read 10 slots 5..12, 0..3"
        );
        let many: Vec<_> = (0..7).map(|i| i * 10..i * 10 + 1).collect();
        assert!(ranges_note(6, &many).ends_with("40..41, 2 more"));
    }
}
