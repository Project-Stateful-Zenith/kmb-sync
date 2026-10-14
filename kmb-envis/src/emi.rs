use crate::db::IdentityRow;
use crate::definition::TypeDescription;
use kmb::uni::{ArchiveDescription, Description, Value};
use std::collections::HashMap;

pub const SLAVE_TIMEOUT: u32 = 0x0020_0000;

#[derive(Clone, Debug, PartialEq)]
pub struct Feeder {
    pub slot: u32,
    pub name: String,
    pub identity: IdentityRow,
    pub inom: f32,
    pub ct: u8,
}

fn bytes(d: &Description, group: &str, name: &str) -> Option<Vec<u8>> {
    let one = |n: &str| match d.value(group, n)? {
        Value::U8Array(b) => Some(b.clone()),
        _ => None,
    };
    if let Some(b) = one(name) {
        return Some(b);
    }
    let mut out = Vec::new();
    for part in 1.. {
        match one(&format!("{name}_{part}")) {
            Some(b) => out.extend(b),
            None if part == 1 => return None,
            None => break,
        }
    }
    Some(out)
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn module_identity(b: &[u8], wide: bool, empty: bool) -> Option<IdentityRow> {
    if empty {
        return None;
    }
    let (device_no, at): (u32, usize) = if wide {
        (be32(b, 0)?, 4)
    } else {
        (u32::from(be16(b, 0)?), 2)
    };
    let u = |i: usize| be16(b, at.checked_add(i)?);
    let mut sub = [0_u16; 6];
    for (k, s) in sub.iter_mut().enumerate() {
        *s = u(12_usize.checked_add(k.checked_mul(2)?)?)?;
    }
    Some(IdentityRow {
        device_no,
        device_type: u(0)?,
        props_type: u(2)?,
        software_version: u(4)?,
        hardware_version: u(6)?,
        software_modules: u(8)?,
        device_addr: *b.get(at.checked_add(10)?)?,
        bootloader_version: *b.get(at.checked_add(11)?)?,
        sub_device_type: sub,
        archives: 0,
    })
}

const fn fallback_identity(serial: u32) -> IdentityRow {
    IdentityRow {
        device_no: serial,
        device_type: 2,
        props_type: 96,
        software_version: 0,
        hardware_version: 0,
        software_modules: 128,
        device_addr: 0,
        bootloader_version: 0,
        sub_device_type: [0, 0, 1280, 4, 0, 0],
        archives: 0,
    }
}

#[allow(clippy::too_many_lines)]
pub fn feeders(config: &Description) -> Result<Vec<Feeder>, String> {
    let extended = !config.groups.iter().any(|g| g.name == "Slaves");
    let section = if extended { "SlavesExt" } else { "Slaves" };
    if !config.groups.iter().any(|g| g.name == section) {
        return Err("the configuration has no Slaves section".into());
    }
    let num = |name: &str| {
        config
            .value(section, name)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0)
    };
    let version = num("Version");
    let count = num(if extended { "CntCfg" } else { "Cnt" });
    if count == 0 || (extended && num("CntData") == 0) {
        return Err(format!("{section} configures no modules"));
    }
    let (ident_slots, per_module) = if extended {
        (count.saturating_mul(2), count)
    } else {
        (count, count / 2)
    };
    let get = |name: &str| bytes(config, section, name).unwrap_or_default();
    let (ident, status, ident_s, install) =
        (get("Ident"), get("Status"), get("IdentS"), get("Install"));
    let (ident, status, ident_s, install) = (&ident[..], &status[..], &ident_s[..], &install[..]);
    let stride = |b: &[u8], n: usize| b.len().checked_div(n).unwrap_or(0);
    let (ident_stride, status_stride) = (stride(ident, ident_slots), stride(status, count));
    let (names_stride, install_stride) = (stride(ident_s, per_module), stride(install, per_module));
    let wide = version >= 4;
    let id_len = if wide { 28 } else { 26 };
    let extra = if wide { 2 } else { 0 };
    let mut out = Vec::new();
    let mut slot = 0_u32;
    for i in 0..count / 2 {
        let at = |stride: usize, off: usize| stride.saturating_mul(i).saturating_add(off);
        let status_at = usize::from(!extended);
        let module_status = status
            .get(at(status_stride, status_at))
            .copied()
            .unwrap_or(0);
        let names: Vec<String> = (0..13)
            .map(|n: usize| {
                let base = n.saturating_mul(20).saturating_add(3);
                let start = at(names_stride, base.saturating_add(extra));
                let end = at(names_stride, base.saturating_add(20));
                ident_s
                    .get(start..end)
                    .map(kmb::decode::kmb_string)
                    .unwrap_or_default()
            })
            .collect();
        let serial = if wide {
            be32(ident_s, at(names_stride, 1))
        } else {
            be16(ident_s, at(names_stride, 1)).map(u32::from)
        }
        .unwrap_or(0);
        let raw = ident
            .get(at(ident_stride, 0)..at(ident_stride, id_len))
            .unwrap_or_default();
        let empty = if extended {
            raw.iter().take(6).all(|&x| x == 0)
        } else {
            raw.iter().all(|&x| x == 0)
        };
        let identity =
            module_identity(raw, wide, empty).unwrap_or_else(|| fallback_identity(serial));
        let ins = install.get(at(install_stride, 0)..).unwrap_or_default();
        let ct = |j: usize| ins.get(j).copied().unwrap_or(0);
        let inom = |n: usize| {
            be32(ins, n.saturating_mul(17).saturating_add(16)).map_or(0.0, f32::from_bits)
        };
        for (j, number) in (0..4_usize).zip(1_u16..) {
            if module_status & (16_u8 << j) == 0 {
                continue;
            }
            let this = slot;
            slot = slot.saturating_add(1);
            if ct(j) == 1 || ct(j) == 5 {
                continue;
            }
            let feeder = names
                .get(j.saturating_mul(3).saturating_add(1))
                .cloned()
                .unwrap_or_default();
            let module = names.first().cloned().unwrap_or_default();
            let name = if feeder.is_empty() {
                format!("{module}.F{number}")
            } else {
                format!("{module}.{feeder}")
            };
            let mut identity = identity.clone();
            identity.device_type |= number << 8;
            identity.archives = 65;
            if identity.device_no == 65534 {
                continue;
            }
            out.push(Feeder {
                slot: this,
                name,
                identity,
                inom: inom(j.saturating_mul(3)),
                ct: ct(j),
            });
        }
    }
    Ok(out)
}

