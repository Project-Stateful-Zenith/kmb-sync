use crate::definition::{TypeDescription, envis_crc};
use crate::{Error, Result};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub const MIN_DB_VERSION: (i32, i32, i32, i32) = (1, 2, 0, 14_671);

pub const OBJECT_NAME_MAX: usize = 32;

/// Longest wait for the TCP connection. Without it a host that drops packets
/// keeps a run, and its meter link, waiting for minutes; tiberius bounds the
/// login after it by itself (15 s).
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub trust_cert: bool,
}

pub struct Envis {
    pub(crate) client: Client<Compat<TcpStream>>,
    definitions: Vec<StoredDefinition>,
    unreadable: Vec<i32>,
    loaded: bool,
}

#[derive(Clone, Debug)]
pub struct StoredDefinition {
    pub id: i32,
    pub fields: TypeDescription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRow {
    pub device_no: u32,
    pub device_type: u16,
    pub sub_device_type: [u16; 6],
    pub props_type: u16,
    pub software_version: u16,
    pub hardware_version: u16,
    pub software_modules: u16,
    pub device_addr: u8,
    pub bootloader_version: u8,
    pub archives: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub object: i32,
    pub identity: i32,
    pub id: i32,
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn sql_err(e: tiberius::error::Error) -> Error {
    Error::Sql(e.to_string())
}

pub(crate) fn col<'a, T: tiberius::FromSql<'a>>(row: &'a Row, i: usize) -> Result<Option<T>> {
    row.try_get(i).map_err(sql_err)
}

impl Envis {
    pub async fn connect(target: &Target) -> Result<Self> {
        let mut config = Config::new();
        config.host(&target.host);
        config.port(target.port);
        config.database(&target.database);
        config.authentication(AuthMethod::sql_server(&target.user, &target.password));
        config.encryption(EncryptionLevel::Required);
        if target.trust_cert {
            config.trust_cert();
        }
        config.application_name("kmb-sync");
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(config.get_addr()))
            .await
            .map_err(|_| {
                Error::Sql(format!(
                    "connect to {}:{} timed out after {CONNECT_TIMEOUT:?}",
                    target.host, target.port
                ))
            })?
            .map_err(|e| Error::Sql(format!("connect to {}:{}: {e}", target.host, target.port)))?;
        tcp.set_nodelay(true)
            .map_err(|e| Error::Sql(e.to_string()))?;
        let client = Box::pin(Client::connect(config, tcp.compat_write()))
            .await
            .map_err(sql_err)?;
        let mut db = Self {
            client,
            definitions: Vec::new(),
            unreadable: Vec::new(),
            loaded: false,
        };
        db.check_version().await?;
        Ok(db)
    }

    async fn check_version(&mut self) -> Result<()> {
        let rows = self
            .client
            .simple_query("SELECT TOP 1 Major, Minor, Build, Revision FROM dbo.ENVDBUpdates")
            .await
            .map_err(sql_err)?
            .into_first_result()
            .await
            .map_err(sql_err)?;
        let row = rows
            .first()
            .ok_or_else(|| Error::Version("dbo.ENVDBUpdates is empty".into()))?;
        let v = (
            col::<i32>(row, 0)?.unwrap_or(0),
            col::<i32>(row, 1)?.unwrap_or(0),
            col::<i32>(row, 2)?.unwrap_or(0),
            col::<i32>(row, 3)?.unwrap_or(0),
        );
        if v.0 != MIN_DB_VERSION.0 || v < MIN_DB_VERSION {
            return Err(Error::Version(format!(
                "database version {}.{}.{}.{} is not 1.x from 1.2.0.14671 on",
                v.0, v.1, v.2, v.3
            )));
        }
        Ok(())
    }

    pub(crate) async fn exec(&mut self, sql: &str) -> Result<()> {
        self.client
            .simple_query(sql)
            .await
            .map_err(sql_err)?
            .into_results()
            .await
            .map_err(sql_err)?;
        Ok(())
    }

    pub async fn measurement(
        &mut self,
        object: &str,
        meas_name: &str,
        identity: &IdentityRow,
        url: &str,
    ) -> Result<Measurement> {
        let object = self.object(object).await?;
        let identity_id = self.identity(object, identity).await?;
        let id = self.meas_name(identity_id, meas_name).await?;
        self.client
            .execute(
                "MERGE dbo.SmpDeviceUrl WITH (HOLDLOCK) AS t
                 USING (SELECT @P1 AS u, @P2 AS i) AS s
                 ON t.keydeviceUrl = s.u AND t.keyIdentifyId = s.i
                 WHEN MATCHED THEN UPDATE SET [Time] = SYSUTCDATETIME()
                 WHEN NOT MATCHED THEN INSERT ([Time], keydeviceUrl, keyIdentifyId)
                     VALUES (SYSUTCDATETIME(), s.u, s.i);",
                &[&url, &identity_id],
            )
            .await
            .map_err(sql_err)?;
        Ok(Measurement {
            object,
            identity: identity_id,
            id,
        })
    }

    async fn object(&mut self, name: &str) -> Result<i32> {
        let cut: String = name.chars().take(OBJECT_NAME_MAX).collect();
        let alias = (cut.chars().count() < name.chars().count()).then(|| cut.clone());
        if let Some(id) = self
            .one_i32(
                "SELECT Id FROM dbo.SmpObjectDB WHERE Parent IS NULL AND objekt = @P1",
                &[&cut],
            )
            .await?
        {
            return Ok(id);
        }
        self.one_i32(
            "INSERT INTO dbo.SmpObjectDB (Parent, UserName, objekt) OUTPUT inserted.Id
             VALUES (NULL, @P1, @P2)",
            &[&alias, &cut],
        )
        .await?
        .ok_or_else(|| Error::Sql("object insert returned no id".into()))
    }

    async fn identity(&mut self, object: i32, i: &IdentityRow) -> Result<i32> {
        let no = i64::from(i.device_no);
        let (props, dtype) = (i32::from(i.props_type), i32::from(i.device_type));
        let found = self
            .one_i32(
                "SELECT TOP 1 Id FROM dbo.SmpIdentifyDB
                 WHERE DeviceNo = @P1 AND PropsTypeDB = @P2 AND DeviceTypeDB = @P3
                   AND objekt = @P4 AND AdeviceAddress IS NULL AND ArchiveDef IS NULL
                 ORDER BY Id",
                &[&no, &props, &dtype, &object],
            )
            .await?;
        let (sw, hw, modules) = (
            i32::from(i.software_version),
            i32::from(i.hardware_version),
            i32::from(i.software_modules),
        );
        let (addr, boot) = (i32::from(i.device_addr), i32::from(i.bootloader_version));
        let archives = i64::try_from(i.archives).unwrap_or(i64::MAX);
        if let Some(id) = found {
            self.client
                .execute(
                    "UPDATE dbo.SmpIdentifyDB SET SoftwareVersion = @P2, HardwareVersion = @P3,
                         BootloaderVersion = @P4, DeviceAddr = @P5,
                         SoftwareModules = CAST(CAST(ISNULL(SoftwareModules, 0) AS int) | @P6 AS numeric(5,0)),
                         Archives = CAST(CAST(ISNULL(Archives, 0) AS bigint) | @P7 AS numeric(20,0))
                     WHERE Id = @P1",
                    &[&id, &sw, &hw, &boot, &addr, &modules, &archives],
                )
                .await
                .map_err(sql_err)?;
            return Ok(id);
        }
        let s = i.sub_device_type.map(i32::from);
        self.one_i32(
            "INSERT INTO dbo.SmpIdentifyDB (AdeviceAddress, ArchiveDef, DeviceNo, SoftwareVersion,
                 HardwareVersion, SoftwareModules, DeviceAddr, BootloaderVersion, Archives, objekt,
                 DeviceTypeDB, SubDeviceTypeDB, SubDeviceType2DB, SubDeviceType3DB,
                 SubDeviceType4DB, SubDeviceType5DB, SubDeviceType6DB, PropsTypeDB, UserName)
             OUTPUT inserted.Id
             VALUES (NULL, NULL, @P1, @P2, @P3, @P4, @P5, @P6, @P7, @P8, @P9, @P10, @P11, @P12,
                 @P13, @P14, @P15, @P16, NULL)",
            &[
                &no, &sw, &hw, &modules, &addr, &boot, &archives, &object, &dtype, &s[0], &s[1],
                &s[2], &s[3], &s[4], &s[5], &props,
            ],
        )
        .await?
        .ok_or_else(|| Error::Sql("identity insert returned no id".into()))
    }

