use crate::{
    command::{Command, Effect, EffectKind},
    error::Error,
    prelude::CommandBuilder,
    protocol::{
        command::Command as ProtocolCommand,
        error::{MissingWord, ProtocolError},
        word::{next_sentence, TrapCategory, TrapResult, Word, WordCategory, WordType},
        WordContent, WordSequenceItem,
    },
};
use log::{debug, error};
use std::{
    collections::HashMap,
    fmt::Debug,
    pin::Pin,
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
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
struct Running {
    sender: mpsc::Sender<Reply>,
    reply_timeout: Option<Duration>,
    last_reply: Instant,
}

/// A command for the dispatch task.
struct Queued {
    command: ProtocolCommand,
    /// where its replies go
    replies: mpsc::Sender<Reply>,
    reply_timeout: Option<Duration>,
}

/// A reply of the device to a command as it came, before a [`ParsedMessage`] reads it: the
/// connection carries commands of any type, and a user may keep the replies to read them
/// again later, by another type too ([`Replies::unparsed`], [`Replies::kept`]).
#[derive(Debug, Clone)]
pub enum Reply {
    /// `!re`: its attributes in their order, a key may come twice
    Sentence(Box<[Attribute]>),
    Trap {
        category: Option<TrapCategory>,
        message: Box<[u8]>,
    },
    Error(Error),
}

/// An attribute of a reply (`=key=value`).
#[derive(Debug, Clone)]
pub struct Attribute {
    key: Box<[u8]>,
    value: Option<Box<[u8]>>,
}

impl Reply {
    /// Whether it is no answer of the device but the connection failing (closed, no reply in
    /// time): nothing to keep.
    pub fn is_failure(&self) -> bool {
        matches!(self, Reply::Error(_))
    }
    fn parse<D: ParsedMessage>(&self, context: &D::Context) -> D {
        match self {
            Reply::Sentence(attributes) => {
                let attributes: Vec<(&[u8], Option<&[u8]>)> = attributes
                    .iter()
                    .map(|attribute| (attribute.key.as_ref(), attribute.value.as_deref()))
                    .collect();
                D::parse_message(&attributes, context)
            }
            Reply::Trap { category, message } => D::process_trap(
                TrapResult {
                    category: *category,
                    message,
                },
                context,
            ),
            Reply::Error(error) => D::process_error(error, context),
        }
    }
}

/// The replies of one command, each read by `D`; ends with the command (`!done`).
pub struct Replies<D: ParsedMessage> {
    replies: Source,
    context: D::Context,
}

/// Where the replies come from.
enum Source {
    /// the device, as they arrive
    Device(ReceiverStream<Reply>),
    /// replies kept from an earlier command, from `next` on
    Kept { replies: Arc<[Reply]>, next: usize },
}

impl Source {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Reply>> {
        match self {
            Source::Device(replies) => Pin::new(replies).poll_next(cx),
            Source::Kept { replies, next } => {
                let reply = replies.get(*next).cloned();
                *next += 1;
                Poll::Ready(reply)
            }
        }
    }
}

impl<D: ParsedMessage> Replies<D> {
    /// The replies of an earlier command (from [`Replies::unparsed`]) read as `D` with
    /// `context`, as if the command came again.
    pub fn kept(replies: Arc<[Reply]>, context: D::Context) -> Self {
        Replies {
            replies: Source::Kept { replies, next: 0 },
            context,
        }
    }
    /// The replies as they come, unread, to keep.
    pub fn unparsed(self) -> Unparsed {
        Unparsed(self.replies)
    }
}

// the context is never pinned, only handed to `parse` by reference
impl<D: ParsedMessage> Unpin for Replies<D> {}

impl<D: ParsedMessage> Stream for Replies<D> {
    type Item = D;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<D>> {
        let this = self.get_mut();
        this.replies
            .poll_next(cx)
            .map(|reply| reply.map(|reply| reply.parse(&this.context)))
    }
}

/// The replies of one command unread ([`Replies::unparsed`]); ends with the command.
pub struct Unparsed(Source);

impl Stream for Unparsed {
    type Item = Reply;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Reply>> {
        self.get_mut().0.poll_next(cx)
    }
}
use tokio_stream::{wrappers::ReceiverStream, Stream};

pub trait ParsedMessage: Send + 'static {
    type Context: Send + 'static + Debug + Clone;
    fn parse_message(sentence: &[(&[u8], Option<&[u8]>)], context: &Self::Context) -> Self;
    fn process_error(error: &Error, context: &Self::Context) -> Self;
    fn process_trap(result: TrapResult, context: &Self::Context) -> Self;
}

