use mikrotik_api::prelude::MikrotikDevice;
use std::net::IpAddr;

use clap::Parser;
use encoding_rs::mem::encode_latin1_lossy;
use mikrotik_api::simple::SimpleResult;
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
    let device: MikrotikDevice<SimpleResult> = MikrotikDevice::connect(
        (args.device, 8728),
        b"admin",
        args.password.as_deref().map(|v| encode_latin1_lossy(v)),
    )
    .await?;
    let mut stream = device
        .send_simple_command(b"/system/resource/print", ())
        .await;
    while let Some(result) = stream.next().await {
        println!("Result: {result:?}");
    }
    Ok(())
}
