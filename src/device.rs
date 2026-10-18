use crate::{
    error::Error,
    prelude::CommandBuilder,
    protocol::{
        command::Command,
        error::{MissingWord, ProtocolError},
        word::{next_sentence, TrapCategory, TrapResult, Word, WordCategory, WordType},
        WordContent, WordSequenceItem,
    },
};
use log::{debug, error};
use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, ToSocketAddrs},
    sync::mpsc,
    time::{interval, timeout, Instant, MissedTickBehavior},
};

/// How long the device may take to answer the login.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a command may go without a reply before it fails with
/// [`Error::ReplyTimeout`] (and is cancelled on the device): every reply counts, so a long
/// command that keeps answering (a countdown, a monitor) goes on. Over a slow radio link a
/// connection can stall for good without the TCP connection ending.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// A command waiting for replies: where they go, and when it may give up.
struct Running<D: ParsedMessage> {
    sender: mpsc::Sender<D>,
    context: D::Context,
    reply_timeout: Option<Duration>,
    last_reply: Instant,
}

/// A command for the dispatch task: what to send, where its replies go, its reply timeout.
type Queued<D> = (Command, mpsc::Sender<D>, <D as ParsedMessage>::Context, Option<Duration>);
use tokio_stream::wrappers::ReceiverStream;

pub trait ParsedMessage: Send + 'static {
    type Context: Send + 'static + Debug + Clone;
    fn parse_message(sentence: &[(&[u8], Option<&[u8]>)], context: &Self::Context) -> Self;
    fn process_error(error: &Error, context: &Self::Context) -> Self;
    fn process_trap(result: TrapResult, context: &Self::Context) -> Self;
}

#[derive(Debug, Clone)]
pub struct MikrotikDevice<D: ParsedMessage> {
    inner: Arc<InnerMikrotikDevice<D>>,
}

impl<D: ParsedMessage> MikrotikDevice<D> {
    fn create_command<'a>(&self, command: impl Into<WordSequenceItem<'a>>) -> CommandBuilder {
        let tag = self.inner.next_tag.fetch_add(1, Ordering::Relaxed);
        CommandBuilder::new(tag, command)
    }
    /// Sends a command, which fails after [`REPLY_TIMEOUT`] without a reply.
    pub async fn send_command<F: FnOnce(CommandBuilder) -> CommandBuilder>(
        &self,
        command: impl Into<WordSequenceItem<'_>>,
        command_builder: F,
        context: D::Context,
    ) -> ReceiverStream<D> {
        self.send_command_with_reply_timeout(command, command_builder, context, Some(REPLY_TIMEOUT))
            .await
    }
    /// Sends a command that fails after `reply_timeout` without a reply; `None` waits as long
    /// as the connection lasts.
    pub async fn send_command_with_reply_timeout<F: FnOnce(CommandBuilder) -> CommandBuilder>(
        &self,
        command: impl Into<WordSequenceItem<'_>>,
        command_builder: F,
        context: D::Context,
        reply_timeout: Option<Duration>,
    ) -> ReceiverStream<D> {
        let cmd = command_builder(self.create_command(command)).build();
        let (response_sender, response_receiver) = mpsc::channel(16);
        // the connection is gone (the device closed it, or reading it failed): the command
        // answers that instead
        if let Err(mpsc::error::SendError((_, response_sender, context, _))) = self
            .inner
            .command_tx_send
            .send((cmd, response_sender, context, reply_timeout))
            .await
        {
            let _ = response_sender.try_send(D::process_error(&Error::ConnectionClosed, &context));
        }
        ReceiverStream::new(response_receiver)
    }
    pub async fn send_simple_command(
        &self,
        command: impl Into<WordSequenceItem<'_>>,
        context: D::Context,
    ) -> ReceiverStream<D> {
        self.send_command(command, |cb| cb, context).await
    }
}

#[derive(Debug)]
struct InnerMikrotikDevice<D: ParsedMessage> {
    command_tx_send: mpsc::Sender<Queued<D>>,
    next_tag: AtomicU16,
}

