#![allow(
    clippy::arithmetic_side_effects,
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::or_fun_call,
    clippy::double_ended_iterator_last
)]
#![cfg(test)]

use kmb_envis::append::{Appender, Report};
use kmb_envis::db::{Envis, IdentityRow, Target};
use kmb_envis::definition::{DataType, Prop, TypeDescription};
use kmb_envis::pack::{Calendar, decode_index, from_timestamp, row_time, time_index};
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

const SCHEMA: &str = include_str!("schema.sql");
const MIN: u64 = 60_000;

struct Server {
    host: String,
    port: u16,
    password: String,
}

fn server() -> Option<Server> {
    let v = std::env::var("KMB_ENVIS_TEST").ok()?;
    let mut parts = v.splitn(3, ':');
    let host = parts.next()?.to_string();
    let port = parts.next()?.parse().ok()?;
    let password = std::fs::read_to_string(parts.next()?)
        .ok()?
        .trim()
        .to_string();
    Some(Server {
        host,
        port,
        password,
    })
}

async fn raw(s: &Server, db: &str) -> Client<tokio_util::compat::Compat<TcpStream>> {
    let mut c = Config::new();
    c.host(&s.host);
    c.port(s.port);
    c.database(db);
    c.authentication(AuthMethod::sql_server("sa", &s.password));
    c.trust_cert();
    let tcp = TcpStream::connect(c.get_addr()).await.unwrap();
    Client::connect(c, tcp.compat_write()).await.unwrap()
}

struct TestDb {
    name: String,
    target: Target,
}

impl TestDb {
    async fn new(s: &Server, tag: &str) -> Self {
        let name = format!("kmbtest_{tag}_{}", std::process::id());
        let mut m = raw(s, "master").await;
        m.simple_query(format!(
            "IF DB_ID('{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}] END; CREATE DATABASE [{name}] COLLATE SQL_Latin1_General_CP1_CI_AS"
        ))
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
        let mut d = raw(s, &name).await;
        d.simple_query(SCHEMA)
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        Self {
            target: Target {
                host: s.host.clone(),
                port: s.port,
                database: name.clone(),
                user: "sa".into(),
                password: s.password.clone(),
                trust_cert: true,
            },
            name,
        }
    }

    async fn drop_db(self, s: &Server) {
        let mut m = raw(s, "master").await;
        m.simple_query(format!(
            "ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{0}]",
            self.name
        ))
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    }
}

fn identity() -> IdentityRow {
    IdentityRow {
        device_no: 12345,
        device_type: 37888,
        sub_device_type: [309, 10, 256, 20, 0, 512],
        props_type: 48,
        software_version: 6472,
        hardware_version: 1,
        software_modules: 160,
        device_addr: 1,
        bootloader_version: 48,
        archives: 0x200_0079,
    }
}

fn u1_definition() -> TypeDescription {
    TypeDescription {
        props: vec![Prop {
            prop_name: "U_avg_U1".into(),
            user_name: "U1".into(),
            unit: Some("V".into()),
            group: Some("U".into()),
            sub_group: Some("avg".into()),
            data_type: DataType::Value,
            big_endian: true,
            mul: 1.0,
            info: 0,
            info2: 1,
            len: 4,
            type_ser: "System.Single".into(),
            a_ind: None,
        }],
        virtuals: Vec::new(),
    }
}

fn row(t: u64, v: f32) -> Vec<u8> {
    let mut r = t.to_be_bytes().to_vec();
    r.extend(v.to_be_bytes());
    r
}

fn t(s: &str) -> u64 {
    from_timestamp(s.parse().unwrap())
}

#[derive(Debug)]
struct Pack {
    key: u64,
    end: u64,
    count: i64,
    def: i32,
    rows: Vec<Vec<u8>>,
    index: Vec<(u32, u64)>,
}

