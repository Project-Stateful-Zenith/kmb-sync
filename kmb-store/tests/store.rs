#![cfg(test)]

use kmb::archive::uni::Layout;
use kmb::uni::ArchiveDescription;
use kmb_store::{Batch, Definition, Error, IdentityInfo, NewRecord, RecordQuery, Rejected, Store};

fn hex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

const ARCH_PHASE0: &str = "00 00 00 01 00 01  06 00 00 0000 3F800000 00 4D 61 69 6E 00  FF";
const ARCH_PHASE1: &str = "00 00 00 04 00 04
    4C 65 6E 67 74 68 00 06 00 00 00 00 01 0001 01
    53 75 6D 00         06 00 00 00 00 01 0001 01
    54 69 6D 65 00      06 00 00 00 00 13 0001 01
    55 31 00            06 00 00 00 00 04 0001 01";
const RECORD: &str = "0010 0E9A 000000B0A237A5C0 43660000";
const RECORD_TIME: u64 = 0xB0_A237_A5C0;

fn definition() -> Definition {
    let mut desc = ArchiveDescription::default();
    desc.push_groups(&hex(ARCH_PHASE0)).unwrap();
    desc.push_fields(&hex(ARCH_PHASE1)).unwrap();
    let layout = Layout::new(&desc, 0, 1, 15000).unwrap();
    Definition::from_layout(&layout, &desc)
}

fn identity() -> IdentityInfo {
    IdentityInfo {
        device_no: 12345,
        device_type: 0x9400,
        props_type: 48,
        software_version: 0x18D1,
        hardware_version: 2,
        software_modules: 0xA0,
        bootloader_version: 0x13,
        firmware: "4.27.6353".into(),
    }
}

fn path() -> Vec<String> {
    vec!["Example".into(), "Station A".into()]
}

fn setup(store: &mut Store) -> (i64, i64) {
    let id = store
        .upsert_identity(&path(), &identity(), "tcp://192.0.2.10:2101/1", 1000)
        .unwrap();
    let m = store.measurement(id, "default").unwrap();
    let d = store.definition(&definition()).unwrap();
    (m, d)
}

fn record(slot: u32, time: u64, bytes: &[u8]) -> NewRecord {
    NewRecord {
        slot,
        time,
        bytes: bytes.to_vec(),
    }
}

#[test]
fn definition_round_trip_and_decode() {
    let def = definition();
    assert_eq!(def.record_len, 16);
    let layout = def.layout().unwrap();
    assert_eq!(layout.record_len, 16);
    let fields = def.decode(&hex(RECORD)).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "U1");
    assert_eq!(fields[0].value, serde_json::json!(230.0));
    let back: Definition = serde_json::from_str(&def.canonical()).unwrap();
    assert_eq!(back, def);
}

#[test]
fn definitions_dedupe_on_full_text() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.definition(&definition()).unwrap();
    let b = s.definition(&definition()).unwrap();
    assert_eq!(a, b);
    let mut other = definition();
    other.groups[0].unit = "V".into();
    let c = s.definition(&other).unwrap();
    assert_ne!(a, c);
    assert_eq!(s.load_definition(c).unwrap(), other);
}

#[test]
fn identity_is_found_again_and_updated() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s
        .upsert_identity(&path(), &identity(), "tcp://192.0.2.10:2101/1", 1)
        .unwrap();
    let mut newer = identity();
    newer.software_version = 0x18D2;
    let b = s
        .upsert_identity(&path(), &newer, "tcp://192.0.2.11:2101/1", 2)
        .unwrap();
    assert_eq!(a, b);
    let other_object = s
        .upsert_identity(&["Elsewhere".into()], &identity(), "x", 3)
        .unwrap();
    assert_ne!(a, other_object);
}

#[test]
fn ingest_is_idempotent_and_moves_the_cursor() {
    let mut s = Store::open_in_memory().unwrap();
    let (m, d) = setup(&mut s);
    let rec = hex(RECORD);
    let mut later = rec.clone();
    later[11] = 0xC1;
    let batch = Batch {
        measurement: m,
        archive: 0,
        definition: d,
        records: vec![
            record(0, RECORD_TIME, &rec),
            record(1, RECORD_TIME + 1, &later),
        ],
        rejected: vec![Rejected {
            slot: 2,
            reason: "sum".into(),
        }],
        run: None,
    };
    let first = s.ingest(&batch, 10).unwrap();
    assert_eq!(
        (first.inserted, first.duplicates, first.rejected),
        (2, 0, 1)
    );
    assert_eq!(first.cursor, Some(RECORD_TIME + 1));
    let again = s.ingest(&batch, 11).unwrap();
    assert_eq!((again.inserted, again.duplicates), (0, 2));
    assert_eq!(s.record_count(m, 0).unwrap(), 2);
    let c = s.cursor(m, 0).unwrap().unwrap();
    assert_eq!((c.last_time, c.last_slot), (RECORD_TIME + 1, 1));
    let states = s.download_states(m, 0, 10).unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!((states[0].slot_from, states[0].slot_to), (2, 2));
}

