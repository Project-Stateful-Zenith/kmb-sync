use kmb::uni::value::TypeCode;
use kmb::uni::{ArchiveDescription, ArchiveField, ArchiveGroup};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Value,
    Counter,
    HiddenData,
}

impl DataType {
    const fn name(self) -> &'static str {
        match self {
            Self::Value => "Value",
            Self::Counter => "Counter",
            Self::HiddenData => "HiddenData",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "Value" => Some(Self::Value),
            "Counter" => Some(Self::Counter),
            "HiddenData" => Some(Self::HiddenData),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    pub prop_name: String,
    pub user_name: String,
    pub unit: Option<String>,
    pub group: Option<String>,
    pub sub_group: Option<String>,
    pub data_type: DataType,
    pub big_endian: bool,
    pub mul: f32,
    pub info: u8,
    pub info2: u8,
    pub len: u16,
    pub type_ser: String,
    pub a_ind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualProp {
    pub xml: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeDescription {
    pub props: Vec<Prop>,
    pub virtuals: Vec<VirtualProp>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not supported archive configuration: no known record header")]
    Header,
    #[error("archive group {0} has no descriptor")]
    NoGroup(u8),
    #[error("stored TypeXML is not understood: {0}")]
    Xml(&'static str),
}

fn net_type(f: &ArchiveField) -> Option<(&'static str, usize)> {
    let pb = usize::from(f.pb);
    Some(match TypeCode::from_wire(f.type_code) {
        TypeCode::U8 if f.type_code == 0 => ("System.Byte", 1),
        TypeCode::U16 => ("System.UInt16", 2),
        TypeCode::U32 => ("System.UInt32", 4),
        TypeCode::U64 => ("System.UInt64", 8),
        TypeCode::F32 => ("System.Single", 4),
        TypeCode::F64 => ("System.Double", 8),
        TypeCode::S8 => ("System.SByte", 1),
        TypeCode::S16 => ("System.Int16", 2),
        TypeCode::S32 => ("System.Int32", 4),
        TypeCode::S64 => ("System.Int64", 8),
        TypeCode::U8Array => ("System.Byte[]", pb),
        TypeCode::U16Array => ("System.UInt16[]", pb.saturating_mul(2)),
        TypeCode::U32Array => ("System.UInt32[]", pb.saturating_mul(4)),
        TypeCode::F32Array => ("System.Single[]", pb.saturating_mul(4)),
        TypeCode::F64Array => ("System.Double[]", pb.saturating_mul(8)),
        TypeCode::String => ("System.String", pb),
        TypeCode::DateTime => ("System.DateTime", 8),
        TypeCode::U8 | TypeCode::S8Array | TypeCode::S16Array | TypeCode::S32Array => {
            return None;
        }
    })
    .filter(|&(_, size)| size > 0)
}

fn special_chars(s: &str) -> String {
    s.replace("{fi}", "φ")
        .replace("{d}", "Δ")
        .replace("{O}", "Ω")
        .replace("{1}", "ⅰ")
        .replace("{2}", "ⅱ")
        .replace("{3}", "ⅲ")
        .replace("{4}", "ⅳ")
}

fn check_string(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c < '\u{20}' && !matches!(c, '\t' | '\n' | '\r') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn blank(s: String) -> String {
    if s.trim().is_empty() {
        String::new()
    } else {
        s
    }
}

fn array_indexes(bitmap: &[u8]) -> Option<String> {
    let set: Vec<usize> = bitmap
        .iter()
        .enumerate()
        .flat_map(|(i, b)| {
            (0..8_usize)
                .filter(move |bit| (b >> bit) & 1 == 1)
                .map(move |bit| i.saturating_mul(8).saturating_add(bit).saturating_add(1))
        })
        .collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while let Some(&start) = set.get(i) {
        let mut end = start;
        while set.get(i.saturating_add(1)) == Some(&end.saturating_add(1)) {
            end = end.saturating_add(1);
            i = i.saturating_add(1);
        }
        parts.push(if end == start {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i = i.saturating_add(1);
    }
    (!parts.is_empty()).then(|| parts.join(";"))
}

fn class_of<'a>(
    desc: &'a ArchiveDescription,
    f: &ArchiveField,
    ic: u8,
) -> Option<&'a ArchiveGroup> {
    desc.groups
        .iter()
        .find(|g| g.idx == f.ic && g.idxs == f.ics && g.idxss == f.icss)
        .or_else(|| desc.groups.iter().find(|g| g.idx == ic))
}

fn parent_of<'a>(desc: &'a ArchiveDescription, c: &ArchiveGroup) -> Option<&'a ArchiveGroup> {
    (c.idxss != 0)
        .then(|| {
            desc.groups
                .iter()
                .find(|g| g.idx == c.idx && g.idxs == c.idxs && g.idxss == 0)
        })
        .flatten()
}

const HEADERS: [&[(&str, &str)]; 4] = [
    &[("Length", "System.UInt16"), ("Time", "System.DateTime")],
    &[
        ("Length", "System.UInt16"),
        ("Sum", "System.UInt16"),
        ("Time", "System.DateTime"),
    ],
    &[
        ("Length", "System.UInt16"),
        ("Sum", "System.UInt16"),
        ("AId", "System.UInt32"),
        ("Time", "System.DateTime"),
    ],
    &[
        ("Length", "System.UInt16"),
        ("Sum", "System.UInt16"),
        ("AId", "System.UInt32"),
        ("Ecc", "System.Byte[]"),
        ("Time", "System.DateTime"),
    ],
];

impl TypeDescription {
    pub fn from_descriptors(desc: &ArchiveDescription, ic: u8) -> Result<(Self, usize), Error> {
        if !desc.groups.iter().any(|g| g.idx == ic) {
            return Err(Error::NoGroup(ic));
        }
        let (with_fields, _) = Self::props_with_fields(desc, ic);
        let mut props: Vec<Prop> = with_fields.into_iter().map(|(p, _)| p).collect();
        let header = HEADERS
            .iter()
            .rev()
            .find(|h| {
                props.len() > h.len()
                    && props
                        .iter()
                        .zip(h.iter())
                        .all(|(p, (name, ty))| p.prop_name == *name && p.type_ser == *ty)
            })
            .ok_or(Error::Header)?;
        let mut remove = 0_usize;
        for (p, (name, _)) in props.iter().zip(header.iter()) {
            remove = remove.saturating_add(match *name {
                "Length" | "Sum" => 2,
                "AId" => 4,
                "Ecc" => usize::from(p.len),
                _ => 0,
            });
        }
        props.drain(..header.len());
        rename_duplicates(&mut props);
        Ok((
            Self {
                props,
                virtuals: Vec::new(),
            },
            remove,
        ))
    }

    #[must_use]
    pub fn props_with_fields(desc: &ArchiveDescription, ic: u8) -> (Vec<(Prop, usize)>, u32) {
        let mut out = Vec::new();
        let mut undefined = 0_u32;
        for (i, f) in desc.fields.iter().enumerate().filter(|(_, f)| f.ic == ic) {
            if f.zapis == 0 {
                continue;
            }
            let Some(class) = class_of(desc, f, ic) else {
                continue;
            };
            let Some((type_ser, _)) = net_type(f) else {
                continue;
            };
            out.push((prop(desc, f, class, type_ser, &mut undefined), i));
        }
        (out, undefined)
    }

    #[must_use]
    pub fn same_layout(&self, other: &Self) -> bool {
        let key = |p: &Prop| {
            (
                p.user_name.clone(),
                p.type_ser.clone(),
                p.len,
                p.info,
                p.info2,
                p.data_type,
            )
        };
        self.props.len() == other.props.len()
            && self.props.iter().map(key).eq(other.props.iter().map(key))
    }

    #[must_use]
    pub fn same_fields(&self, other: &Self) -> bool {
        self.props == other.props
    }
}

fn prop(
    desc: &ArchiveDescription,
    f: &ArchiveField,
    class: &ArchiveGroup,
    type_ser: &str,
    undefined: &mut u32,
) -> Prop {
    let mut unit = Some(class.unit.clone());
    let (mut group, mut sub_group) = if f.ics == 0 {
        (None, None)
    } else if let Some(parent) = parent_of(desc, class) {
        (Some(parent.name.clone()), Some(class.name.clone()))
    } else {
        (Some(class.name.clone()), None)
    };
    let mut name = f.name.clone();
    let mut a_ind = None;
    if f.info1 == 0 {
        for (codes, tag) in [([1, 21], "avg"), ([2, 22], "min"), ([3, 23], "max")] {
            if codes.contains(&f.info2) {
                if sub_group.is_none() {
                    sub_group = Some(tag.to_string());
                } else {
                    name = format!("{tag}.{name}");
                }
            }
        }
    } else if f.info1 == 5 && f.info2 == 200 {
        a_ind = f.zapis2.as_deref().and_then(array_indexes);
    }
    match name.rfind('[') {
        Some(0) => {
            name = String::new();
            unit = Some(String::new());
        }
        Some(i) => {
            unit = name.get(i.saturating_add(1)..).map(str::to_string);
            name.truncate(i);
        }
        None => {}
    }
    let mut name = blank(special_chars(&name));
    unit = unit.map(|u| blank(special_chars(&u)));
    group = group.map(|g| blank(special_chars(&g)));
    sub_group = sub_group.map(|s| blank(special_chars(&s)));
    if name.is_empty() {
        name = format!("Undefined_{undefined}");
        *undefined = undefined.saturating_add(1);
    }
    let data_type = match f.info1 {
        7 => DataType::Counter,
        10 => DataType::HiddenData,
        _ => DataType::Value,
    };
    let user_name = check_string(&name);
    let unit = unit.map(|u| check_string(&u));
    let group = group.map(|g| check_string(&g));
    let sub_group = sub_group.map(|s| check_string(&s));
    let mut prop_name = String::new();
    for part in [&group, &sub_group].into_iter().flatten() {
        if !part.is_empty() {
            prop_name.push_str(part);
            prop_name.push('_');
        }
    }
    prop_name.push_str(&user_name);
    Prop {
        prop_name,
        user_name,
        unit,
        group,
        sub_group,
        data_type,
        big_endian: true,
        mul: class.k,
        info: f.info1,
        info2: f.info2,
        len: f.pb,
        type_ser: type_ser.to_string(),
        a_ind,
    }
}

fn rename_duplicates(props: &mut [Prop]) {
    let mut seen = std::collections::HashSet::new();
    for p in props.iter_mut() {
        if p.prop_name.is_empty() {
            p.prop_name = "EMPTYNAME".into();
            p.user_name = "EMPTYNAME".into();
        }
        if !seen.contains(&p.prop_name) {
            seen.insert(p.prop_name.clone());
            continue;
        }
        for n in 2..1002 {
            let candidate = format!("{}({n})", p.prop_name);
            if !seen.contains(&candidate) {
                p.prop_name = candidate;
                p.user_name = format!("{}({n})", p.user_name);
                seen.insert(p.prop_name.clone());
                break;
            }
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[must_use]
pub fn net_float(v: f32) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "INF" } else { "-INF" }.into();
    }
    let e = format!("{v:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-5..7).contains(&exp) {
        return format!("{v}");
    }
    let sign = if exp < 0 { '-' } else { '+' };
    let digits = exp.unsigned_abs();
    format!("{mantissa}E{sign}{digits:02}")
}

impl TypeDescription {
    #[must_use]
    pub fn to_xml(&self) -> String {
        let mut x = String::from(
            "<?xml version=\"1.0\"?>\r\n<UniTypeDescription xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\r\n  <Props>",
        );
        for p in &self.props {
            x.push_str("\r\n    <PropDesc xsi:type=\"UniPropDesc\">");
            let mut el = |name: &str, value: Option<&str>| match value {
                None => {}
                Some("") => {
                    let _ = write!(x, "\r\n      <{name} />");
                }
                Some(v) => {
                    let _ = write!(x, "\r\n      <{name}>{}</{name}>", escape(v));
                }
            };
            el("PropName", Some(&p.prop_name));
            el("UserName", Some(&p.user_name));
            el("Unit", p.unit.as_deref());
            el("Group", p.group.as_deref());
            el("SubGroup", p.sub_group.as_deref());
            el("DataType", Some(p.data_type.name()));
            el(
                "BigEndian",
                Some(if p.big_endian { "true" } else { "false" }),
            );
            el("mul", Some(&net_float(p.mul)));
            el("info", Some(&p.info.to_string()));
            el("info2", Some(&p.info2.to_string()));
            el("len", Some(&p.len.to_string()));
            el("TypeSer", Some(&p.type_ser));
            el("AInd", p.a_ind.as_deref());
            x.push_str("\r\n    </PropDesc>");
        }
        x.push_str("\r\n  </Props>\r\n</UniTypeDescription>");
        x
    }

    pub fn from_sql_xml(xml: &str) -> Result<Self, Error> {
        let body = between(xml, "<Props>", "</Props>").ok_or(Error::Xml("no <Props>"))?;
        let mut props = Vec::new();
        let mut virtuals = Vec::new();
        let mut rest = body;
        while let Some(start) = rest.find("<PropDesc") {
            let tail = rest.get(start..).unwrap_or("");
            let end = tail
                .find("</PropDesc>")
                .ok_or(Error::Xml("unclosed <PropDesc>"))?;
            let whole_end = end.saturating_add("</PropDesc>".len());
            let element = tail.get(..whole_end).unwrap_or("");
            if element.contains("xsi:type=\"UniPropDesc\"") {
                props.push(parse_prop(element)?);
            } else {
                virtuals.push(VirtualProp {
                    xml: element.to_string(),
                });
            }
            rest = tail.get(whole_end..).unwrap_or("");
        }
        Ok(Self { props, virtuals })
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let from = s.find(open)?.saturating_add(open.len());
    let rest = s.get(from..)?;
    rest.get(..rest.find(close)?)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn element(xml: &str, name: &str) -> Option<String> {
    if xml.contains(&format!("<{name} />")) || xml.contains(&format!("<{name}/>")) {
        return Some(String::new());
    }
    between(xml, &format!("<{name}>"), &format!("</{name}>")).map(unescape)
}

fn parse_prop(xml: &str) -> Result<Prop, Error> {
    let need = |name: &'static str| element(xml, name).ok_or(Error::Xml(name));
    let num = |name: &'static str| -> Result<String, Error> { need(name) };
    Ok(Prop {
        prop_name: need("PropName")?,
        user_name: element(xml, "UserName").unwrap_or_default(),
        unit: element(xml, "Unit"),
        group: element(xml, "Group"),
        sub_group: element(xml, "SubGroup"),
        data_type: DataType::parse(&need("DataType")?).ok_or(Error::Xml("DataType"))?,
        big_endian: need("BigEndian")? == "true",
        mul: num("mul")?.parse().map_err(|_| Error::Xml("mul"))?,
        info: num("info")?.parse().map_err(|_| Error::Xml("info"))?,
        info2: num("info2")?.parse().map_err(|_| Error::Xml("info2"))?,
        len: num("len")?.parse().map_err(|_| Error::Xml("len"))?,
        type_ser: need("TypeSer")?,
        a_ind: element(xml, "AInd"),
    })
}

#[must_use]
pub fn envis_crc(text: &str) -> u32 {
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    envis_crc_bytes(&bytes)
}

#[must_use]
pub fn envis_crc_bytes(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(idx: u8, idxs: u8, idxss: u8, name: &str, unit: &str) -> ArchiveGroup {
        ArchiveGroup {
            idx,
            idxs,
            idxss,
            typ: 0,
            k: 1.0,
            unit: unit.into(),
            name: name.into(),
        }
    }

    fn field(name: &str, ics: u8, type_code: u8, pb: u16, info2: u8) -> ArchiveField {
        ArchiveField {
            name: name.into(),
            ic: 6,
            ics,
            icss: 0,
            info1: 0,
            info2,
            type_code,
            pb,
            zapis: 1,
            zapis2: None,
        }
    }

    fn example() -> ArchiveDescription {
        ArchiveDescription {
            groups: vec![group(6, 0, 0, "MainPQ", ""), group(6, 1, 0, "U", "V")],
            fields: vec![
                field("Length", 0, 1, 1, 0),
                field("Sum", 0, 1, 1, 0),
                field("AId", 0, 2, 1, 0),
                field("Ecc", 0, 10, 3, 0),
                field("Time", 0, 19, 1, 0),
                field("U1", 1, 4, 4, 1),
            ],
        }
    }

    const EXAMPLE_XML: &str = "<?xml version=\"1.0\"?>\r\n<UniTypeDescription xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\r\n  <Props>\r\n    <PropDesc xsi:type=\"UniPropDesc\">\r\n      <PropName>U_avg_U1</PropName>\r\n      <UserName>U1</UserName>\r\n      <Unit>V</Unit>\r\n      <Group>U</Group>\r\n      <SubGroup>avg</SubGroup>\r\n      <DataType>Value</DataType>\r\n      <BigEndian>true</BigEndian>\r\n      <mul>1</mul>\r\n      <info>0</info>\r\n      <info2>1</info2>\r\n      <len>4</len>\r\n      <TypeSer>System.Single</TypeSer>\r\n    </PropDesc>\r\n  </Props>\r\n</UniTypeDescription>";

    #[test]
    fn worked_example() {
        let (t, remove) = TypeDescription::from_descriptors(&example(), 6).unwrap();
        assert_eq!(remove, 11);
        assert_eq!(t.props.len(), 1);
        assert_eq!(t.props[0].prop_name, "U_avg_U1");
        assert_eq!(t.to_xml(), EXAMPLE_XML);
        let compact: String = EXAMPLE_XML
            .split("\r\n")
            .skip(1)
            .map(str::trim_start)
            .collect();
        assert_eq!(TypeDescription::from_sql_xml(&compact).unwrap(), t);
    }

    #[test]
    fn crc() {
        let xsi_first = EXAMPLE_XML.replace(
            "xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"",
            "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"",
        );
        assert_eq!(envis_crc(&xsi_first), 542_703_276);
    }

    #[test]
    fn names_units_and_arrays() {
        let mut d = example();
        d.fields.push(field("THD[%]", 1, 4, 4, 0));
        d.fields.push(field("{fi}1", 1, 4, 4, 0));
        d.fields.push(field("[x", 1, 4, 4, 0));
        d.fields.push(field("U1", 1, 4, 4, 1));
        d.fields.push(field("Neg", 1, 16, 2, 0));
        let (t, _) = TypeDescription::from_descriptors(&d, 6).unwrap();
        let names: Vec<&str> = t.props.iter().map(|p| p.prop_name.as_str()).collect();
        assert_eq!(
            names,
            ["U_avg_U1", "U_THD", "U_φ1", "U_Undefined_0", "U_avg_U1(2)"]
        );
        assert_eq!(t.props[1].unit.as_deref(), Some("%]"));
        assert_eq!(t.props[3].unit.as_deref(), Some(""));
        assert_eq!(
            array_indexes(&[0xFF, 0xFF, 0xFF, 0x05]),
            Some("1-25;27".into())
        );
        assert_eq!(array_indexes(&[0]), None);
    }

    #[test]
    fn floats() {
        assert_eq!(net_float(1.0), "1");
        assert_eq!(net_float(0.001), "0.001");
        assert_eq!(net_float(1e-6), "1E-06");
        assert_eq!(net_float(0.1), "0.1");
        assert_eq!(net_float(1000.0), "1000");
        assert_eq!(net_float(1e7), "1E+07");
        assert_eq!(net_float(1.5e-6), "1.5E-06");
    }
}