async fn packs(s: &Server, db: &str, meas: i32, arch: u8) -> Vec<Pack> {
    let mut c = raw(s, db).await;
    let rows = c
        .query(
            "SELECT keyTime, endTime, CAST(Count AS bigint), ArchDef, DataBuf, TimeIndexes
             FROM dbo.UniArchiveBinPack WHERE keymeasName = @P1 AND keyArchID = @P2 ORDER BY keyTime",
            &[&meas, &arch],
        )
        .await
        .unwrap()
        .into_first_result()
        .await
        .unwrap();
    let ms = |v: time::PrimitiveDateTime| {
        u64::try_from(v.assume_utc().unix_timestamp_nanos() / 1_000_000 - 946_684_800_000).unwrap()
    };
    rows.iter()
        .map(|r| {
            let buf: &[u8] = r.get(4).unwrap();
            Pack {
                key: ms(r.get(0).unwrap()),
                end: ms(r.get(1).unwrap()),
                count: r.get(2).unwrap(),
                def: r.get(3).unwrap(),
                rows: buf.chunks(12).map(<[u8]>::to_vec).collect(),
                index: r
                    .get::<&[u8], _>(5)
                    .map(decode_index)
                    .unwrap_or_default()
                    .iter()
                    .map(|e| (e.row, e.time))
                    .collect(),
            }
        })
        .collect()
}

fn check(p: &Pack, period: u64) {
    let times: Vec<u64> = p.rows.iter().map(|r| row_time(r).unwrap()).collect();
    assert_eq!(p.count, i64::try_from(times.len()).unwrap());
    assert_eq!(p.key, times[0]);
    assert_eq!(p.end, *times.last().unwrap());
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    let canonical: Vec<(u32, u64)> = time_index(&times, period, 12)
        .iter()
        .map(|e| (e.row, e.time))
        .collect();
    assert_eq!(p.index, canonical);
}

async fn write(db: &mut Envis, cal: &Calendar, meas: i32, def: i32, rows: &[Vec<u8>]) -> Report {
    write_period(db, cal, meas, def, MIN, rows).await
}