#[test]
fn same_time_other_bytes_is_kept() {
    let mut s = Store::open_in_memory().unwrap();
    let (m, d) = setup(&mut s);
    let rec = hex(RECORD);
    let mut other = rec.clone();
    other[15] = 1;
    let batch = Batch {
        measurement: m,
        archive: 0,
        definition: d,
        records: vec![record(0, RECORD_TIME, &rec), record(1, RECORD_TIME, &other)],
        rejected: vec![],
        run: None,
    };
    assert_eq!(s.ingest(&batch, 1).unwrap().inserted, 2);
}

#[test]
fn cursor_never_moves_back() {
    let mut s = Store::open_in_memory().unwrap();
    let (m, d) = setup(&mut s);
    let mk = |t: u64, slot: u32| Batch {
        measurement: m,
        archive: 0,
        definition: d,
        records: vec![record(slot, t, &t.to_be_bytes())],
        rejected: vec![],
        run: None,
    };
    s.ingest(&mk(500, 5), 1).unwrap();
    let r = s.ingest(&mk(100, 1), 2).unwrap();
    assert_eq!(r.cursor, Some(500));
}

#[test]
fn query_window_and_order() {
    let mut s = Store::open_in_memory().unwrap();
    let (m, d) = setup(&mut s);
    let records = (0..5u32)
        .map(|i| record(i, 1000 + u64::from(i), &[u8::try_from(i).unwrap(); 4]))
        .collect();
    let batch = Batch {
        measurement: m,
        archive: 0,
        definition: d,
        records,
        rejected: vec![],
        run: None,
    };
    s.ingest(&batch, 1).unwrap();
    let q = RecordQuery {
        measurement: m,
        archive: 0,
        from: Some(1001),
        to: Some(1004),
        limit: 10,
        newest_first: false,
    };
    let got: Vec<u64> = s.records(&q).unwrap().iter().map(|r| r.time).collect();
    assert_eq!(got, [1001, 1002, 1003]);
    let q = RecordQuery {
        newest_first: true,
        limit: 2,
        ..q
    };
    let got: Vec<u64> = s.records(&q).unwrap().iter().map(|r| r.time).collect();
    assert_eq!(got, [1003, 1002]);
}

#[test]
fn runs_and_heartbeat() {
    let mut s = Store::open_in_memory().unwrap();
    assert_eq!(s.heartbeat().unwrap(), None);
    s.beat(100, false).unwrap();
    let hb = s.heartbeat().unwrap().unwrap();
    assert_eq!((hb.last_beat, hb.clean_shutdown), (100, false));
    let r = s.begin_run("ST305-main", "ST305", 5).unwrap();
    let open = s.begin_run("ST305-main", "ST305", 6).unwrap();
    s.finish_run(r, "ok", None, kmb_store::Traffic { rx: 900, tx: 40 }, 7)
        .unwrap();
    assert_eq!(s.close_stale_runs(8).unwrap(), 1);
    let runs = s.runs(Some("ST305-main"), 10).unwrap();
    assert_eq!(runs[0].id, open);
    assert_eq!(runs[0].outcome.as_deref(), Some("interrupted"));
    assert_eq!(runs[1].outcome.as_deref(), Some("ok"));
    assert_eq!((runs[1].bytes_rx, runs[1].bytes_tx), (900, 40));
    let t = s.traffic(Some("ST305-main"), None).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(
        (t[0].day.as_str(), t[0].runs, t[0].bytes_rx),
        ("1970-01-01", 2, 900)
    );
    assert_eq!(s.traffic(None, Some(10)).unwrap(), []);
}

#[test]
fn reopen_keeps_data_and_refuses_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("online.db");
    {
        let mut s = Store::open(&file).unwrap();
        setup(&mut s);
    }
    let s = Store::open(&file).unwrap();
    assert_eq!(s.measurements().unwrap()[0].path, path());
    drop(s);
    rusqlite_set_version(&file, 99);
    assert!(matches!(
        Store::open(&file),
        Err(Error::SchemaTooNew { found: 99 })
    ));
}

