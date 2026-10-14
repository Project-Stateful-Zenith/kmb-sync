#![allow(
    clippy::arithmetic_side_effects,
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::or_fun_call,
    clippy::double_ended_iterator_last
)]

use kmb::uni::DataSet;
use kmb_envis::config::{data_blob, try_description_blob};
use kmb_envis::definition::envis_crc_bytes;

fn hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(s.get(i..i.saturating_add(2))?, 16).ok())
        .collect()
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: check_config HOST:PORT DESC_HEX DATA_HEX";
    let host = args.next().ok_or(usage)?;
    let desc = hex(&std::fs::read_to_string(args.next().ok_or(usage)?)?);
    let data = hex(&std::fs::read_to_string(args.next().ok_or(usage)?)?);
    let mut c = kmb_tokio::connect(host.as_str(), 1).await?;
    c.identify().await?;
    let config = c.read_description(DataSet::Config).await?;
    let ours_desc = try_description_blob(&config)?;
    let ours_data = data_blob(&config)?;
    println!(
        "description: ours {} B crc {}, stored {} B crc {}, first difference {:?}",
        ours_desc.len(),
        envis_crc_bytes(&ours_desc),
        desc.len(),
        envis_crc_bytes(&desc),
        first_difference(&ours_desc, &desc)
    );
    println!(
        "data: ours {} B crc {}, stored {} B crc {}, first difference {:?}",
        ours_data.len(),
        envis_crc_bytes(&ours_data),
        data.len(),
        envis_crc_bytes(&data),
        first_difference(&ours_data, &data)
    );
    let cfg_time = config.value("Message", "CfgSetTime");
    println!("CfgSetTime {cfg_time:?}");
    Ok(())
}
