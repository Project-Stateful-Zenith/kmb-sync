use kmb::archive::uni::{Layout, Stat};
use kmb::uni::value::{TypeCode, Value};
use kmb::uni::{ArchiveDescription, ArchiveField, ArchiveGroup};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupDef {
    pub idx: u8,
    pub idxs: u8,
    pub idxss: u8,
    pub typ: u16,
    pub k: f32,
    pub unit: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub ic: u8,
    pub ics: u8,
    pub icss: u8,
    pub info1: u8,
    pub info2: u8,
    pub type_code: u8,
    pub pb: u16,
    pub zapis: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zapis2: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub archive: u8,
    pub version: u32,
    pub record_len: usize,
    pub groups: Vec<GroupDef>,
    pub fields: Vec<FieldDef>,
}

impl From<&ArchiveGroup> for GroupDef {
    fn from(g: &ArchiveGroup) -> Self {
        Self {
            idx: g.idx,
            idxs: g.idxs,
            idxss: g.idxss,
            typ: g.typ,
            k: g.k,
            unit: g.unit.clone(),
            name: g.name.clone(),
        }
    }
}

impl From<&GroupDef> for ArchiveGroup {
    fn from(g: &GroupDef) -> Self {
        Self {
            idx: g.idx,
            idxs: g.idxs,
            idxss: g.idxss,
            typ: g.typ,
            k: g.k,
            unit: g.unit.clone(),
            name: g.name.clone(),
        }
    }
}

impl From<&ArchiveField> for FieldDef {
    fn from(f: &ArchiveField) -> Self {
        Self {
            name: f.name.clone(),
            ic: f.ic,
            ics: f.ics,
            icss: f.icss,
            info1: f.info1,
            info2: f.info2,
            type_code: f.type_code,
            pb: f.pb,
            zapis: f.zapis,
            zapis2: f.zapis2.clone(),
        }
    }
}

impl From<&FieldDef> for ArchiveField {
    fn from(f: &FieldDef) -> Self {
        Self {
            name: f.name.clone(),
            ic: f.ic,
            ics: f.ics,
            icss: f.icss,
            info1: f.info1,
            info2: f.info2,
            type_code: f.type_code,
            pb: f.pb,
            zapis: f.zapis,
            zapis2: f.zapis2.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DecodedField {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<&'static str>,
    pub unit: String,
    pub value: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum DefinitionError {
    #[error("archive definition does not build a layout: {0}")]
    Layout(String),
    #[error("record does not decode: {0}")]
    Record(String),
}

impl Definition {
    #[must_use]
    pub fn from_layout(layout: &Layout, desc: &ArchiveDescription) -> Self {
        let mut groups = vec![GroupDef::from(&layout.group)];
        groups.extend(
            desc.groups
                .iter()
                .filter(|g| g.idx == layout.group.idx && (g.idxs, g.idxss) != (0, 0))
                .map(GroupDef::from),
        );
        Self {
            archive: layout.archive,
            version: layout.version,
            record_len: layout.record_len,
            groups,
            fields: desc
                .fields
                .iter()
                .filter(|f| f.ic == layout.group.idx)
                .map(FieldDef::from)
                .collect(),
        }
    }

    #[must_use]
    pub fn description(&self) -> ArchiveDescription {
        ArchiveDescription {
            groups: self.groups.iter().map(ArchiveGroup::from).collect(),
            fields: self.fields.iter().map(ArchiveField::from).collect(),
        }
    }

    pub fn layout(&self) -> Result<Layout, DefinitionError> {
        let layout = Layout::new(
            &self.description(),
            self.archive,
            self.version,
            self.record_len.saturating_add(1),
        )
        .map_err(|e| DefinitionError::Layout(e.to_string()))?;
        if layout.record_len != self.record_len {
            return Err(DefinitionError::Layout(
                "record length differs from the stored one".into(),
            ));
        }
        Ok(layout)
    }

    #[must_use]
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    #[must_use]
    pub fn crc32(&self) -> u32 {
        crc32fast::hash(self.canonical().as_bytes())
    }

    pub fn decode(&self, record: &[u8]) -> Result<Vec<DecodedField>, DefinitionError> {
        let layout = self.layout()?;
        let desc = self.description();
        let values = layout
            .decode(record)
            .map_err(|e| DefinitionError::Record(e.to_string()))?;
        let columns = layout.columns(&desc);
        let time_index = layout
            .slots
            .iter()
            .position(|s| s.field.name == "Time")
            .unwrap_or(0);
        Ok(values
            .into_iter()
            .zip(columns)
            .zip(&layout.slots)
            .skip(time_index.saturating_add(1))
            .map(|(((_, value), column), slot)| {
                let k = f64::from(column.k);
                let value = if slot.field.typ() == TypeCode::DateTime {
                    value_json(&value, 1.0)
                } else {
                    value_json(&value, k)
                };
                DecodedField {
                    name: column.name,
                    stat: column.stat.map(Stat::as_str),
                    unit: column.unit,
                    value,
                }
            })
            .collect())
    }
}

const fn is_one(k: f64) -> bool {
    k.to_bits() == 1.0_f64.to_bits()
}

fn number(v: f64, k: f64) -> serde_json::Value {
    let x = if is_one(k) { v } else { v * k };
    serde_json::Number::from_f64(x).map_or(serde_json::Value::Null, serde_json::Value::Number)
}

#[must_use]
pub fn value_json(value: &Value, k: f64) -> serde_json::Value {
    use serde_json::Value as J;
    let list = |it: &mut dyn Iterator<Item = f64>| J::Array(it.map(|v| number(v, k)).collect());
    match value {
        Value::U8(v) if is_one(k) => J::from(*v),
        Value::U16(v) if is_one(k) => J::from(*v),
        Value::U32(v) if is_one(k) => J::from(*v),
        Value::U64(v) if is_one(k) => J::from(*v),
        Value::S8(v) if is_one(k) => J::from(*v),
        Value::S16(v) if is_one(k) => J::from(*v),
        Value::S32(v) if is_one(k) => J::from(*v),
        Value::S64(v) if is_one(k) => J::from(*v),
        Value::U8Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::U16Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::U32Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::F32Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::F64Array(a) => list(&mut a.iter().copied()),
        Value::S8Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::S16Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::S32Array(a) => list(&mut a.iter().map(|&v| f64::from(v))),
        Value::String(s) => J::from(s.as_str()),
        Value::Empty => J::Null,
        other => other.as_f64().map_or(J::Null, |v| number(v, k)),
    }
}