async fn write_period(
    db: &mut Envis,
    cal: &Calendar,
    meas: i32,
    def: i32,
    period: u64,
    rows: &[Vec<u8>],
) -> Report {
    let mut a = Appender::begin(db, cal, meas, 0, def, period, 12)
        .await
        .unwrap();
    for r in rows {
        a.add(r).await.unwrap();
    }
    a.commit().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn version_and_identity_rows() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "ident").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let url = "-PACKED:6800$-MODBUS:AUTO$tcp://192.0.2.1:2101/1/10000/10000";
    let m = db
        .measurement("Station A", "01 UMD-98 Station A", &identity(), url)
        .await
        .unwrap();
    let mut newer = identity();
    newer.software_version = 6500;
    newer.archives = 1 << 16;
    let again = db
        .measurement("Station A", "01 UMD-98 Station A", &newer, url)
        .await
        .unwrap();
    assert_eq!(m, again);
    let long = "An object name that is longer than thirty-two characters";
    let other = db
        .measurement(long, "DEFAULT", &identity(), url)
        .await
        .unwrap();
    assert_ne!(other.object, m.object);
    drop(db);
    let mut c = raw(&s, &tdb.name).await;
    let r = c
        .simple_query(
            "SELECT CAST(SoftwareVersion AS int), CAST(Archives AS bigint) FROM dbo.SmpIdentifyDB WHERE Id = 1;
             SELECT objekt, UserName FROM dbo.SmpObjectDB ORDER BY Id;
             SELECT COUNT(*) FROM dbo.SmpDeviceUrl",
        )
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert_eq!(r[0][0].get::<i32, _>(0), Some(6500));
    assert_eq!(r[0][0].get::<i64, _>(1), Some(0x200_0079 | (1 << 16)));
    assert_eq!(
        r[1][1].get::<&str, _>(0),
        Some("An object name that is longer th")
    );
    assert_eq!(
        r[1][1].get::<&str, _>(1),
        Some("An object name that is longer th")
    );
    assert_eq!(r[1][0].get::<&str, _>(1), None);
    assert_eq!(r[2][0].get::<i32, _>(0), Some(2));
    c.simple_query("UPDATE dbo.ENVDBUpdates SET Revision = 14000")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    assert!(Envis::connect(&tdb.target).await.is_err());
    drop(c);
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn definitions_are_reused_by_layout() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "defs").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let ours = u1_definition();
    assert!(db.definition_for(m.id, 0, &ours, false).await.is_err());
    let id = db.definition_for(m.id, 0, &ours, true).await.unwrap();
    let mut renamed = ours.clone();
    renamed.props[0].group = Some("Volt".into());
    renamed.props[0].prop_name = "Volt_avg_U1".into();
    assert_eq!(
        db.definition_for(m.id, 0, &renamed, false).await.unwrap(),
        id
    );
    drop(db);
    let mut c = raw(&s, &tdb.name).await;
    let r = c
        .simple_query("SELECT CAST(CRC AS bigint), Len FROM dbo.UniArchiveDefinition")
        .await
        .unwrap()
        .into_first_result()
        .await
        .unwrap();
    let xml = ours.to_xml();
    assert_eq!(
        r[0].get::<i64, _>(0),
        Some(i64::from(kmb_envis::definition::envis_crc(&xml)))
    );
    assert_eq!(r[0].get::<i32, _>(1), Some(0));
    drop(c);
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn continuous_packs() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "cont").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let cal = Calendar::new("UTC").unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let start = t("2026-12-31T23:50:00Z");
    let rows: Vec<Vec<u8>> = (0..20_u64).map(|i| row(start + i * MIN, 1.0)).collect();
    let r = write(&mut db, &cal, m.id, def, &rows).await;
    assert_eq!(
        r,
        Report {
            inserted: 20,
            duplicates: 0,
            refused: 0
        }
    );
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 2, "{p:?}");
    assert_eq!(p[0].end, t("2027-01-01T00:00:00Z"));
    assert_eq!(p[1].key, t("2027-01-01T00:01:00Z"));
    for x in &p {
        check(x, MIN);
    }

    let mut more = rows[15..].to_vec();
    more.extend((23..26_u64).map(|i| row(start + i * MIN, 2.0)));
    let r = write(&mut db, &cal, m.id, def, &more).await;
    assert_eq!(
        r,
        Report {
            inserted: 3,
            duplicates: 5,
            refused: 0
        }
    );
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 2);
    assert_eq!(p[1].count, 12);
    assert_eq!(p[1].index.len(), 4, "{:?}", p[1].index);
    for x in &p {
        check(x, MIN);
    }

    let fill: Vec<Vec<u8>> = (20..23_u64).map(|i| row(start + i * MIN, 3.0)).collect();
    let r = write(&mut db, &cal, m.id, def, &fill).await;
    assert_eq!(
        r,
        Report {
            inserted: 3,
            duplicates: 0,
            refused: 0
        }
    );
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p[1].count, 15);
    assert_eq!(p[1].index.len(), 2, "{:?}", p[1].index);
    for x in &p {
        check(x, MIN);
    }
    drop(db);
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_packs_and_other_definitions() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "sparse").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let cal = Calendar::new("UTC").unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let at = t("2026-10-14T08:00:00Z");
    let rows = vec![row(at, 1.0), row(at, 2.0), row(at + 5, 3.0)];
    let r = write_period(&mut db, &cal, m.id, def, 0, &rows).await;
    assert_eq!(r.inserted, 3);
    let r = write_period(
        &mut db,
        &cal,
        m.id,
        def,
        0,
        &[row(at + 5, 3.0), row(at + 5, 4.0)],
    )
    .await;
    assert_eq!((r.inserted, r.duplicates), (1, 1));
    let r = write_period(
        &mut db,
        &cal,
        m.id,
        def,
        0,
        &[row(at, 2.0), row(at + 1, 9.0)],
    )
    .await;
    assert_eq!((r.inserted, r.duplicates), (1, 1));
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 1);
    check(&p[0], 0);
    assert_eq!(p[0].count, 5);

    let mut other = u1_definition();
    other.props[0].user_name = "U2".into();
    let def2 = db.definition_for(m.id, 0, &other, true).await.unwrap();
    assert_ne!(def, def2);
    let r = write_period(
        &mut db,
        &cal,
        m.id,
        def2,
        0,
        &[row(at + 2, 1.0), row(at + 10, 1.0)],
    )
    .await;
    assert_eq!((r.inserted, r.refused), (1, 1));
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 2);
    assert_eq!(p[1].def, def2);
    drop(db);
    tdb.drop_db(&s).await;
}

