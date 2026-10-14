#![allow(
    clippy::arithmetic_side_effects,
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::or_fun_call,
    clippy::double_ended_iterator_last
)]

use kmb_envis::definition::{TypeDescription, envis_crc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: check_definitions FILE")?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let (mut same, mut differ, mut formulas) = (0_u32, 0_u32, 0_u32);
    for r in &rows {
        let id = r["Id"].as_i64().unwrap_or(-1);
        let xml = r["TypeXML"].as_str().ok_or("TypeXML missing")?;
        let stored: Option<u32> = r["CRC"]
            .as_u64()
            .and_then(|c| u32::try_from(c).ok())
            .or_else(|| r["CRC"].as_str()?.parse().ok());
        let t = TypeDescription::from_sql_xml(xml)?;
        if !t.virtuals.is_empty() {
            formulas = formulas.saturating_add(1);
            println!(
                "{id}: {} fields, {} formula fields (not rebuilt)",
                t.props.len(),
                t.virtuals.len()
            );
            continue;
        }
        let crc = envis_crc(&t.to_xml());
        if Some(crc) == stored {
            same = same.saturating_add(1);
        } else {
            differ = differ.saturating_add(1);
            println!("{id}: CRC {crc} differs from stored {stored:?}");
        }
    }
    println!("{same} equal, {differ} different, {formulas} with formula fields");
    Ok(())
}
