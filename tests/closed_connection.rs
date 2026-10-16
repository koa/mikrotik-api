//! A command on a connection the device closed answers an error instead of panicking or
//! hanging.
use mikrotik_api::prelude::MikrotikDevice;
use mikrotik_api::simple::SimpleResult;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt;

/// One API word: its length (below 0x80 a single byte), then the word.
fn word(text: &str) -> Vec<u8> {
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

#[tokio::test]
async fn a_command_after_the_device_closed_the_connection_fails() {
    let (client, mut device) = tokio::io::duplex(4096);
    let fake = tokio::spawn(async move {
        let mut login = [0u8; 256];
        let _ = device.read(&mut login).await.unwrap();
        // the login is done, then the device goes away
        let done = [word("!done"), word(".tag=0"), vec![0]].concat();
        device.write_all(&done).await.unwrap();
        drop(device);
    });
    let api = MikrotikDevice::<SimpleResult>::connect_with_stream(client, b"admin", Some(b""))
        .await
        .expect("login");
    fake.await.unwrap();
    // queued while the connection closes, and sent once it is closed
    for _ in 0..2 {
        let mut answer = api
            .send_simple_command(b"/system/identity/print".as_slice(), ())
            .await;
        let first = tokio::time::timeout(Duration::from_secs(5), answer.next())
            .await
            .expect("no answer");
        assert!(matches!(first, Some(SimpleResult::Error(_))), "{first:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A device (or a proxy on the way) that closes the connection instead of answering the login:
/// the connect fails, rather than handing out a connection whose every command fails.
#[tokio::test]
async fn a_connection_closed_at_the_login_fails_to_connect() {
    let (client, mut device) = tokio::io::duplex(4096);
    let fake = tokio::spawn(async move {
        let mut login = [0u8; 256];
        let _ = device.read(&mut login).await.unwrap();
        drop(device);
    });
    let result =
        MikrotikDevice::<SimpleResult>::connect_with_stream(client, b"admin", Some(b"")).await;
    fake.await.unwrap();
    assert!(
        matches!(result, Err(mikrotik_api::error::Error::ClosedAtLogin)),
        "{:?}",
        result.err()
    );
}