    async fn meas_name(&mut self, identity: i32, name: &str) -> Result<i32> {
        if let Some(id) = self
            .one_i32(
                "SELECT Id FROM dbo.SmpMeasNameDB WHERE identifyDB = @P1 AND measName = @P2",
                &[&identity, &name],
            )
            .await?
        {
            return Ok(id);
        }
        self.one_i32(
            "INSERT INTO dbo.SmpMeasNameDB (LastTime, UserName, identifyDB, measName)
             OUTPUT inserted.Id VALUES (NULL, NULL, @P1, @P2)",
            &[&identity, &name],
        )
        .await?
        .ok_or_else(|| Error::Sql("measurement insert returned no id".into()))
    }

    pub(crate) async fn one_i32(
        &mut self,
        sql: &str,
        params: &[&dyn tiberius::ToSql],
    ) -> Result<Option<i32>> {
        let row = self
            .client
            .query(sql, params)
            .await
            .map_err(sql_err)?
            .into_row()
            .await
            .map_err(sql_err)?;
        row.map_or(Ok(None), |r| col::<i32>(&r, 0))
    }

    pub async fn last_time(&mut self, meas: i32, archive: u8) -> Result<Option<u64>> {
        let row = self
            .client
            .query(
                "SELECT MAX(endTime) FROM dbo.UniArchiveBinPack
                 WHERE keymeasName = @P1 AND keyArchID = @P2",
                &[&meas, &archive],
            )
            .await
            .map_err(sql_err)?
            .into_row()
            .await
            .map_err(sql_err)?;
        Ok(match row {
            Some(r) => col::<time::PrimitiveDateTime>(&r, 0)?.map(from_sql_time),
            None => None,
        })
    }

