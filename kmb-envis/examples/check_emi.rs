#![allow(
    clippy::arithmetic_side_effects,
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::or_fun_call,
    clippy::double_ended_iterator_last
)]

use kmb::archive::uni::{Layout, header_version};
use kmb::uni::DataSet;
use kmb_envis::definition::TypeDescription;
use kmb_envis::emi::{LbLayout, feeders};
use serde_json::Value;

fn load(dir: &str, name: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(format!(
        "{dir}/{name}.json"
    ))?)?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: check_emi HOST:PORT FIXTURE_DIR OBJECT";
    let host = args.next().ok_or(usage)?;
    let dir = args.next().ok_or(usage)?;
    let object = args.next().ok_or(usage)?;
    let objects = load(&dir, "objects")?;
    let identities = load(&dir, "identities")?;
    let measurements = load(&dir, "measurements")?;
    let edges = load(&dir, "pack_edges")?;
    let defs = load(&dir, "definitions")?;
    let object_id = objects
        .iter()
        .find(|o| o["objekt"] == object.as_str())
        .and_then(|o| o["Id"].as_i64())
        .ok_or("object not in fixtures")?;

    let mut c = kmb_tokio::connect(host.as_str(), 1).await?;
    c.identify().await?;
    let config = c.read_description(DataSet::Config).await?;
    if std::env::var("DUMP").is_ok() {
        for (i, f) in config.fields.iter().enumerate() {
            let group = config
                .groups
                .iter()
                .find(|g| g.idx == f.ic)
                .map(|g| g.name.as_str());
            if group.is_some_and(|g| g.starts_with("Slaves")) {
                let v = config.values.get(i).cloned().flatten();
                let shown = format!("{v:?}");
                println!(
                    "{:?} {} type {} pb {} = {}",
                    group,
                    f.name,
                    f.type_code,
                    f.pb,
                    shown.get(..120).unwrap_or(&shown)
                );
            }
        }
    }
    let archives = c.read_archive_description().await?;
    let list = feeders(&config)?;
    let lb_ic = archives
        .groups
        .iter()
        .find(|g| g.idxs == 0 && g.idxss == 0 && g.name == "LB")
        .ok_or("no LB archive")?
        .idx;
    let layout = LbLayout::new(&archives, lb_ic)?;
    let version = header_version(&archives.fields, None);
    let lb = Layout::new(&archives, 25, version, 15_000)?;
    let (_, time_at) = TypeDescription::from_descriptors(&archives, lb_ic)?;
    let geometry = c.geometry(&config, &lb);
    let (mut same, mut differ) = (0_u32, 0_u32);
    for f in &list {
        let ident = identities.iter().find(|i| {
            i["objekt"].as_i64() == Some(object_id)
                && i["DeviceNo"].as_u64() == Some(u64::from(f.identity.device_no))
                && i["DeviceTypeDB"].as_u64() == Some(u64::from(f.identity.device_type))
        });
        let row_differs = ident.is_some_and(|i| {
            let n = |k: &str| i[k].as_u64().unwrap_or(u64::MAX);
            let r = &f.identity;
            let subs = [
                "SubDeviceTypeDB",
                "SubDeviceType2DB",
                "SubDeviceType3DB",
                "SubDeviceType4DB",
                "SubDeviceType5DB",
                "SubDeviceType6DB",
            ];
            n("SoftwareVersion") != u64::from(r.software_version)
                || n("HardwareVersion") != u64::from(r.hardware_version)
                || n("DeviceAddr") != u64::from(r.device_addr)
                || n("BootloaderVersion") != u64::from(r.bootloader_version)
                || subs
                    .iter()
                    .zip(r.sub_device_type)
                    .any(|(k, v)| n(k) != u64::from(v))
        });
        if row_differs {
            println!("   identity row differs: ours {:?}", f.identity);
        }
        let meas = ident.and_then(|i| {
            measurements
                .iter()
                .find(|m| m["identifyDB"] == i["Id"] && m["measName"] == f.name.as_str())
        });
        println!(
            "slot {:>2} {:<20} dev {} type {:#x}: identity {}, measurement {}",
            f.slot,
            f.name,
            f.identity.device_no,
            f.identity.device_type,
            ident.map_or("MISSING".into(), |i| i["Id"].to_string()),
            meas.map_or("MISSING".into(), |m| m["Id"].to_string())
        );
        let Some(meas_id) = meas.and_then(|m| m["Id"].as_i64()) else {
            continue;
        };
        for arch in [0_i64, 6] {
            let Some(edge) = edges.iter().find(|e| {
                e["keymeasName"].as_i64() == Some(meas_id) && e["keyArchID"].as_i64() == Some(arch)
            }) else {
                continue;
            };
            let stored: Vec<u8> = (0..edge["LastRow"].as_str().unwrap_or("").len())
                .step_by(2)
                .filter_map(|i| {
                    u8::from_str_radix(edge["LastRow"].as_str()?.get(i..i + 2)?, 16).ok()
                })
                .collect();
            let t = u64::from_be_bytes(stored.get(..8).ok_or("short row")?.try_into()?);
            let packs = load(&dir, "packs")?;
            let def_id = packs
                .iter()
                .filter(|p| {
                    p["keymeasName"].as_i64() == Some(meas_id)
                        && p["keyArchID"].as_i64() == Some(arch)
                })
                .last()
                .and_then(|p| p["ArchDef"].as_i64())
                .ok_or("no pack")?;
            let xml = defs
                .iter()
                .find(|d| d["Id"].as_i64() == Some(def_id))
                .and_then(|d| d["TypeXML"].as_str())
                .ok_or("no definition")?;
            let td = TypeDescription::from_sql_xml(xml)?;
            let at = c.record_at_time(25, kmb::KmbTime(t)).await?;
            let Some(slot) = at.slot else {
                println!("   arch {arch}: record of {t} no longer on the meter");
                continue;
            };
            let rec = c.read_records(&lb, slot..slot + 1, geometry).await?;
            let Some(Ok(bytes)) = rec.into_iter().next().map(|r| r.bytes) else {
                println!("   arch {arch}: record unreadable");
                continue;
            };
            let ours = layout.row(&bytes, time_at, f.slot, &td.props);
            if ours.as_deref() == Some(stored.as_slice()) {
                same += 1;
            } else {
                differ += 1;
                let first = ours
                    .as_ref()
                    .and_then(|o| o.iter().zip(&stored).position(|(a, b)| a != b));
                println!(
                    "   arch {arch} def {def_id}: DIFFERENT (ours {:?} B, stored {} B, first difference at {first:?})",
                    ours.as_ref().map(Vec::len),
                    stored.len()
                );
            }
        }
    }
    println!(
        "{} feeders; rows equal {same}, different {differ}",
        list.len()
    );
    Ok(())
}