fn rusqlite_set_version(file: &std::path::Path, v: i64) {
    let conn = rusqlite::Connection::open(file).unwrap();
    conn.pragma_update(None, "user_version", v).unwrap();
}

#[test]
fn version_1_gains_run_traffic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("online.db");
    {
        let mut s = Store::open(&file).unwrap();
        s.begin_run("j", "d", 1).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch(
            "ALTER TABLE run DROP COLUMN bytes_rx; ALTER TABLE run DROP COLUMN bytes_tx;
             DROP INDEX run_started; DROP TABLE run_log; PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let mut s = Store::open(&file).unwrap();
    let runs = s.runs(None, 10).unwrap();
    assert_eq!((runs[0].bytes_rx, runs[0].bytes_tx), (0, 0));
    s.finish_run(
        runs[0].id,
        "ok",
        None,
        kmb_store::Traffic { rx: 5, tx: 6 },
        2,
    )
    .unwrap();
    assert_eq!(s.runs(None, 1).unwrap()[0].bytes_tx, 6);
}

#[test]
fn version_2_gains_the_run_start_index() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("online.db");
    drop(Store::open(&file).unwrap());
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch("DROP INDEX run_started; DROP TABLE run_log; PRAGMA user_version = 2;")
            .unwrap();
    }
    drop(Store::open(&file).unwrap());
    let conn = rusqlite::Connection::open(&file).unwrap();
    let (version, index): (i64, i64) = (
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap(),
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'run_started'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    assert_eq!((version, index), (kmb_store::SCHEMA_VERSION, 1));
}

#[test]
fn run_log_keeps_two_periods() {
    const YEAR: i64 = 365 * 86_400_000;
    let mut s = Store::open_in_memory().unwrap();
    let job_runs = |s: &Store| -> Vec<i64> {
        let mut t: Vec<i64> = s
            .runs(None, 100)
            .unwrap()
            .iter()
            .map(|r| r.started_at)
            .collect();
        t.sort_unstable();
        t
    };
    for t in [1000, 1000 + YEAR / 2] {
        s.begin_run("j", "d", t).unwrap();
    }
    // The first period starts at the oldest run.
    let p = s.rotate_runs(1000 + YEAR - 1, YEAR).unwrap();
    assert_eq!((p.start, p.deleted), (1000, 0));
    // After one period nothing is old enough to go: the previous period is kept.
    s.begin_run("j", "d", 1000 + YEAR + 5).unwrap();
    let p = s.rotate_runs(1000 + YEAR + 10, YEAR).unwrap();
    assert_eq!((p.start, p.deleted), (1000 + YEAR, 0));
    assert_eq!(job_runs(&s).len(), 3);
    // After the second period the oldest period goes.
    let p = s.rotate_runs(1000 + 2 * YEAR, YEAR).unwrap();
    assert_eq!((p.start, p.deleted), (1000 + 2 * YEAR, 2));
    assert_eq!(job_runs(&s), [1000 + YEAR + 5]);
    // Calling again in the same period changes nothing.
    let p = s.rotate_runs(1000 + 2 * YEAR + 1, YEAR).unwrap();
    assert_eq!((p.start, p.deleted), (1000 + 2 * YEAR, 0));
}

#[test]
fn run_log_catches_up_after_a_long_stop() {
    const YEAR: i64 = 365 * 86_400_000;
    let mut s = Store::open_in_memory().unwrap();
    s.begin_run("j", "d", 0).unwrap();
    assert_eq!(s.rotate_runs(10, YEAR).unwrap().start, 0);
    s.begin_run("j", "d", 3 * YEAR + 1).unwrap();
    // Three periods passed while stopped: periods stay on their grid and
    // everything before the previous one goes.
    let p = s.rotate_runs(3 * YEAR + 2, YEAR).unwrap();
    assert_eq!((p.start, p.deleted), (3 * YEAR, 1));
}

#[test]
fn run_log_starts_now_without_runs() {
    let mut s = Store::open_in_memory().unwrap();
    assert_eq!(s.rotate_runs(5000, 1000).unwrap().start, 5000);
}

#[test]
fn version_3_gains_the_run_log() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("online.db");
    {
        let mut s = Store::open(&file).unwrap();
        s.begin_run("j", "d", 7).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch("DROP TABLE run_log; PRAGMA user_version = 3;")
            .unwrap();
    }
    let mut s = Store::open(&file).unwrap();
    assert_eq!(s.rotate_runs(100, 1000).unwrap().start, 7);
    assert_eq!(s.runs(None, 10).unwrap().len(), 1);
}