    pub async fn newest_definition(&mut self, meas: i32, archive: u8) -> Result<Option<i32>> {
        self.one_i32(
            "SELECT TOP 1 ArchDef FROM dbo.UniArchiveBinPack
             WHERE keymeasName = @P1 AND keyArchID = @P2 ORDER BY keyTime DESC",
            &[&meas, &archive],
        )
        .await
    }

    pub async fn newest_rows(&mut self, object: &str) -> Result<Vec<(i32, String, u8, String)>> {
        let rows = self
            .client
            .query(
                "SELECT m.Id, m.measName, p.keyArchID, CONVERT(varchar(30), MAX(p.endTime), 126)
                 FROM dbo.UniArchiveBinPack p
                 JOIN dbo.SmpMeasNameDB m ON m.Id = p.keymeasName
                 JOIN dbo.SmpIdentifyDB i ON i.Id = m.identifyDB
                 JOIN dbo.SmpObjectDB o ON o.Id = i.objekt
                 WHERE o.objekt = @P1
                 GROUP BY m.Id, m.measName, p.keyArchID ORDER BY m.Id, p.keyArchID",
                &[&object],
            )
            .await
            .map_err(sql_err)?
            .into_first_result()
            .await
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for r in &rows {
            out.push((
                col::<i32>(r, 0)?.unwrap_or(0),
                col::<&str>(r, 1)?.unwrap_or_default().to_string(),
                col::<u8>(r, 2)?.unwrap_or(0),
                col::<&str>(r, 3)?.unwrap_or_default().to_string(),
            ));
        }
        Ok(out)
    }

    async fn load_definitions(&mut self) -> Result<()> {
        if self.loaded {
            return Ok(());
        }
        self.reload_definitions().await
    }

