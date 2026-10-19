pub mod definition;

pub use definition::{DecodedField, Definition, DefinitionError};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 4;

const MIGRATE_1_TO_2: &str = "
ALTER TABLE run ADD COLUMN bytes_rx INTEGER NOT NULL DEFAULT 0;
ALTER TABLE run ADD COLUMN bytes_tx INTEGER NOT NULL DEFAULT 0;
";
const MIGRATE_2_TO_3: &str = "
CREATE INDEX run_started ON run (started_at);
";
const MIGRATE_3_TO_4: &str = "
CREATE TABLE run_log (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    period_start INTEGER NOT NULL
);
";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than {SCHEMA_VERSION}")]
    SchemaTooNew { found: i64 },
    #[error(transparent)]
    Definition(#[from] DefinitionError),
    #[error("stored definition {0} is corrupt")]
    CorruptDefinition(i64),
    #[error("database schema version {found} is unknown")]
    UnknownSchema { found: i64 },
    #[error("{0} {1} does not exist")]
    NotFound(&'static str, i64),
}

pub type Result<T> = std::result::Result<T, Error>;

pub type ObjectId = i64;
pub type IdentityId = i64;
pub type MeasurementId = i64;
pub type DefinitionId = i64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IdentityInfo {
    pub device_no: u32,
    pub device_type: u16,
    pub props_type: u16,
    pub software_version: u16,
    pub hardware_version: u16,
    pub software_modules: u16,
    pub bootloader_version: u8,
    pub firmware: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRecord {
    pub slot: u32,
    pub time: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub slot: u32,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct Batch {
    pub measurement: MeasurementId,
    pub archive: u8,
    pub definition: DefinitionId,
    pub records: Vec<NewRecord>,
    pub rejected: Vec<Rejected>,
    pub run: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IngestReport {
    pub inserted: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub cursor: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Cursor {
    pub last_time: u64,
    pub last_slot: u32,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StoredRecord {
    pub time: u64,
    pub slot: u32,
    pub definition: DefinitionId,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordQuery {
    pub measurement: MeasurementId,
    pub archive: u8,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub limit: usize,
    pub newest_first: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MeasurementInfo {
    pub id: MeasurementId,
    pub name: String,
    pub path: Vec<String>,
    pub identity: IdentityId,
    pub device_no: u32,
    pub archives: Vec<(u8, Cursor)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DownloadState {
    pub archive: u8,
    pub slot_from: u32,
    pub slot_to: u32,
    pub reason: String,
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: i64,
    pub job: String,
    pub device: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub outcome: Option<String>,
    pub inserted: i64,
    pub duplicates: i64,
    pub rejected: i64,
    pub detail: Option<String>,
    pub bytes_rx: i64,
    pub bytes_tx: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Traffic {
    pub rx: u64,
    pub tx: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceDay {
    pub day: String,
    #[serde(skip)]
    pub device: String,
    pub runs: i64,
    pub bytes: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrafficDay {
    pub day: String,
    pub job: String,
    pub runs: i64,
    pub bytes_rx: i64,
    pub bytes_tx: i64,
    pub inserted: i64,
}

/// State of the run log after [`Store::rotate_runs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunLogPeriod {
    /// Start of the current period, Unix milliseconds.
    pub start: i64,
    /// Runs deleted by this call.
    pub deleted: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    pub last_beat: i64,
    pub clean_shutdown: bool,
}

const SCHEMA: &str = "
CREATE TABLE object (
    id        INTEGER PRIMARY KEY,
    parent_id INTEGER NOT NULL DEFAULT 0,
    name      TEXT NOT NULL,
    UNIQUE (parent_id, name)
);
CREATE TABLE identity (
    id                 INTEGER PRIMARY KEY,
    object_id          INTEGER NOT NULL REFERENCES object(id),
    device_no          INTEGER NOT NULL,
    device_type        INTEGER NOT NULL,
    props_type         INTEGER NOT NULL,
    software_version   INTEGER NOT NULL,
    hardware_version   INTEGER NOT NULL,
    software_modules   INTEGER NOT NULL,
    bootloader_version INTEGER NOT NULL,
    firmware           TEXT NOT NULL,
    first_seen         INTEGER NOT NULL,
    last_seen          INTEGER NOT NULL,
    UNIQUE (object_id, device_no, device_type, props_type)
);
CREATE TABLE device_url (
    url         TEXT NOT NULL,
    identity_id INTEGER NOT NULL REFERENCES identity(id),
    last_seen   INTEGER NOT NULL,
    PRIMARY KEY (url, identity_id)
);
CREATE TABLE measurement (
    id          INTEGER PRIMARY KEY,
    identity_id INTEGER NOT NULL REFERENCES identity(id),
    name        TEXT NOT NULL,
    UNIQUE (identity_id, name)
);
CREATE TABLE archive_definition (
    id         INTEGER PRIMARY KEY,
    crc32      INTEGER NOT NULL,
    archive    INTEGER NOT NULL,
    record_len INTEGER NOT NULL,
    body       TEXT NOT NULL
);
CREATE INDEX archive_definition_crc ON archive_definition (crc32);
CREATE TABLE record (
    measurement_id INTEGER NOT NULL REFERENCES measurement(id),
    archive        INTEGER NOT NULL,
    time           INTEGER NOT NULL,
    digest         BLOB NOT NULL,
    slot           INTEGER NOT NULL,
    definition_id  INTEGER NOT NULL REFERENCES archive_definition(id),
    bytes          BLOB NOT NULL,
    inserted_at    INTEGER NOT NULL,
    PRIMARY KEY (measurement_id, archive, time, digest)
) WITHOUT ROWID;
CREATE TABLE cursor (
    measurement_id INTEGER NOT NULL REFERENCES measurement(id),
    archive        INTEGER NOT NULL,
    last_time      INTEGER NOT NULL,
    last_slot      INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (measurement_id, archive)
);
CREATE TABLE download_state (
    id             INTEGER PRIMARY KEY,
    measurement_id INTEGER NOT NULL REFERENCES measurement(id),
    archive        INTEGER NOT NULL,
    slot_from      INTEGER NOT NULL,
    slot_to        INTEGER NOT NULL,
    reason         TEXT NOT NULL,
    run_id         INTEGER,
    at             INTEGER NOT NULL
);
CREATE INDEX download_state_measurement ON download_state (measurement_id, archive, at);
CREATE TABLE actual_values (
    measurement_id INTEGER NOT NULL REFERENCES measurement(id),
    time           INTEGER NOT NULL,
    body           TEXT NOT NULL,
    PRIMARY KEY (measurement_id, time)
);
CREATE TABLE run (
    id          INTEGER PRIMARY KEY,
    job         TEXT NOT NULL,
    device      TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    outcome     TEXT,
    inserted    INTEGER NOT NULL DEFAULT 0,
    duplicates  INTEGER NOT NULL DEFAULT 0,
    rejected    INTEGER NOT NULL DEFAULT 0,
    detail      TEXT,
    bytes_rx    INTEGER NOT NULL DEFAULT 0,
    bytes_tx    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX run_job ON run (job, started_at);
CREATE INDEX run_started ON run (started_at);
CREATE TABLE heartbeat (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    last_beat      INTEGER NOT NULL,
    clean_shutdown INTEGER NOT NULL
);
CREATE TABLE run_log (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    period_start INTEGER NOT NULL
);
";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match found {
            0 => {
                conn.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            1 => {
                conn.execute_batch(&format!(
                    "BEGIN; {MIGRATE_1_TO_2} {MIGRATE_2_TO_3} {MIGRATE_3_TO_4} \
                     PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            2 => {
                conn.execute_batch(&format!(
                    "BEGIN; {MIGRATE_2_TO_3} {MIGRATE_3_TO_4} \
                     PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            3 => {
                conn.execute_batch(&format!(
                    "BEGIN; {MIGRATE_3_TO_4} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {}
            found if found > SCHEMA_VERSION => return Err(Error::SchemaTooNew { found }),
            found => return Err(Error::UnknownSchema { found }),
        }
        Ok(Self { conn })
    }

    pub fn upsert_identity(
        &mut self,
        path: &[String],
        info: &IdentityInfo,
        url: &str,
        now: i64,
    ) -> Result<IdentityId> {
        let tx = self.conn.transaction()?;
        let object = object_in(&tx, path)?;
        tx.execute(
            "INSERT INTO identity (object_id, device_no, device_type, props_type,
                 software_version, hardware_version, software_modules, bootloader_version,
                 firmware, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
             ON CONFLICT (object_id, device_no, device_type, props_type) DO UPDATE SET
                 software_version = excluded.software_version,
                 hardware_version = excluded.hardware_version,
                 software_modules = excluded.software_modules,
                 bootloader_version = excluded.bootloader_version,
                 firmware = excluded.firmware,
                 last_seen = excluded.last_seen",
            params![
                object,
                info.device_no,
                info.device_type,
                info.props_type,
                info.software_version,
                info.hardware_version,
                info.software_modules,
                info.bootloader_version,
                info.firmware,
                now
            ],
        )?;
        let id: IdentityId = tx.query_row(
            "SELECT id FROM identity
             WHERE object_id = ?1 AND device_no = ?2 AND device_type = ?3 AND props_type = ?4",
            params![object, info.device_no, info.device_type, info.props_type],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO device_url (url, identity_id, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT (url, identity_id) DO UPDATE SET last_seen = excluded.last_seen",
            params![url, id, now],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn measurement(&mut self, identity: IdentityId, name: &str) -> Result<MeasurementId> {
        self.conn.execute(
            "INSERT INTO measurement (identity_id, name) VALUES (?1, ?2)
             ON CONFLICT (identity_id, name) DO NOTHING",
            params![identity, name],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM measurement WHERE identity_id = ?1 AND name = ?2",
            params![identity, name],
            |r| r.get(0),
        )?)
    }

    pub fn definition(&mut self, def: &Definition) -> Result<DefinitionId> {
        let body = def.canonical();
        let crc = def.crc32();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: Option<DefinitionId> = tx
            .query_row(
                "SELECT id FROM archive_definition WHERE crc32 = ?1 AND body = ?2",
                params![crc, body],
                |r| r.get(0),
            )
            .optional()?;
        let id = if let Some(id) = found {
            id
        } else {
            tx.execute(
                "INSERT INTO archive_definition (crc32, archive, record_len, body)
                 VALUES (?1, ?2, ?3, ?4)",
                params![crc, def.archive, count(def.record_len), body],
            )?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn load_definition(&self, id: DefinitionId) -> Result<Definition> {
        let body: String = self
            .conn
            .query_row(
                "SELECT body FROM archive_definition WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound("definition", id))?;
        serde_json::from_str(&body).map_err(|_| Error::CorruptDefinition(id))
    }

    pub fn ingest(&mut self, batch: &Batch, now: i64) -> Result<IngestReport> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = IngestReport::default();
        let mut newest: Option<(u64, u32)> = None;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO record (measurement_id, archive, time, digest, slot,
                     definition_id, bytes, inserted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO NOTHING",
            )?;
            for r in &batch.records {
                let digest = Sha256::digest(&r.bytes);
                let n = insert.execute(params![
                    batch.measurement,
                    batch.archive,
                    sql_time(r.time),
                    &digest[..],
                    r.slot,
                    batch.definition,
                    r.bytes,
                    now
                ])?;
                if n == 1 {
                    report.inserted = report.inserted.saturating_add(1);
                } else {
                    report.duplicates = report.duplicates.saturating_add(1);
                }
                if newest.is_none_or(|(t, _)| r.time >= t) {
                    newest = Some((r.time, r.slot));
                }
            }
            let mut state = tx.prepare_cached(
                "INSERT INTO download_state (measurement_id, archive, slot_from, slot_to,
                     reason, run_id, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (from, to, reason) in coalesce(&batch.rejected) {
                state.execute(params![
                    batch.measurement,
                    batch.archive,
                    from,
                    to,
                    reason,
                    batch.run,
                    now
                ])?;
            }
            report.rejected = batch.rejected.len();
        }
        if let Some((time, slot)) = newest {
            tx.execute(
                "INSERT INTO cursor (measurement_id, archive, last_time, last_slot, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (measurement_id, archive) DO UPDATE SET
                     last_time = excluded.last_time,
                     last_slot = excluded.last_slot,
                     updated_at = excluded.updated_at
                 WHERE excluded.last_time >= cursor.last_time",
                params![batch.measurement, batch.archive, sql_time(time), slot, now],
            )?;
        }
        if let Some(run) = batch.run {
            tx.execute(
                "UPDATE run SET inserted = inserted + ?2, duplicates = duplicates + ?3,
                     rejected = rejected + ?4
                 WHERE id = ?1",
                params![
                    run,
                    count(report.inserted),
                    count(report.duplicates),
                    count(report.rejected)
                ],
            )?;
        }
        report.cursor = cursor_in(&tx, batch.measurement, batch.archive)?.map(|c| c.last_time);
        tx.commit()?;
        Ok(report)
    }

    pub fn cursor(&self, measurement: MeasurementId, archive: u8) -> Result<Option<Cursor>> {
        cursor_in(&self.conn, measurement, archive)
    }

    pub fn store_values(
        &mut self,
        measurement: MeasurementId,
        time: u64,
        body: &serde_json::Value,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO actual_values (measurement_id, time, body) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
            params![measurement, sql_time(time), body.to_string()],
        )?;
        Ok(n == 1)
    }

    pub fn latest_values(
        &self,
        measurement: MeasurementId,
        limit: usize,
    ) -> Result<Vec<(u64, serde_json::Value)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT time, body FROM actual_values WHERE measurement_id = ?1
             ORDER BY time DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![measurement, count(limit)], |r| {
            Ok((from_sql(r.get(0)?), r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (t, body) = row?;
            out.push((
                t,
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
            ));
        }
        Ok(out)
    }

    pub fn records(&self, q: &RecordQuery) -> Result<Vec<StoredRecord>> {
        let order = if q.newest_first { "DESC" } else { "ASC" };
        let mut st = self.conn.prepare(&format!(
            "SELECT time, slot, definition_id, bytes FROM record
             WHERE measurement_id = ?1 AND archive = ?2 AND time >= ?3 AND time < ?4
             ORDER BY time {order}, slot {order} LIMIT ?5"
        ))?;
        let rows = st.query_map(
            params![
                q.measurement,
                q.archive,
                sql_time(q.from.unwrap_or(0)),
                sql_time(q.to.unwrap_or(u64::MAX)),
                count(q.limit)
            ],
            |r| {
                Ok(StoredRecord {
                    time: from_sql(r.get(0)?),
                    slot: r.get(1)?,
                    definition: r.get(2)?,
                    bytes: r.get(3)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn record_count(&self, measurement: MeasurementId, archive: u8) -> Result<u64> {
        Ok(self
            .conn
            .query_row(
                "SELECT count(*) FROM record WHERE measurement_id = ?1 AND archive = ?2",
                params![measurement, archive],
                |r| r.get::<_, i64>(0),
            )
            .map(from_sql)?)
    }

    pub fn measurements(&self) -> Result<Vec<MeasurementInfo>> {
        let mut st = self.conn.prepare(
            "SELECT m.id, m.name, i.id, i.object_id, i.device_no
             FROM measurement m JOIN identity i ON i.id = m.identity_id ORDER BY m.id",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, u32>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (id, name, identity, object, device_no) in rows {
            let mut cur = self.conn.prepare_cached(
                "SELECT archive, last_time, last_slot, updated_at FROM cursor
                 WHERE measurement_id = ?1 ORDER BY archive",
            )?;
            let archives = cur
                .query_map([id], |r| {
                    Ok((
                        r.get::<_, u8>(0)?,
                        Cursor {
                            last_time: from_sql(r.get(1)?),
                            last_slot: r.get(2)?,
                            updated_at: r.get(3)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.push(MeasurementInfo {
                id,
                name,
                path: self.path(object)?,
                identity,
                device_no,
                archives,
            });
        }
        Ok(out)
    }

    fn path(&self, mut object: ObjectId) -> Result<Vec<String>> {
        let mut path = Vec::new();
        while object != 0 {
            let (parent, name): (i64, String) = self.conn.query_row(
                "SELECT parent_id, name FROM object WHERE id = ?1",
                [object],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            path.push(name);
            object = parent;
        }
        path.reverse();
        Ok(path)
    }

    pub fn download_states(
        &self,
        measurement: MeasurementId,
        archive: u8,
        limit: usize,
    ) -> Result<Vec<DownloadState>> {
        let mut st = self.conn.prepare_cached(
            "SELECT archive, slot_from, slot_to, reason, at FROM download_state
             WHERE measurement_id = ?1 AND archive = ?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![measurement, archive, count(limit)], |r| {
            Ok(DownloadState {
                archive: r.get(0)?,
                slot_from: r.get(1)?,
                slot_to: r.get(2)?,
                reason: r.get(3)?,
                at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn begin_run(&mut self, job: &str, device: &str, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO run (job, device, started_at) VALUES (?1, ?2, ?3)",
            params![job, device, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_run(
        &mut self,
        run: i64,
        outcome: &str,
        detail: Option<&str>,
        traffic: Traffic,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE run SET finished_at = ?2, outcome = ?3, detail = ?4, bytes_rx = ?5,
                 bytes_tx = ?6
             WHERE id = ?1",
            params![
                run,
                now,
                outcome,
                detail,
                sql_time(traffic.rx),
                sql_time(traffic.tx)
            ],
        )?;
        Ok(())
    }

    /// Records the bytes a run has used so far, so they survive a crash.
    pub fn run_traffic(&mut self, run: i64, traffic: Traffic) -> Result<()> {
        self.conn.execute(
            "UPDATE run SET bytes_rx = ?2, bytes_tx = ?3 WHERE id = ?1",
            params![run, sql_time(traffic.rx), sql_time(traffic.tx)],
        )?;
        Ok(())
    }

    /// Bytes used by the runs of `job` that started after `since`.
    pub fn job_bytes_since(&self, job: &str, since: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(bytes_rx + bytes_tx), 0) FROM run
             WHERE job = ?1 AND started_at > ?2",
            params![job, since],
            |r| r.get(0),
        )?;
        Ok(from_sql(n))
    }

    pub fn add_run_counts(&mut self, run: i64, report: &IngestReport) -> Result<()> {
        self.conn.execute(
            "UPDATE run SET inserted = inserted + ?2, duplicates = duplicates + ?3,
                 rejected = rejected + ?4
             WHERE id = ?1",
            params![
                run,
                count(report.inserted),
                count(report.duplicates),
                count(report.rejected)
            ],
        )?;
        Ok(())
    }

    pub fn close_stale_runs(&mut self, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE run SET finished_at = ?1, outcome = 'interrupted' WHERE finished_at IS NULL",
            [now],
        )?)
    }

    pub fn runs(&self, job: Option<&str>, limit: usize) -> Result<Vec<Run>> {
        let mut st = self.conn.prepare_cached(if job.is_some() {
            "SELECT id, job, device, started_at, finished_at, outcome, inserted, duplicates,
                 rejected, detail, bytes_rx, bytes_tx
             FROM run WHERE job = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2"
        } else {
            "SELECT id, job, device, started_at, finished_at, outcome, inserted, duplicates,
                 rejected, detail, bytes_rx, bytes_tx
             FROM run WHERE ?1 IS NULL ORDER BY id DESC LIMIT ?2"
        })?;
        let rows = st.query_map(params![job, count(limit)], |r| {
            Ok(Run {
                id: r.get(0)?,
                job: r.get(1)?,
                device: r.get(2)?,
                started_at: r.get(3)?,
                finished_at: r.get(4)?,
                outcome: r.get(5)?,
                inserted: r.get(6)?,
                duplicates: r.get(7)?,
                rejected: r.get(8)?,
                detail: r.get(9)?,
                bytes_rx: r.get(10)?,
                bytes_tx: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn traffic(&self, job: Option<&str>, since: Option<i64>) -> Result<Vec<TrafficDay>> {
        let mut st = self.conn.prepare_cached(
            "SELECT date(started_at / 1000, 'unixepoch') AS day, job, COUNT(*), SUM(bytes_rx),
                 SUM(bytes_tx), SUM(inserted)
             FROM run WHERE (?1 IS NULL OR job = ?1) AND (?2 IS NULL OR started_at >= ?2)
             GROUP BY day, job ORDER BY day, job",
        )?;
        let rows = st.query_map(params![job, since], |r| {
            Ok(TrafficDay {
                day: r.get(0)?,
                job: r.get(1)?,
                runs: r.get(2)?,
                bytes_rx: r.get(3)?,
                bytes_tx: r.get(4)?,
                inserted: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn device_days(&self, since: i64) -> Result<Vec<DeviceDay>> {
        let mut st = self.conn.prepare_cached(
            "SELECT date(started_at / 1000, 'unixepoch', 'localtime') AS day, device, COUNT(*),
                 SUM(bytes_rx) + SUM(bytes_tx)
             FROM run WHERE started_at >= ?1
             GROUP BY day, device ORDER BY day, device",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok(DeviceDay {
                day: r.get(0)?,
                device: r.get(1)?,
                runs: r.get(2)?,
                bytes: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn heartbeat(&self) -> Result<Option<Heartbeat>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_beat, clean_shutdown FROM heartbeat WHERE id = 1",
                [],
                |r| {
                    Ok(Heartbeat {
                        last_beat: r.get(0)?,
                        clean_shutdown: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// Rotates the run log in periods of `period` milliseconds, keeping the
    /// current and the previous period. The first period starts at the
    /// oldest run, or at `now` when there is none. When a period ends, runs,
    /// download states and actual values from before the previous period are
    /// deleted. A `period` below 1 is taken as 1.
    pub fn rotate_runs(&mut self, now: i64, period: i64) -> Result<RunLogPeriod> {
        let period = period.max(1);
        let tx = self.conn.transaction()?;
        let stored: Option<i64> = tx
            .query_row("SELECT period_start FROM run_log WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        let start = match stored {
            Some(start) => start,
            None => tx.query_row("SELECT MIN(started_at) FROM run", [], |r| {
                Ok(r.get::<_, Option<i64>>(0)?.map_or(now, |t| t.min(now)))
            })?,
        };
        let ended = now.saturating_sub(start).checked_div(period).unwrap_or(0);
        let (start, deleted) = if ended > 0 {
            let start = start.saturating_add(ended.saturating_mul(period));
            let cut = start.saturating_sub(period);
            let deleted = tx.execute("DELETE FROM run WHERE started_at < ?1", [cut])?;
            tx.execute("DELETE FROM download_state WHERE at < ?1", [cut])?;
            tx.execute(
                "DELETE FROM actual_values WHERE time < ?1",
                [sql_time(kmb::KmbTime::from_unix_ms(cut).0)],
            )?;
            (start, deleted)
        } else {
            (start, 0)
        };
        if stored != Some(start) {
            tx.execute(
                "INSERT INTO run_log (id, period_start) VALUES (1, ?1)
                 ON CONFLICT (id) DO UPDATE SET period_start = ?1",
                params![start],
            )?;
        }
        tx.commit()?;
        Ok(RunLogPeriod { start, deleted })
    }

    pub fn beat(&mut self, now: i64, clean_shutdown: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO heartbeat (id, last_beat, clean_shutdown) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET last_beat = ?1, clean_shutdown = ?2",
            params![now, i64::from(clean_shutdown)],
        )?;
        Ok(())
    }
}

fn object_in(conn: &Connection, path: &[String]) -> Result<ObjectId> {
    let mut parent: ObjectId = 0;
    for name in path {
        conn.execute(
            "INSERT INTO object (parent_id, name) VALUES (?1, ?2)
             ON CONFLICT (parent_id, name) DO NOTHING",
            params![parent, name],
        )?;
        parent = conn.query_row(
            "SELECT id FROM object WHERE parent_id = ?1 AND name = ?2",
            params![parent, name],
            |r| r.get(0),
        )?;
    }
    Ok(parent)
}

fn cursor_in(conn: &Connection, measurement: MeasurementId, archive: u8) -> Result<Option<Cursor>> {
    Ok(conn
        .query_row(
            "SELECT last_time, last_slot, updated_at FROM cursor
             WHERE measurement_id = ?1 AND archive = ?2",
            params![measurement, archive],
            |r| {
                Ok(Cursor {
                    last_time: from_sql(r.get(0)?),
                    last_slot: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            },
        )
        .optional()?)
}

fn from_sql(t: i64) -> u64 {
    u64::try_from(t).unwrap_or(0)
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn sql_time(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(i64::MAX)
}

fn coalesce(rejected: &[Rejected]) -> Vec<(u32, u32, &str)> {
    let mut out: Vec<(u32, u32, &str)> = Vec::new();
    for r in rejected {
        match out.last_mut() {
            Some((_, to, reason)) if *reason == r.reason && to.checked_add(1) == Some(r.slot) => {
                *to = r.slot;
            }
            _ => out.push((r.slot, r.slot, &r.reason)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rej(slot: u32, reason: &str) -> Rejected {
        Rejected {
            slot,
            reason: reason.into(),
        }
    }

    #[test]
    fn coalesce_ranges() {
        let r = [
            rej(1, "sum"),
            rej(2, "sum"),
            rej(3, "ecc"),
            rej(5, "ecc"),
            rej(6, "ecc"),
        ];
        assert_eq!(coalesce(&r), [(1, 2, "sum"), (3, 3, "ecc"), (5, 6, "ecc")]);
    }
}