async fn scalar(c: &mut Client<tokio_util::compat::Compat<TcpStream>>, sql: &str) -> Option<i32> {
    c.simple_query(sql)
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .and_then(|r| r.get::<i32, _>(0))
}

fn disjoint(p: &[Pack]) {
    for w in p.windows(2) {
        assert!(w[0].end < w[1].key, "{p:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reopened_packs_end_before_the_next_pack() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "nextpack").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let cal = Calendar::new("UTC").unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let at = |h: u64, min: u64| t("2026-06-01T00:00:00Z") + h * 60 * MIN + min * MIN;
    let rows = |h: u64, mins: std::ops::RangeInclusive<u64>| -> Vec<Vec<u8>> {
        mins.map(|i| row(at(h, i), 1.0)).collect()
    };
    let r = write(&mut db, &cal, m.id, def, &rows(10, 0..=5)).await;
    assert_eq!(r.inserted, 6);
    let mut sql = raw(&s, &tdb.name).await;
    let def2 = scalar(
        &mut sql,
        &format!(
            "INSERT INTO dbo.UniArchiveDefinition (CRC, TypeXML, Len) OUTPUT inserted.Id
             SELECT CRC, TypeXML, Len FROM dbo.UniArchiveDefinition WHERE Id = {def}"
        ),
    )
    .await
    .unwrap();
    let r = write(&mut db, &cal, m.id, def2, &rows(11, 0..=5)).await;
    assert_eq!(r.inserted, 6);

    let mut more = vec![row(at(10, 3), 1.0)];
    more.extend(rows(10, 6..=10));
    more.extend([row(at(11, 0), 1.0), row(at(11, 6), 1.0)]);
    let r = write(&mut db, &cal, m.id, def, &more).await;
    assert_eq!(
        r,
        Report {
            inserted: 6,
            duplicates: 2,
            refused: 0
        }
    );
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 2, "{p:?}");
    assert_eq!((p[0].count, p[0].end), (11, at(10, 10)));
    assert_eq!((p[1].key, p[1].count, p[1].def), (at(11, 0), 7, def2));
    disjoint(&p);
    for x in &p {
        check(x, MIN);
    }

    let r = write(
        &mut db,
        &cal,
        m.id,
        def,
        &[row(at(9, 58), 1.0), row(at(11, 7), 1.0)],
    )
    .await;
    assert_eq!(r.inserted, 2);
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 3, "{p:?}");
    assert_eq!(
        (p[0].key, p[0].end, p[0].count),
        (at(9, 58), at(10, 10), 12)
    );
    assert_eq!((p[2].key, p[2].count, p[2].def), (at(11, 7), 1, def));
    disjoint(&p);
    for x in &p {
        check(x, MIN);
    }
    drop((db, sql));
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn definitions_stored_meanwhile_are_reused() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "defsnew").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let mut other = u1_definition();
    other.props[0].user_name = "U2".into();
    let xml = other.to_xml();
    let crc = i64::from(kmb_envis::definition::envis_crc(&xml));
    let mut c = raw(&s, &tdb.name).await;
    let theirs = c
        .query(
            "INSERT INTO dbo.UniArchiveDefinition (CRC, TypeXML, Len)
             OUTPUT inserted.Id VALUES (@P1, CAST(@P2 AS xml), 0)",
            &[&crc, &xml.as_str()],
        )
        .await
        .unwrap()
        .into_row()
        .await
        .unwrap()
        .unwrap()
        .get::<i32, _>(0)
        .unwrap();
    assert_ne!(def, theirs);
    assert!(db.stored_definition(theirs).await.unwrap().is_some());
    assert_eq!(
        db.definition_for(m.id, 0, &other, true).await.unwrap(),
        theirs
    );
    assert_eq!(
        scalar(&mut c, "SELECT COUNT(*) FROM dbo.UniArchiveDefinition").await,
        Some(2)
    );
    drop((db, c));
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_merge_sees_every_row_at_its_time() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "sparsedup").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let cal = Calendar::new("UTC").unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let step = u64::from(kmb_envis::pack::sparse_step(12));
    assert_eq!(step, 834);
    let at = t("2026-10-14T00:00:00Z");
    let shared = at + (step - 1) * 1000;
    let mut rows: Vec<Vec<u8>> = (0..step - 1).map(|i| row(at + i * 1000, 0.0)).collect();
    rows.extend([row(shared, 1.0), row(shared, 2.0), row(shared + 1000, 3.0)]);
    let r = write_period(&mut db, &cal, m.id, def, 0, &rows).await;
    assert_eq!(r.inserted, 836);
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p[0].index, [(0, at), (834, shared)]);
    let r = write_period(&mut db, &cal, m.id, def, 0, &[row(shared, 1.0)]).await;
    assert_eq!((r.inserted, r.duplicates), (0, 1));
    let p = packs(&s, &tdb.name, m.id, 0).await;
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].count, 836);
    check(&p[0], 0);
    drop(db);
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_archive_is_locked() {
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "emptylock").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let cal = Calendar::new("UTC").unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let def = db
        .definition_for(m.id, 0, &u1_definition(), true)
        .await
        .unwrap();
    let lock = format!(
        "SET LOCK_TIMEOUT 1000; BEGIN TRANSACTION;
         SELECT TOP 1 keyTime FROM dbo.UniArchiveBinPack WITH (XLOCK, ROWLOCK, HOLDLOCK)
         WHERE keymeasName = {} AND keyArchID = 0;
         ROLLBACK TRANSACTION",
        m.id
    );
    let mut c = raw(&s, &tdb.name).await;
    let a = Appender::begin(&mut db, &cal, m.id, 0, def, MIN, 12)
        .await
        .unwrap();
    let blocked = match c.simple_query(lock.as_str()).await {
        Ok(stream) => stream.into_results().await.is_err(),
        Err(_) => true,
    };
    assert!(blocked, "the empty archive was not locked");
    a.commit().await.unwrap();
    drop(c);
    let mut c = raw(&s, &tdb.name).await;
    c.simple_query(lock.as_str())
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    drop((db, c));
    tdb.drop_db(&s).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dstate_runs_are_replaced() {
    use kmb_envis::db::BadRun;
    let Some(s) = server() else { return };
    let tdb = TestDb::new(&s, "dstate").await;
    let mut db = Envis::connect(&tdb.target).await.unwrap();
    let m = db
        .measurement("O", "M", &identity(), "tcp://x")
        .await
        .unwrap();
    let at = t("2026-10-14T08:00:00Z") + 5;
    let run = BadRun {
        from: 3,
        to: 4,
        start: at,
        end: at + MIN,
        reason: 16,
    };
    db.update_dstate(m.id, 0, 0..10, &[run]).await.unwrap();
    let runs = format!(
        "SET LOCK_TIMEOUT 5000;
         SELECT COUNT(*), MIN(CAST([From] AS int)), MAX(CAST([To] AS int))
         FROM dbo.DState WHERE keymeasName = {}",
        m.id
    );
    let mut c = raw(&s, &tdb.name).await;
    let count = async |c: &mut Client<tokio_util::compat::Compat<TcpStream>>| {
        let r = c
            .simple_query(runs.as_str())
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .unwrap();
        (
            r.get::<i32, _>(0).unwrap(),
            r.get::<i32, _>(1),
            r.get::<i32, _>(2),
        )
    };
    assert_eq!(count(&mut c).await, (1, Some(3), Some(4)));
    db.update_dstate(m.id, 0, 4..10, &[]).await.unwrap();
    assert_eq!(count(&mut c).await, (1, Some(3), Some(3)));
    assert!(db.update_dstate(-1, 0, 0..10, &[run]).await.is_err());
    db.update_dstate(m.id, 0, 0..10, &[]).await.unwrap();
    assert_eq!(count(&mut c).await, (0, None, None));
    drop((db, c));
    tdb.drop_db(&s).await;
}