fn localbus_names(field: &str) -> Option<[Option<String>; 4]> {
    let phases = |f: &dyn Fn(usize) -> String, sum: Option<String>| {
        Some([Some(f(1)), Some(f(2)), Some(f(3)), sum])
    };
    let pqs = |x: &str, group: &str| {
        let g = if group.is_empty() {
            format!("{x}_avg_")
        } else {
            format!("{group}/{x}_avg_")
        };
        phases(&|n| format!("{g}{x}{n}"), Some(format!("{g}3{x}")))
    };
    let energy = |group: &str, name: &str| {
        phases(
            &|n| format!("T/{group}_{name}{n}"),
            Some(format!("T/{group}_{name}")),
        )
    };
    match field {
        "I" => phases(&|n| format!("I_avg_I{n}"), None),
        "P" | "Q" | "S" | "D" => pqs(field, ""),
        "THDi" => phases(&|n| format!("Harmonics/THDI_avg_THDI{n}"), None),
        "Pfh" | "Qfh" => pqs(field, "Harmonics"),
        "Ifh" => phases(&|n| format!("Harmonics/Ifh_I_{n}fh"), None),
        "Ifi" => phases(&|n| format!("Harmonics/Ifh_φI_{n}fh"), None),
        "Idc" => phases(&|n| format!("DC/I_avg_I{n}dc"), None),
        "EPi" => energy("Aq", "+Ai"),
        "EPii" => energy("Aq", "-Ac"),
        "EPiii" => energy("Aq", "-Ai"),
        "EPiv" => energy("Aq", "+Ac"),
        "EQi" => energy("Rq", "+Ri"),
        "EQii" => energy("Rq", "+Rc"),
        "EQiii" => energy("Rq", "-Ri"),
        "EQiv" => energy("Rq", "-Rc"),
        _ => None,
    }
}

#[derive(Clone, Debug, Default)]
pub struct LbLayout {
    master: HashMap<String, (usize, usize)>,
    local: HashMap<String, usize>,
    currents: Vec<usize>,
    array_pb: u16,
}

impl LbLayout {
    pub fn new(desc: &ArchiveDescription, ic: u8) -> Result<Self, String> {
        let lb_sub = desc
            .groups
            .iter()
            .find(|g| g.idx == ic && g.name == "LocalBus")
            .map(|g| g.idxs)
            .ok_or("the LB archive has no LocalBus group")?;
        let (props, _) = TypeDescription::props_with_fields(desc, ic);
        let names: HashMap<usize, String> =
            props.into_iter().map(|(p, f)| (f, p.prop_name)).collect();
        let mut out = Self::default();
        let mut offset = 0_usize;
        for (i, f) in desc.fields.iter().enumerate() {
            if f.ic != ic || f.zapis == 0 {
                continue;
            }
            let width = f
                .typ()
                .width(f.pb)
                .max(if f.type_code == 19 { 8 } else { 0 });
            if f.ics == lb_sub {
                if let Some(elements) = localbus_names(&f.name) {
                    out.array_pb = f.pb;
                    for (e, name) in elements.iter().enumerate() {
                        if let Some(name) = name {
                            out.local
                                .insert(name.clone(), offset.saturating_add(e.saturating_mul(4)));
                        }
                    }
                    if f.name == "I" {
                        out.currents = (0..3_usize)
                            .map(|e| offset.saturating_add(e.saturating_mul(4)))
                            .collect();
                    }
                }
            } else if let Some(name) = names.get(&i) {
                out.master.insert(name.clone(), (offset, width));
            }
            offset = offset.saturating_add(width);
        }
        Ok(out)
    }

