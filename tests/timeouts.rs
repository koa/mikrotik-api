//! A device (or a way to it) that stops answering: the login and a command fail after their
//! time limit instead of waiting for good; a command that keeps replying goes on.
use mikrotik_api::prelude::{LOGIN_TIMEOUT, REPLY_TIMEOUT};
use mikrotik_api::error::Error;
use mikrotik_api::prelude::MikrotikDevice;
use mikrotik_api::simple::SimpleResult;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::Instant;
use tokio_stream::StreamExt;

/// One API word: its length (below 0x80 a single byte), then the word.
fn word(text: &str) -> Vec<u8> {
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Reads the login and answers it.
async fn logged_in(device: &mut DuplexStream) {
    let mut login = [0u8; 256];
    let _ = device.read(&mut login).await.unwrap();
    let done = [word("!done"), word(".tag=0"), vec![0]].concat();
    device.write_all(&done).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_login_times_out() {
    let (client, _device) = tokio::io::duplex(4096);
    let start = Instant::now();
    let result =
        MikrotikDevice::<SimpleResult>::connect_with_stream(client, b"admin", Some(b"")).await;
    assert!(matches!(result, Err(Error::LoginTimeout(_))), "{:?}", result.err());
    assert!(start.elapsed() >= LOGIN_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_command_times_out_and_is_cancelled() {
    let (client, mut device) = tokio::io::duplex(4096);
    let fake = tokio::spawn(async move {
        logged_in(&mut device).await;
        // the command, then (after the timeout) its cancel; never an answer
        let mut received = Vec::new();
        let mut buffer = [0u8; 256];
        while !String::from_utf8_lossy(&received).contains("/cancel") {
            let read = device.read(&mut buffer).await.unwrap();
            assert!(read > 0, "connection closed before the cancel");
            received.extend_from_slice(&buffer[..read]);
        }
        device
    });
    let api = MikrotikDevice::<SimpleResult>::connect_with_stream(client, b"admin", Some(b""))
        .await
        .expect("login");
    let start = Instant::now();
    let mut answer = api
        .send_simple_command(b"/system/identity/print".as_slice(), ())
        .await;
    let first = answer.next().await;
    assert!(
        matches!(first, Some(SimpleResult::Error(Error::ReplyTimeout(_)))),
        "{first:?}"
    );
    assert!(start.elapsed() >= REPLY_TIMEOUT);
    assert!(start.elapsed() < REPLY_TIMEOUT + Duration::from_secs(5));
    let _device = fake.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_command_that_keeps_replying_goes_on() {
    let (client, mut device) = tokio::io::duplex(4096);
    let fake = tokio::spawn(async move {
        logged_in(&mut device).await;
        let mut command = [0u8; 256];
        let _ = device.read(&mut command).await.unwrap();
        // a reply every minute for five minutes, then the end
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let reply = [word("!re"), word(".tag=1"), word("=status=counting"), vec![0]].concat();
            device.write_all(&reply).await.unwrap();
        }
        let done = [word("!done"), word(".tag=1"), vec![0]].concat();
        device.write_all(&done).await.unwrap();
        device
    });
    let api = MikrotikDevice::<SimpleResult>::connect_with_stream(client, b"admin", Some(b""))
        .await
        .expect("login");
    let answers: Vec<_> = api
        .send_simple_command(b"/system/device-mode/update".as_slice(), ())
        .await
        .collect()
        .await;
    assert_eq!(answers.len(), 5, "{answers:?}");
    assert!(answers.iter().all(|a| matches!(a, SimpleResult::Sentence(_))), "{answers:?}");
    let _device = fake.await.unwrap();
}
