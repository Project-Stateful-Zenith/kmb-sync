use crate::config::{self, EnvisDb};
use crate::device::MeterClient;
use anyhow::{Context, Result};
use kmb::Identify;
use kmb::archive::uni::archive_name;
use kmb::uni::{ArchiveDescription, Description, Value};
use kmb_envis::db::{Envis, IdentityRow, Target};
use kmb_envis::pack::Calendar;

pub struct EnvisSlot {
    target: Target,
    pub calendar: Calendar,
    pub allow_new: bool,
    idle: std::sync::Mutex<Vec<Envis>>,
}

pub struct Conn<'s> {
    slot: &'s EnvisSlot,
    db: Option<Envis>,
}

impl Conn<'_> {
    /// The connection, opened again if an error dropped it.
    pub async fn db(&mut self) -> Result<&mut Envis> {
        if self.db.is_none() {
            self.db = Some(self.slot.connect().await?);
        }
        self.db
            .as_mut()
            .context("ENVIS database connection is missing")
    }

    pub fn mark_broken(&mut self) {
        self.db = None;
    }

    pub fn release(mut self) {
        if let Some(db) = self.db.take() {
            self.slot
                .idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(db);
        }
    }
}

impl EnvisSlot {
    pub fn new(c: &EnvisDb) -> Result<Self> {
        let calendar = match &c.time_zone {
            Some(zone) => Calendar::new(zone).with_context(|| format!("time zone {zone:?}"))?,
            None => Calendar::system(),
        };
        Ok(Self {
            target: Target {
                host: c.host.clone(),
                port: c.port,
                database: c.database.clone(),
                user: c.user.clone(),
                password: config::read_secret(&c.password_file)?,
                trust_cert: c.trust_cert,
            },
            calendar,
            allow_new: c.allow_new_definitions,
            idle: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub async fn acquire(&self) -> Result<Conn<'_>> {
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let db = match idle {
            Some(db) => db,
            None => self.connect().await?,
        };
        Ok(Conn {
            slot: self,
            db: Some(db),
        })
    }

    async fn connect(&self) -> Result<Envis> {
        Box::pin(Envis::connect(&self.target))
            .await
            .with_context(|| {
                format!(
                    "ENVIS database {} on {}",
                    self.target.database, self.target.host
                )
            })
    }
}

pub async fn notebook(client: &mut MeterClient, address: u8) -> Result<(String, String)> {
    let payload = client
        .get(&kmb::command::get(address, kmb::command::cmd::GET_NOTEBOOK))
        .await
        .context("reading the notebook")?;
    let field = |range: std::ops::Range<usize>| {
        let bytes: Vec<u8> = payload
            .get(range)
            .unwrap_or_default()
            .iter()
            .map(|&b| if b == 0xFF { b' ' } else { b })
            .collect();
        let text = kmb::decode::kmb_string(&bytes).replace('?', " ");
        let text = text.trim();
        if text.is_empty() {
            "DEFAULT".to_string()
        } else {
            text.to_string()
        }
    };
    Ok((field(0..32), field(32..64)))
}

#[must_use]
pub fn identity_row(id: &Identify, archives: &ArchiveDescription) -> IdentityRow {
    let bits = archives
        .groups
        .iter()
        .filter(|g| g.idxs == 0 && g.idxss == 0)
        .filter_map(|g| (0..64_u8).find(|&t| archive_name(t) == Some(g.name.as_str())))
        .fold(0_u64, |acc, t| acc | (1_u64 << t));
    IdentityRow {
        device_no: id.device_no,
        device_type: id.device_type,
        sub_device_type: id.sub_device_type,
        props_type: id.props_type,
        software_version: id.software_version,
        hardware_version: id.hardware_version,
        software_modules: id.software_modules,
        device_addr: id.device_addr,
        bootloader_version: id.bootloader_version,
        archives: bits,
    }
}

#[must_use]
pub fn period(config: &Description, ic: u8) -> u64 {
    let name = format!("ArchPeriod_{ic}");
    config
        .fields
        .iter()
        .position(|f| f.name == name)
        .and_then(|i| config.values.get(i)?.as_ref())
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

pub const ENVIS_ORDER: [u8; 20] = [
    0, 25, 26, 24, 6, 3, 5, 4, 14, 17, 9, 8, 11, 13, 1, 2, 10, 15, 16, 50,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    pub archive: u8,
    pub name: String,
    pub ic: u8,
}

#[must_use]
pub fn offered(desc: &ArchiveDescription) -> Vec<Offered> {
    let mut out: Vec<Offered> = desc
        .groups
        .iter()
        .filter(|g| g.idxs == 0 && g.idxss == 0)
        .map(|g| Offered {
            archive: (0..=u8::MAX)
                .find(|&t| archive_name(t) == Some(g.name.as_str()))
                .unwrap_or(g.idx),
            name: g.name.clone(),
            ic: g.idx,
        })
        .collect();
    let rank = |o: &Offered| {
        let known = archive_name(o.archive) == Some(o.name.as_str());
        ENVIS_ORDER
            .iter()
            .position(|&a| known && a == o.archive)
            .unwrap_or(ENVIS_ORDER.len())
    };
    out.sort_by_key(|o| (rank(o), o.archive));
    out
}

#[must_use]
pub const fn envis_unsupported(archive: u8) -> Option<&'static str> {
    match archive {
        11 => Some("GO waveforms have their own ENVIS writer, not built"),
        13 => Some("Modbus Master sets have their own ENVIS writer, not built"),
        18 => Some("averaged values are stored as actual values, not built"),
        24 => Some("H2M has its own ENVIS writer, not built"),
        26 => Some("LB2 is not stored in an ENVIS database yet"),
        _ => None,
    }
}
