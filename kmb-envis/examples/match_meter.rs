#![allow(
    clippy::arithmetic_side_effects,
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::or_fun_call,
    clippy::double_ended_iterator_last
)]

use kmb_envis::definition::TypeDescription;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let host = args
        .next()
        .ok_or("usage: match_meter HOST:PORT FILE [ID...]")?;
    let file = args
        .next()
        .ok_or("usage: match_meter HOST:PORT FILE [ID...]")?;
    let wanted: Vec<i64> = args.map(|a| a.parse()).collect::<Result<_, _>>()?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    let stored: Vec<(i64, TypeDescription)> = rows
        .iter()
        .filter_map(|r| {
            let t = TypeDescription::from_sql_xml(r["TypeXML"].as_str()?).ok()?;
            Some((r["Id"].as_i64()?, t))
        })
        .collect();
    let mut c = kmb_tokio::connect(host.as_str(), 1).await?;
    let id = c.identify().await?;
    println!("device {} sw {}", id.device_no, id.software_version);
    let desc = c.read_archive_description().await?;
    if let Ok(ic) = std::env::var("DUMP").map(|v| v.parse::<u8>()) {
        let ic = ic?;
        for g in desc.groups.iter().filter(|g| g.idx == ic) {
            println!("group {g:?}");
        }
        for f in desc.fields.iter().filter(|f| f.ic == ic) {
            println!("field {f:?}");
        }
        return Ok(());
    }
    for g in desc.groups.iter().filter(|g| g.idxs == 0 && g.idxss == 0) {
        match TypeDescription::from_descriptors(&desc, g.idx) {
            Err(e) => println!("{} (ic {}): {e}", g.name, g.idx),
            Ok((t, remove)) => {
                let same: Vec<i64> = stored
                    .iter()
                    .filter(|(_, s)| s.same_fields(&t))
                    .map(|(i, _)| *i)
                    .collect();
                let layout: Vec<i64> = stored
                    .iter()
                    .filter(|(_, s)| s.same_layout(&t))
                    .map(|(i, _)| *i)
                    .collect();
                println!(
                    "{} (ic {}): {} fields, header {remove} B, same fields {same:?}, same layout {layout:?}",
                    g.name,
                    g.idx,
                    t.props.len()
                );
                for (sid, s) in stored.iter().filter(|(i, _)| wanted.contains(i)) {
                    if s.props.len() != t.props.len() && !same.contains(sid) {
                        println!("  vs {sid}: {} stored fields", s.props.len());
                    }
                    if let Some((a, b)) = t.props.iter().zip(&s.props).find(|(a, b)| a != b) {
                        println!(
                            "  vs {sid}: first difference\n    ours   {a:?}\n    stored {b:?}"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