    #[must_use]
    pub fn fits(&self, archive: u8, props: &[crate::definition::Prop]) -> bool {
        let energy = props.first().is_some_and(|p| p.prop_name.starts_with("T/"));
        !props.is_empty()
            && energy == (archive == 6)
            && props.iter().all(|p| match self.local.get(&p.prop_name) {
                Some(_) => p.len == self.array_pb,
                None => self.master.contains_key(&p.prop_name),
            })
    }

    #[must_use]
    pub fn row(
        &self,
        record: &[u8],
        time_at: usize,
        slot: u32,
        props: &[crate::definition::Prop],
    ) -> Option<Vec<u8>> {
        let shift = usize::try_from(slot).ok()?.checked_mul(16)?;
        let mut row = record.get(time_at..time_at.checked_add(8)?)?.to_vec();
        let all_nan = !self.currents.is_empty()
            && self.currents.iter().all(|&o| {
                record
                    .get(o.saturating_add(shift)..o.saturating_add(shift).saturating_add(4))
                    .and_then(|b| b.try_into().ok())
                    .is_some_and(|b: [u8; 4]| f32::from_be_bytes(b).is_nan())
            });
        for p in props {
            if let Some(&o) = self.local.get(&p.prop_name) {
                row.extend_from_slice(
                    record
                        .get(o.saturating_add(shift)..o.saturating_add(shift).saturating_add(4))?,
                );
            } else if let Some(&(o, w)) = self.master.get(&p.prop_name) {
                let bytes = record.get(o..o.checked_add(w)?)?;
                if p.prop_name == "Status_Measurement_Flags" && all_nan {
                    let v = u32::from_be_bytes(bytes.try_into().ok()?) | SLAVE_TIMEOUT;
                    row.extend_from_slice(&v.to_be_bytes());
                } else {
                    row.extend_from_slice(bytes);
                }
            } else {
                return None;
            }
        }
        Some(row)
    }
}

#[must_use]
pub fn feeder_config(cfg_time: u64, unom: f32, f: &Feeder) -> (Vec<u8>, Vec<u8>) {
    let mut a = Vec::new();
    for (idx, name) in [(0_u8, "Message"), (2, "Install")] {
        a.extend([idx, 0]);
        a.extend_from_slice(name.as_bytes());
        a.push(0);
    }
    a.push(0xFF);
    let mut b = Vec::new();
    for (name, ic, typ, pb) in [
        ("CfgSetTime", 0_u8, 3_u8, 4_u16),
        ("Unom", 2, 4, 4),
        ("Inom", 2, 4, 4),
        ("cType", 2, 0, 2),
    ] {
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        b.extend([ic, typ]);
        b.extend_from_slice(&pb.to_be_bytes());
        b.push(1);
    }
    let desc = crate::config::blocks(&[&a, &b]).unwrap_or_default();
    let mut data = cfg_time.to_be_bytes().to_vec();
    data.extend(unom.to_be_bytes());
    data.extend(f.inom.to_be_bytes());
    data.push(f.ct);
    (desc, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeder_description_lengths() {
        let f = Feeder {
            slot: 0,
            name: "M.F1".into(),
            identity: fallback_identity(1),
            inom: 100.0,
            ct: 2,
        };
        let (desc, data) = feeder_config(5, 230.0, &f);
        let mut rest = desc.as_slice();
        for _ in 0..2 {
            let (len, tail) = rest.split_at(2);
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
            assert!(len > 0 && len <= tail.len());
            rest = &tail[len..];
        }
        assert_eq!(rest, [0_u8; 0]);
        assert_eq!(data.len(), 8 + 4 + 4 + 1);
    }

    #[test]
    fn module_identity_slot() {
        let mut b = 12_345_u32.to_be_bytes().to_vec();
        for v in [2_u16, 96, 5617, 1, 128] {
            b.extend(v.to_be_bytes());
        }
        b.extend([1, 43]);
        for v in [0_u16, 0, 0x500, 4, 0, 0x200] {
            b.extend(v.to_be_bytes());
        }
        assert_eq!(b.len(), 28);
        let id = module_identity(&b, true, false).unwrap();
        assert_eq!(
            id,
            IdentityRow {
                device_no: 12_345,
                device_type: 2,
                props_type: 96,
                software_version: 5617,
                hardware_version: 1,
                software_modules: 128,
                device_addr: 1,
                bootloader_version: 43,
                sub_device_type: [0, 0, 0x500, 4, 0, 0x200],
                archives: 0,
            }
        );
        let short: Vec<u8> = b.get(2..).unwrap().to_vec();
        assert_eq!(
            module_identity(&short, false, false).unwrap().device_no,
            12_345
        );
        assert_eq!(module_identity(&b, true, true), None);
    }
}