    async fn reload_definitions(&mut self) -> Result<()> {
        let rows = self
            .client
            .simple_query(
                "SELECT Id, CAST(TypeXML AS nvarchar(max)) FROM dbo.UniArchiveDefinition ORDER BY Id",
            )
            .await
            .map_err(sql_err)?
            .into_first_result()
            .await
            .map_err(sql_err)?;
        let mut definitions = Vec::new();
        let mut unreadable = Vec::new();
        for r in &rows {
            let Some(id) = col::<i32>(r, 0)? else {
                continue;
            };
            match col::<&str>(r, 1)?.map(TypeDescription::from_sql_xml) {
                Some(Ok(fields)) => definitions.push(StoredDefinition { id, fields }),
                _ => unreadable.push(id),
            }
        }
        self.definitions = definitions;
        self.unreadable = unreadable;
        self.loaded = true;
        Ok(())
    }

    async fn load_definition(&mut self, id: i32) -> Result<()> {
        self.load_definitions().await?;
        if !self.definitions.iter().any(|d| d.id == id) && !self.unreadable.contains(&id) {
            self.reload_definitions().await?;
        }
        Ok(())
    }

    pub async fn definitions(&mut self) -> Result<&[StoredDefinition]> {
        self.load_definitions().await?;
        Ok(&self.definitions)
    }

    pub async fn stored_definition(&mut self, id: i32) -> Result<Option<StoredDefinition>> {
        self.load_definition(id).await?;
        Ok(self.definitions.iter().find(|d| d.id == id).cloned())
    }

    fn fitting_definition(&self, newest: Option<i32>, ours: &TypeDescription) -> Option<i32> {
        let fitting = || {
            self.definitions
                .iter()
                .filter(|d| d.fields.same_layout(ours))
                .map(|d| d.id)
        };
        newest
            .filter(|n| fitting().any(|id| id == *n))
            .or_else(|| fitting().max())
    }

    pub async fn definition_for(
        &mut self,
        meas: i32,
        archive: u8,
        ours: &TypeDescription,
        allow_new: bool,
    ) -> Result<i32> {
        let newest = self.newest_definition(meas, archive).await?;
        let fresh = !self.loaded;
        match newest {
            Some(id) => self.load_definition(id).await?,
            None => self.load_definitions().await?,
        }
        if let Some(id) = self.fitting_definition(newest, ours) {
            return Ok(id);
        }
        if !fresh {
            self.reload_definitions().await?;
            if let Some(id) = self.fitting_definition(newest, ours) {
                return Ok(id);
            }
        }
        if !allow_new {
            return Err(Error::NoDefinition(archive));
        }
        let xml = ours.to_xml();
        let crc = i64::from(envis_crc(&xml));
        let id = self
            .one_i32(
                "INSERT INTO dbo.UniArchiveDefinition (CRC, TypeXML, Len)
                 OUTPUT inserted.Id VALUES (@P1, CAST(@P2 AS xml), 0)",
                &[&crc, &xml.as_str()],
            )
            .await?
            .ok_or_else(|| Error::Sql("definition insert returned no id".into()))?;
        self.definitions.push(StoredDefinition {
            id,
            fields: ours.clone(),
        });
        Ok(id)
    }
}

impl Envis {
    pub async fn store_config(
        &mut self,
        meas: i32,
        cfg_time: u64,
        description: &[u8],
        data: &[u8],
    ) -> Result<()> {
        let d = self.config_bin(description).await?;
        let v = self.config_bin(data).await?;
        let at = to_sql_time(cfg_time);
        self.client
            .execute(
                "IF EXISTS (SELECT 1 FROM dbo.UniConfig WITH (UPDLOCK, HOLDLOCK)
                            WHERE keymeasName = @P1 AND keyTime = CAST(@P2 AS datetime))
                     UPDATE dbo.UniConfig SET Description = @P3, Data = @P4
                     WHERE keymeasName = @P1 AND keyTime = CAST(@P2 AS datetime)
                 ELSE
                     INSERT INTO dbo.UniConfig (keymeasName, keyTime, Description, Data)
                     VALUES (@P1, CAST(@P2 AS datetime), @P3, @P4)",
                &[&meas, &at, &d, &v],
            )
            .await
            .map_err(sql_err)?;
        Ok(())
    }

    async fn config_bin(&mut self, bytes: &[u8]) -> Result<i32> {
        let crc = i64::from(crate::definition::envis_crc_bytes(bytes));
        if let Some(id) = self
            .one_i32(
                "SELECT TOP 1 Id FROM dbo.UniConfigBin WHERE CRC = @P1 AND Data = @P2 ORDER BY Id",
                &[&crc, &bytes],
            )
            .await?
        {
            return Ok(id);
        }
        self.one_i32(
            "INSERT INTO dbo.UniConfigBin (CRC, Data) OUTPUT inserted.Id VALUES (@P1, @P2)",
            &[&crc, &bytes],
        )
        .await?
        .ok_or_else(|| Error::Sql("config insert returned no id".into()))
    }
}

