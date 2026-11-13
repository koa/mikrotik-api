//! One connection runs commands of any type, each read by its own `Reply`: a raw command and
//! a typed read share the login. Only a command that changes the device advances the
//! generation.
use mikrotik_api::command::{Read, Write};
use mikrotik_api::error::Error;
use mikrotik_api::prelude::{
    Command, Connection, Generation, ParsedMessage, RawCommand, TrapResult,
};
use mikrotik_api::simple::SimpleResult;
use std::borrow::Cow;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt;

/// One API word: its length (below 0x80 a single byte), then the word.
fn word(text: &str) -> Vec<u8> {
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// A reply read as the keys of its attributes, in their order.
#[derive(Debug, PartialEq)]
enum Keys {
    Sentence(Vec<Box<[u8]>>),
    Trap(Box<[u8]>),
    Error,
}

impl ParsedMessage for Keys {
    type Context = ();

    fn parse_message(sentence: &[(&[u8], Option<&[u8]>)], _: &()) -> Self {
        Keys::Sentence(sentence.iter().map(|(key, _)| Box::from(*key)).collect())
    }

    fn process_error(_: &Error, _: &()) -> Self {
        Keys::Error
    }

    fn process_trap(result: TrapResult, _: &()) -> Self {
        Keys::Trap(Box::from(result.message))
    }
}

/// `/interface/print`, read as [`Keys`].
struct InterfaceKeys;

impl Command for InterfaceKeys {
    type Effect = Read;
    type Reply = Keys;
    fn path(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(b"/interface/print")
    }
    fn context(&self) {}
}

#[tokio::test]
async fn raw_and_typed_commands_share_one_connection() {
    let (client, mut device) = tokio::io::duplex(4096);
    let fake = tokio::spawn(async move {
        let mut buffer = [0u8; 256];
        let _ = device.read(&mut buffer).await.unwrap();
        let done = [word("!done"), word(".tag=0"), vec![0]].concat();
        device.write_all(&done).await.unwrap();
        // two commands, answered in the order they came
        let mut received = Vec::new();
        while !String::from_utf8_lossy(&received).contains(".tag=2") {
            let read = device.read(&mut buffer).await.unwrap();
            received.extend_from_slice(&buffer[..read]);
        }
        let replies = [
            word("!re"),
            word(".tag=1"),
            word("=name=lab"),
            vec![0],
            word("!done"),
            word(".tag=1"),
            vec![0],
            word("!re"),
            word(".tag=2"),
            word("=b=1"),
            word("=a=2"),
            vec![0],
            word("!trap"),
            word(".tag=2"),
            word("=message=no such item"),
            vec![0],
            word("!done"),
            word(".tag=2"),
            vec![0],
        ]
        .concat();
        device.write_all(&replies).await.unwrap();
        device
    });
    let generation = Generation::default();
    let connection =
        Connection::connect_with_stream(client, b"admin", Some(b""), generation.clone())
            .await
            .expect("login");
    let identity = connection
        .run(&RawCommand::<Read>::new(
            b"/system/identity/print".as_slice(),
        ))
        .await;
    let keys = connection.run(&InterfaceKeys).await;
    let identity: Vec<_> = identity.collect().await;
    let keys: Vec<_> = keys.collect().await;
    assert!(
        matches!(identity.as_slice(), [SimpleResult::Sentence(row)]
            if row.get("name").and_then(|v| v.as_deref()) == Some("lab")),
        "{identity:?}"
    );
    assert_eq!(
        keys,
        [
            Keys::Sentence(vec![Box::from(*b"b"), Box::from(*b"a")]),
            Keys::Trap(Box::from(*b"no such item")),
        ]
    );
    assert_eq!(generation.current(), 0, "reading changes nothing");
    let _device = fake.await.unwrap();
}

#[tokio::test]
async fn a_change_advances_the_generation_of_every_connection_sharing_it() {
    let generation = Generation::default();
    let mut connections = Vec::new();
    for _ in 0..2 {
        let (client, mut device) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut buffer = [0u8; 256];
            let _ = device.read(&mut buffer).await.unwrap();
            let done = [word("!done"), word(".tag=0"), vec![0]].concat();
            device.write_all(&done).await.unwrap();
            // every further command: done at once
            let mut tag = 1;
            while device.read(&mut buffer).await.unwrap_or(0) > 0 {
                let done = [word("!done"), word(&format!(".tag={tag}")), vec![0]].concat();
                device.write_all(&done).await.unwrap();
                tag += 1;
            }
        });
        connections.push(
            Connection::connect_with_stream(client, b"admin", Some(b""), generation.clone())
                .await
                .expect("login"),
        );
    }
    let set = RawCommand::<Write>::new(b"/system/identity/set".as_slice())
        .attribute(b"name".as_slice(), b"lab".as_slice());
    let _: Vec<_> = connections[0].run(&set).await.collect().await;
    assert_eq!(generation.current(), 1);
    let _: Vec<_> = connections[1].run(&set).await.collect().await;
    assert_eq!(generation.current(), 2);
    assert_eq!(connections[0].generation().current(), 2);
}