impl<D: ParsedMessage> MikrotikDevice<D> {
    /// Connect over a plain TCP socket - the common case. See
    /// [`MikrotikDevice::connect_with_stream`] for connecting over anything
    /// else (e.g. a stream already tunnelled through a SOCKS proxy).
    pub async fn connect<
        'u,
        'p,
        A: ToSocketAddrs,
        U: Into<WordSequenceItem<'u>>,
        P: Into<WordSequenceItem<'p>>,
    >(
        addr: A,
        username: U,
        password: Option<P>,
    ) -> Result<MikrotikDevice<D>, Error> {
        let stream = TcpStream::connect(addr).await.map_err(Arc::new)?;
        stream.set_nodelay(true).map_err(Arc::new)?;
        Self::connect_with_stream(stream, username, password).await
    }

    /// Log in (within [`LOGIN_TIMEOUT`]) and start the connection's background dispatch task
    /// over an already-established stream, instead of dialing a `TcpStream` directly.
    /// This is what lets a caller route the connection through something
    /// other than a direct TCP dial - e.g. a SOCKS-proxied stream reaching a
    /// device that is only reachable via another host's routing, the way an
    /// operator would reach it in a real deployment with a single management
    /// entry point.
    pub async fn connect_with_stream<
        'u,
        'p,
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        U: Into<WordSequenceItem<'u>>,
        P: Into<WordSequenceItem<'p>>,
    >(
        stream: S,
        username: U,
        password: Option<P>,
    ) -> Result<MikrotikDevice<D>, Error> {
        let mut running = true;
        // Split for independent read/write
        let (mut tcp_rx, mut tcp_tx) = io::split(stream);
        let (command_tx_send, mut command_tx_recv) = mpsc::channel::<Queued<D>>(16);
        let mut running_commands: HashMap<u16, Running<D>> = HashMap::new();
        let tag_sequence: AtomicU16 = Default::default();

        let bytes = |item: WordSequenceItem| {
            let mut buffer = Vec::with_capacity(item.byte_count());
            item.write_to_buffer(&mut buffer);
            buffer
        };
        let username = bytes(username.into());
        let password = password.map(|password| bytes(password.into()));
        let login_tag = tag_sequence.fetch_add(1, Ordering::Relaxed);
        let login_packet =
            CommandBuilder::login(login_tag, username.as_slice(), password.as_deref());
        let mut packet_buf = Vec::new();
        timeout(LOGIN_TIMEOUT, async {
        tcp_tx
            .write_all(login_packet.data.as_ref())
            .await
            .map_err(Arc::new)?;

        // RouterOS before 6.43 answers the login with a challenge (`=ret=`) instead of
        // logging in, and wants the MD5 of a zero byte, the password and the challenge back
        let mut answered_challenge = false;
        loop {
            let read = tcp_rx.read_buf(&mut packet_buf).await.map_err(Arc::new)?;
            if read == 0 {
                return Err(Error::ClosedAtLogin);
            } else {
                match next_sentence(&packet_buf) {
                    Ok((sentence, inc)) => match *sentence.as_slice() {
                        [Word::Category(WordCategory::Done), Word::Tag(_)] => {
                            packet_buf = packet_buf.split_off(inc);
                            break;
                        }
                        [
                            Word::Category(WordCategory::Done),
                            Word::Tag(_),
                            Word::Attribute {
                                key: b"ret",
                                value: Some(challenge),
                            },
                        ] if !answered_challenge => {
                            let challenge = decode_hex(challenge).ok_or(Error::LoginFailed)?;
                            packet_buf = packet_buf.split_off(inc);
                            let mut digest = vec![0];
                            digest.extend_from_slice(password.as_deref().unwrap_or_default());
                            digest.extend_from_slice(&challenge);
                            let response = format!("00{:x}", md5::compute(digest));
                            let tag = tag_sequence.fetch_add(1, Ordering::Relaxed);
                            let packet = CommandBuilder::login_response(
                                tag,
                                username.as_slice(),
                                response.as_bytes(),
                            );
                            tcp_tx
                                .write_all(packet.data.as_ref())
                                .await
                                .map_err(Arc::new)?;
                            answered_challenge = true;
                        }
                        _ => Err(Error::LoginFailed)?,
                    },
                    Err(ProtocolError::Incomplete) => continue,
                    Err(e) => Err(Error::Protocol(e))?,
                }
            }
        }
        Ok::<_, Error>(())
        })
        .await
        .map_err(|_| Error::LoginTimeout(LOGIN_TIMEOUT))??;

        tokio::spawn(async move {
            let mut check = interval(Duration::from_secs(1));
            check.set_missed_tick_behavior(MissedTickBehavior::Delay);
            while running {
                tokio::select! {
                    biased;
                      bytes_read = tcp_rx.read_buf(&mut packet_buf) => match bytes_read {
                        Ok(0) => {
                            // Device closed connection
                            notify_error(&mut running_commands, &Error::ConnectionClosed).await;
                            running = false;
                        }
                        Ok(_) => {
                            let mut offset=0;
                            loop{
                                match next_sentence(&packet_buf[offset..]){
                                    Ok((sentence, inc)) => {
                                        offset+=inc;
                                        match process_sentence(&sentence, &mut running_commands).await {
                                            Err(ProtocolError::UnknownTag(tag)) => {
                                debug!("Unknown tag received: {}", tag);
                            }
                                            Err(e) => {
                                            error!("Error processing sentence: {}", e);
                                            notify_error(&mut running_commands, &Error::Protocol(e)).await;
                                            running = false;
                                            break;
                                        }Ok(..) => {}}
                                    }
                                    Err(ProtocolError::Incomplete) => {
                                        if offset < packet_buf.len() {
                                            packet_buf= packet_buf.split_off(offset);
                                        }else{
                                            packet_buf.clear();
                                        }
                                        break;
                                    }
                                    Err(e) => {
                                        notify_error(&mut running_commands, &Error::Protocol(e)).await;
                                        running=false;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            // Error reading from the device, shutdown the connection
                            notify_error(&mut running_commands, &Error::Io(Arc::new(e))).await;
                            running=false;
                        }
                        },
                                            // Send commands to the device
                    maybe_actor_message = command_tx_recv.recv() => match maybe_actor_message {
                        Some(( Command{ tag, data},response_queue, context, reply_timeout)) => {
                            // Error writing the command to the device, shutdown the connection
                            match tcp_tx.write_all(&data).await {
                                Ok(_) => {
                                    // The command is sent, store the channel to send the responses back
                                    running_commands.insert(tag, Running {
                                        sender: response_queue,
                                        context,
                                        reply_timeout,
                                        last_reply: Instant::now(),
                                    });
                                }
                                Err(e) => {
                                    // Error writing the command to the device, notify every running command and shutdown the connection
                                    notify_error(&mut running_commands, &Error::Io(Arc::new(e))).await;
                                    running = false;
                                }
                            }
                        }
                        None => {
                            // The actor has been dropped, gracefully shutdown
                            // Cancel all running commands and shutdown the connection
                            for (tag, _) in running_commands.drain() {
                                let cancel_command = CommandBuilder::cancel(tag);
                                let _ = tcp_tx.write_all(cancel_command.data.as_ref()).await;
                            }
                            running = false;
                        }
                    },
                    _ = check.tick() => {
                        let now = Instant::now();
                        let timed_out: Vec<(u16, Duration)> = running_commands
                            .iter()
                            .filter_map(|(tag, command)| {
                                command
                                    .reply_timeout
                                    .filter(|limit| now.duration_since(command.last_reply) >= *limit)
                                    .map(|limit| (*tag, limit))
                            })
                            .collect();
                        for (tag, limit) in timed_out {
                            if let Some(command) = running_commands.remove(&tag) {
                                let _ = command
                                    .sender
                                    .send(D::process_error(&Error::ReplyTimeout(limit), &command.context))
                                    .await;
                            }
                            let cancel_command = CommandBuilder::cancel(tag);
                            let _ = tcp_tx.write_all(cancel_command.data.as_ref()).await;
                        }
                    }
                }
            }

            // commands sent meanwhile get no answer from the device anymore; one sent from now
            // on fails right away (`send_command`)
            command_tx_recv.close();
            while let Some((_, response_queue, context, _)) = command_tx_recv.recv().await {
                let _ = response_queue
                    .send(D::process_error(&Error::ConnectionClosed, &context))
                    .await;
            }
            // Final attempt to gracefully close TCP
            let _ = tcp_tx.shutdown().await;
        });

        let device = MikrotikDevice {
            inner: Arc::new(InnerMikrotikDevice {
                command_tx_send,
                next_tag: tag_sequence,
            }),
        };
        Ok(device)
    }
}

async fn notify_error<D: ParsedMessage>(
    running_commands: &mut HashMap<u16, Running<D>>,
    error: &Error,
) {
    for (_, command) in running_commands.drain() {
        if let Err(send_error) = command
            .sender
            .send(D::process_error(error, &command.context))
            .await
        {
            error!("Error processing error:  {:?} / {:?}", error, send_error);
        }
    }
}

async fn process_sentence<D: ParsedMessage>(
    sentence: &[Word<'_>],
    running_commands: &mut HashMap<u16, Running<D>>,
) -> Result<(), ProtocolError> {
    let mut sentence_iter = sentence.iter();
    let word = sentence_iter
        .next()
        .ok_or::<ProtocolError>(ProtocolError::IncompleteSentence(MissingWord::Category))?;

    let category = if let Word::Category(category) = word {
        Ok(*category)
    } else {
        Err(ProtocolError::WordSequence {
            word: word.word_type(),
            expected: &[WordType::Category],
        })
    }?;
    match category {
        WordCategory::Done => {
            let word = sentence_iter
                .next()
                .ok_or(ProtocolError::IncompleteSentence(MissingWord::Tag))?;
            let tag = if let Word::Tag(tag) = word {
                Ok(*tag)
            } else {
                Err(ProtocolError::WordSequence {
                    word: word.word_type(),
                    expected: &[WordType::Tag],
                })
            }?;
            running_commands.remove(&tag);
        }
        WordCategory::Reply => {
            let mut found_tag = None;
            let mut attributes = Vec::new();
            for word in sentence_iter.by_ref() {
                match word {
                    Word::Category(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Category,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                    Word::Tag(tag) => {
                        found_tag = Some(*tag);
                    }
                    Word::Attribute { key, value } => {
                        attributes.push((*key, *value));
                    }
                    Word::Message(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Message,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                }
            }
            send_message_back(running_commands, &mut found_tag, |context| {
                D::parse_message(&attributes, context)
            })
            .await?;
        }
        WordCategory::Trap => {
            let mut found_category = None;
            let mut found_message = None;
            let mut found_tag = None;
            for word in sentence_iter {
                match word {
                    Word::Category(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Category,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                    Word::Tag(tag) => {
                        found_tag = Some(*tag);
                    }
                    Word::Attribute {
                        key: b"category",
                        value: Some(value),
                    } => {
                        if let Ok(category) = TrapCategory::try_from(*value) {
                            found_category = Some(category);
                        }
                    }
                    Word::Attribute {
                        key: b"message",
                        value,
                    } => {
                        found_message = *value;
                    }
                    // a trap concerns its command only: an attribute this crate doesn't know
                    // (a newer RouterOS) is no reason to end the connection
                    Word::Attribute { key, value } => {
                        debug!(
                            "trap attribute ignored: {}={:?}",
                            String::from_utf8_lossy(key),
                            value.map(String::from_utf8_lossy)
                        );
                    }
                    Word::Message(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Message,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                }
            }
            send_message_back(running_commands, &mut found_tag, |context| {
                match (found_category, found_message) {
                    (category, Some(message)) => {
                        D::process_trap(TrapResult { category, message }, context)
                    }
                    (_, None) => D::process_error(
                        &Error::Protocol(ProtocolError::MissingMessageInTrap),
                        context,
                    ),
                }
            })
            .await?;
        }
        WordCategory::Fatal => {
            error!("Fatal error from device")
        }
        WordCategory::Empty => {}
    }
    Ok(())
}

async fn send_message_back<D: ParsedMessage, F: FnOnce(&D::Context) -> D>(
    running_commands: &mut HashMap<u16, Running<D>>,
    found_tag: &mut Option<u16>,
    message_builder: F,
) -> Result<(), ProtocolError> {
    let tag = found_tag.ok_or(ProtocolError::IncompleteSentence(MissingWord::Tag))?;
    let command = running_commands
        .get_mut(&tag)
        .ok_or(ProtocolError::UnknownTag(tag))?;
    command.last_reply = Instant::now();
    if let Err(e) = command.sender.send(message_builder(&command.context)).await {
        error!("Cannot send response on tag {tag}: {:?}", e);
        running_commands.remove(&tag);
    }
    Ok(())
}

/// The bytes of a hex string (the login challenge of RouterOS before 6.43).
fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    hex.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}
