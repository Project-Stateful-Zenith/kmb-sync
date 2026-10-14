use kmb_envis::db::{Envis, Target};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [host, port, database, user, password_file, object] = a.as_slice() else {
        return Err("usage: ping HOST PORT DATABASE USER PASSWORD_FILE OBJECT".into());
    };
    let target = Target {
        host: host.clone(),
        port: port.parse()?,
        database: database.clone(),
        user: user.clone(),
        password: std::fs::read_to_string(password_file)?.trim().to_string(),
        trust_cert: true,
    };
    let mut db = Box::pin(Envis::connect(&target)).await?;
    println!("connected, database version accepted");
    for (meas, name, archive, last) in db.newest_rows(object).await? {
        println!("{meas:>4} {name:<28} archive {archive:>2}: newest row {last}");
    }
    Ok(())
}