pub(crate) fn from_sql_time(t: time::PrimitiveDateTime) -> u64 {
    let ms = t.assume_utc().unix_timestamp_nanos().div_euclid(1_000_000);
    u64::try_from(
        i64::try_from(ms)
            .unwrap_or(i64::MAX)
            .saturating_sub(crate::pack::EPOCH_2000_MS),
    )
    .unwrap_or(0)
}

pub(crate) fn to_sql_time(t: u64) -> time::PrimitiveDateTime {
    let ms = i128::from(t).saturating_add(i128::from(crate::pack::EPOCH_2000_MS));
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(ms.saturating_mul(1_000_000))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    time::PrimitiveDateTime::new(at.date(), at.time())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRun {
    pub from: u32,
    pub to: u32,
    pub start: u64,
    pub end: u64,
    pub reason: u8,
}

fn interpolate(slot: u32, (s0, t0): (u32, u64), (s1, t1): (u32, u64)) -> u64 {
    if s1 <= s0 {
        return t0;
    }
    let span = u128::from(s1.saturating_sub(s0));
    let off = u128::from(slot.saturating_sub(s0)).min(span);
    let dt = u128::from(t1.saturating_sub(t0));
    let step = dt.saturating_mul(off).checked_div(span).unwrap_or(0);
    t0.saturating_add(u64::try_from(step).unwrap_or(u64::MAX))
}

#[must_use]
pub fn bad_runs(
    read: std::ops::Range<u32>,
    good: &[(u32, u64)],
    bad: &[(u32, u8)],
    fallback: u64,
) -> Vec<BadRun> {
    let mut bad: Vec<(u32, u8)> = bad
        .iter()
        .copied()
        .filter(|(s, _)| read.contains(s))
        .collect();
    bad.sort_unstable();
    let mut runs: Vec<BadRun> = Vec::new();
    for (slot, reason) in bad {
        match runs.last_mut() {
            Some(r) if r.reason == reason && r.to.checked_add(1) == Some(slot) => r.to = slot,
            _ => runs.push(BadRun {
                from: slot,
                to: slot,
                start: 0,
                end: 0,
                reason,
            }),
        }
    }
    for r in &mut runs {
        let before = good
            .iter()
            .filter(|(s, _)| *s < r.from)
            .max_by_key(|(s, _)| *s);
        let after = good
            .iter()
            .filter(|(s, _)| *s > r.to)
            .min_by_key(|(s, _)| *s);
        let (a, b) = match (before, after) {
            (Some(&a), Some(&b)) => (a, b),
            (Some(&a), None) => (a, a),
            (None, Some(&b)) => (b, b),
            (None, None) => ((r.from, fallback), (r.from, fallback)),
        };
        r.start = interpolate(r.from, a, b);
        r.end = interpolate(r.to, a, b);
    }
    runs
}

impl Envis {
    pub async fn update_dstate(
        &mut self,
        meas: i32,
        archive: u8,
        read: std::ops::Range<u32>,
        runs: &[BadRun],
    ) -> Result<()> {
        self.exec("SET XACT_ABORT ON; BEGIN TRANSACTION").await?;
        let done = match self.replace_dstate(meas, archive, read, runs).await {
            Ok(()) => self.exec("COMMIT TRANSACTION").await,
            Err(e) => Err(e),
        };
        if done.is_err() {
            let _ = self.exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
        }
        done
    }

    async fn replace_dstate(
        &mut self,
        meas: i32,
        archive: u8,
        read: std::ops::Range<u32>,
        runs: &[BadRun],
    ) -> Result<()> {
        let (first, last) = (i64::from(read.start), i64::from(read.end.saturating_sub(1)));
        let rows = self
            .client
            .query(
                "SELECT keyTime, endTime, CAST([From] AS bigint), CAST([To] AS bigint), CAST(reason AS int)
                 FROM dbo.DState WITH (UPDLOCK, HOLDLOCK)
                 WHERE keymeasName = @P1 AND keyArchID = @P2 AND [To] >= @P3 AND [From] <= @P4",
                &[&meas, &archive, &first, &last],
            )
            .await
            .map_err(sql_err)?
            .into_first_result()
            .await
            .map_err(sql_err)?;
        let mut keep: Vec<BadRun> = runs.to_vec();
        let mut old_keys = Vec::new();
        for r in &rows {
            let (Some(k), Some(e), Some(f), Some(t), Some(reason)) = (
                col::<time::PrimitiveDateTime>(r, 0)?,
                col::<time::PrimitiveDateTime>(r, 1)?,
                col::<i64>(r, 2)?,
                col::<i64>(r, 3)?,
                col::<i32>(r, 4)?,
            ) else {
                continue;
            };
            old_keys.push(k);
            let (Ok(f), Ok(t)) = (u32::try_from(f), u32::try_from(t)) else {
                continue;
            };
            let ends = ((f, from_sql_time(k)), (t, from_sql_time(e)));
            let reason = u8::try_from(reason).unwrap_or(0);
            let mut piece = |from: u32, to: u32| {
                keep.push(BadRun {
                    from,
                    to,
                    start: interpolate(from, ends.0, ends.1),
                    end: interpolate(to, ends.0, ends.1),
                    reason,
                });
            };
            if f < read.start {
                piece(f, read.start.saturating_sub(1).min(t));
            }
            if t >= read.end {
                piece(read.end.max(f), t);
            }
        }
        for k in old_keys {
            self.client
                .execute(
                    "DELETE FROM dbo.DState
                     WHERE keymeasName = @P1 AND keyArchID = @P2 AND keyTime = CAST(@P3 AS datetime)",
                    &[&meas, &archive, &k],
                )
                .await
                .map_err(sql_err)?;
        }
        for r in keep {
            let (f, t) = (i64::from(r.from), i64::from(r.to));
            let reason = i32::from(r.reason);
            self.client
                .execute(
                    "IF NOT EXISTS (SELECT 1 FROM dbo.DState WHERE keyArchID = @P2 AND keymeasName = @P1
                                    AND keyTime = CAST(@P3 AS datetime))
                     INSERT INTO dbo.DState (keyArchID, keymeasName, keyTime, endTime, [From], [To], reason)
                     VALUES (@P2, @P1, CAST(@P3 AS datetime), CAST(@P4 AS datetime), @P5, @P6, @P7)",
                    &[
                        &meas,
                        &archive,
                        &to_sql_time(r.start),
                        &to_sql_time(r.end),
                        &f,
                        &t,
                        &reason,
                    ],
                )
                .await
                .map_err(sql_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_host_that_drops_packets_times_out() {
        // 10.255.255.1 is not routed: its SYNs go unanswered, or the network
        // says unreachable at once; either way no wait of the OS's ~2 min
        let target = Target {
            host: "10.255.255.1".into(),
            port: 1433,
            database: "x".into(),
            user: "u".into(),
            password: "p".into(),
            trust_cert: true,
        };
        let started = std::time::Instant::now();
        assert!(Box::pin(Envis::connect(&target)).await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn runs_and_times() {
        let good = [(9, 1000), (14, 6000)];
        let bad = [(10, 16), (11, 16), (12, 1), (13, 1), (40, 16)];
        let runs = bad_runs(10..20, &good, &bad, 77);
        assert_eq!(
            runs,
            [
                BadRun {
                    from: 10,
                    to: 11,
                    start: 2000,
                    end: 3000,
                    reason: 16
                },
                BadRun {
                    from: 12,
                    to: 13,
                    start: 4000,
                    end: 5000,
                    reason: 1
                },
            ]
        );
        assert_eq!(bad_runs(0..5, &[], &[(1, 1)], 77)[0].start, 77);
    }
}
