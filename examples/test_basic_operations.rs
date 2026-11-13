use mikrotik_api::command::Read;
use mikrotik_api::prelude::{Connection, Generation, RawCommand};
use std::net::IpAddr;

use clap::Parser;
use encoding_rs::mem::encode_latin1_lossy;
use tokio_stream::StreamExt;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// device to contact
    device: IpAddr,

    /// login password
    #[arg(short, long)]
    password: Option<Box<str>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let device = Connection::connect(
        (args.device, 8728),
        b"admin",
        args.password.as_deref().map(|v| encode_latin1_lossy(v)),
        Generation::default(),
    )
    .await?;
    let mut stream = device
        .run(&RawCommand::<Read>::new(
            b"/system/resource/print".as_slice(),
        ))
        .await;
    while let Some(result) = stream.next().await {
        println!("Result: {result:?}");
    }
    Ok(())
}