/// A login on a device over one transport, for commands of any type
/// ([`Connection::run`]). It never connects again: once the transport ends, every command
/// answers [`Error::ConnectionClosed`] and [`Connection::is_closed`] is true. Cloning it gives
/// another handle on the same login.
#[derive(Debug, Clone)]
pub struct Connection {
    inner: Arc<InnerConnection>,
}

#[derive(Debug)]
struct InnerConnection {
    command_tx_send: mpsc::Sender<Queued>,
    next_tag: AtomicU16,
    generation: Generation,
}

/// The generation of a device's state: it goes up with every command that changes the
/// device ([`Effect::Write`], [`Effect::SessionBound`]), on every connection given the same
/// one. What was read at an older generation may be out of date.
#[derive(Debug, Clone, Default)]
pub struct Generation(Arc<AtomicU64>);

impl Generation {
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    /// Counts a change of the device made otherwise (a script running on it, a reboot).
    pub fn advance(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Connection {
    /// Whether the transport ended: every command answers [`Error::ConnectionClosed`].
    pub fn is_closed(&self) -> bool {
        self.inner.command_tx_send.is_closed()
    }

    /// The generation this connection counts its changes in.
    pub fn generation(&self) -> &Generation {
        &self.inner.generation
    }

    /// Sends `command`; its replies are read by its [`Command::Reply`]. A command that
    /// changes the device advances the [`Generation`] before it goes out.
    pub async fn run<C: Command>(&self, command: &C) -> Replies<C::Reply> {
        if matches!(
            <C::Effect as EffectKind>::EFFECT,
            Effect::Write | Effect::SessionBound
        ) {
            self.inner.generation.advance();
        }
        let tag = self.inner.next_tag.fetch_add(1, Ordering::Relaxed);
        let builder = command.build(CommandBuilder::new(tag, command.path()));
        self.queue(builder.build(), command.reply_timeout(), command.context())
            .await
    }

    async fn queue<D: ParsedMessage>(
        &self,
        command: ProtocolCommand,
        reply_timeout: Option<Duration>,
        context: D::Context,
    ) -> Replies<D> {
        let (replies, received) = mpsc::channel(16);
        // the connection is gone (the device closed it, or reading it failed): the command
        // answers that instead
        if let Err(mpsc::error::SendError(Queued { replies, .. })) = self
            .inner
            .command_tx_send
            .send(Queued {
                command,
                replies,
                reply_timeout,
            })
            .await
        {
            let _ = replies.try_send(Reply::Error(Error::ConnectionClosed));
        }
        Replies {
            replies: Source::Device(ReceiverStream::new(received)),
            context,
        }
    }
}

impl Connection {
    /// Connect over a plain TCP socket - the common case. See
    /// [`Connection::connect_with_stream`] for connecting over anything
    /// else (e.g. a stream already tunnelled through a SOCKS proxy). Its changes count in
    /// `generation`, which the device's other connections may share.
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
        generation: Generation,
    ) -> Result<Connection, Error> {
        let stream = TcpStream::connect(addr).await.map_err(Arc::new)?;
        stream.set_nodelay(true).map_err(Arc::new)?;
        Self::connect_with_stream(stream, username, password, generation).await
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
        generation: Generation,
    ) -> Result<Connection, Error> {
        let mut running = true;
        // Split for independent read/write
        let (mut tcp_rx, mut tcp_tx) = io::split(stream);
        let (command_tx_send, mut command_tx_recv) = mpsc::channel::<Queued>(16);
        let mut running_commands: HashMap<u16, Running> = HashMap::new();
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
                            [Word::Category(WordCategory::Done), Word::Tag(_), Word::Attribute {
                                key: b"ret",
                                value: Some(challenge),
                            }] if !answered_challenge => {
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
                                        }
                                            // nobody reads its replies anymore: the device
                                            // may stop sending them
                                            Ok(Some(Unread(tag))) => {
                                                let cancel_command = CommandBuilder::cancel(tag);
                                                let _ = tcp_tx.write_all(cancel_command.data.as_ref()).await;
                                            }
                                            Ok(None) => {}}
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
                        Some(Queued { command: ProtocolCommand { tag, data }, replies, reply_timeout }) => {
                            // Error writing the command to the device, shutdown the connection
                            match tcp_tx.write_all(&data).await {
                                Ok(_) => {
                                    // The command is sent, store the channel to send the responses back
                                    running_commands.insert(tag, Running {
                                        sender: replies,
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
                                    .send(Reply::Error(Error::ReplyTimeout(limit)))
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
            while let Some(Queued { replies, .. }) = command_tx_recv.recv().await {
                let _ = replies.send(Reply::Error(Error::ConnectionClosed)).await;
            }
            // Final attempt to gracefully close TCP
            let _ = tcp_tx.shutdown().await;
        });

        Ok(Connection {
            inner: Arc::new(InnerConnection {
                command_tx_send,
                next_tag: tag_sequence,
                generation,
            }),
        })
    }
}

async fn notify_error(running_commands: &mut HashMap<u16, Running>, error: &Error) {
    for (tag, command) in running_commands.drain() {
        // nobody reads its replies anymore: nobody to tell
        if command
            .sender
            .send(Reply::Error(error.clone()))
            .await
            .is_err()
        {
            debug!("tag {tag} unread at {error:?}");
        }
    }
}

/// A command whose replies nobody reads anymore (its [`Replies`] were dropped before the
/// end, e.g. after the first row): it is forgotten here and cancelled on the device.
struct Unread(u16);

async fn process_sentence(
    sentence: &[Word<'_>],
    running_commands: &mut HashMap<u16, Running>,
) -> Result<Option<Unread>, ProtocolError> {
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
                        attributes.push(Attribute {
                            key: Box::from(*key),
                            value: value.map(Box::from),
                        });
                    }
                    Word::Message(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Message,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                }
            }
            return send_message_back(
                running_commands,
                found_tag,
                Reply::Sentence(attributes.into_boxed_slice()),
            )
            .await;
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
            let reply = match found_message {
                Some(message) => Reply::Trap {
                    category: found_category,
                    message: Box::from(message),
                },
                None => Reply::Error(Error::Protocol(ProtocolError::MissingMessageInTrap)),
            };
            return send_message_back(running_commands, found_tag, reply).await;
        }
        WordCategory::Fatal => {
            error!("Fatal error from device")
        }
        WordCategory::Empty => {}
    }
    Ok(None)
}

async fn send_message_back(
    running_commands: &mut HashMap<u16, Running>,
    found_tag: Option<u16>,
    reply: Reply,
) -> Result<Option<Unread>, ProtocolError> {
    let tag = found_tag.ok_or(ProtocolError::IncompleteSentence(MissingWord::Tag))?;
    let command = running_commands
        .get_mut(&tag)
        .ok_or(ProtocolError::UnknownTag(tag))?;
    command.last_reply = Instant::now();
    if command.sender.send(reply).await.is_err() {
        debug!("nobody reads the replies of tag {tag} anymore: cancelled");
        running_commands.remove(&tag);
        return Ok(Some(Unread(tag)));
    }
    Ok(None)
}

/// The bytes of a hex string (the login challenge of RouterOS before 6.43).
fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    hex.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}
