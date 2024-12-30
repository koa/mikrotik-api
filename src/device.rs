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
use log::error;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, ToSocketAddrs},
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;

pub trait ParsedMessage: Send + 'static {
    fn parse_message(sentence: &[(&[u8], Option<&[u8]>)]) -> Self;
    fn process_error(error: &Error) -> Self;
    fn process_trap(result: TrapResult) -> Self;
}

#[derive(Debug, Clone)]
pub struct MikrotikDevice<D: ParsedMessage> {
    inner: Arc<InnerMikrotikDevice<D>>,
}

impl<D: ParsedMessage> MikrotikDevice<D> {
    fn create_command(&self, command: impl WordContent) -> CommandBuilder {
        let tag = self
            .inner
            .next_tag
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        CommandBuilder::new(tag, command)
    }
    pub async fn send_command<F: FnOnce(CommandBuilder) -> CommandBuilder>(
        &self,
        command: &[u8],
        command_builder: F,
    ) -> ReceiverStream<D> {
        let cmd = command_builder(self.create_command(command)).build();
        let (response_sender, response_receiver) = mpsc::channel(16);
        self.inner
            .command_tx_send
            .send((cmd, response_sender))
            .await
            .expect("Send command failed");
        ReceiverStream::new(response_receiver)
    }
    pub async fn send_simple_command(&self, command: &[u8]) -> ReceiverStream<D> {
        self.send_command(command, |cb| cb).await
    }
}

#[derive(Debug)]
struct InnerMikrotikDevice<D: ParsedMessage> {
    command_tx_send: mpsc::Sender<(Command, mpsc::Sender<D>)>,
    next_tag: AtomicU16,
}

impl<D: ParsedMessage> MikrotikDevice<D> {
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

        let mut running = true;
        // Split for independent read/write
        let (mut tcp_rx, mut tcp_tx) = stream.into_split();
        let (command_tx_send, mut command_tx_recv) =
            mpsc::channel::<(Command, mpsc::Sender<D>)>(16);
        let mut running_commands = HashMap::new();
        let tag_sequence: AtomicU16 = Default::default();

        let login_tag = tag_sequence.fetch_add(1, Ordering::Relaxed);
        let login_packet = CommandBuilder::login(login_tag, username, password);
        tcp_tx
            .write_all(login_packet.data.as_ref())
            .await
            .map_err(Arc::new)?;

        let mut packet_buf = Vec::new();
        loop {
            let read = tcp_rx.read_buf(&mut packet_buf).await.map_err(Arc::new)?;
            if read == 0 {
                running = false;
                break;
            } else {
                match next_sentence(&packet_buf) {
                    Ok((sentence, inc)) => {
                        if let &[Word::Category(WordCategory::Done), Word::Tag(_)] =
                            sentence.as_slice()
                        {
                            packet_buf = packet_buf.split_off(inc);
                            break;
                        } else {
                            Err(Error::LoginFailed)?;
                        }
                    }
                    Err(ProtocolError::Incomplete) => continue,
                    Err(e) => Err(Error::Protocol(e))?,
                }
            }
        }

        tokio::spawn(async move {
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
                                        if let Err(e)=process_sentence(&sentence, &mut running_commands).await{
                                            error!("Error processing sentence: {}", e);
                                            running = false;
                                        }
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
                        Some(( Command{ tag, data},response_queue)) => {
                            // Error writing the command to the device, shutdown the connection
                            match tcp_tx.write_all(&data).await {
                                Ok(_) => {
                                    // The command is sent, store the channel to send the responses back
                                    running_commands.insert(tag, response_queue);
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
                }
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
    running_commands: &mut HashMap<u16, mpsc::Sender<D>>,
    error: &Error,
) {
    for (_, queue) in running_commands.drain() {
        if let Err(send_error) = queue.send(D::process_error(error)).await {
            error!("Error processing error:  {:?} / {:?}", error, send_error);
        }
    }
}

async fn process_sentence<D: ParsedMessage>(
    sentence: &[Word<'_>],
    running_commands: &mut HashMap<u16, mpsc::Sender<D>>,
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
            send_message_back(
                running_commands,
                &mut found_tag,
                D::parse_message(&attributes),
            )
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
                    Word::Attribute { key, value: _ } => {
                        Err(ProtocolError::InvalidAttributeInTrap(Box::from(*key)))?
                    }
                    Word::Message(_) => Err(ProtocolError::WordSequence {
                        word: WordType::Message,
                        expected: &[WordType::Tag, WordType::Attribute],
                    })?,
                }
            }
            let message = match (found_category, found_message) {
                (Some(category), Some(message)) => {
                    D::process_trap(TrapResult { category, message })
                }
                (None, _) => {
                    D::process_error(&Error::Protocol(ProtocolError::MissingCategoryInTrap))
                }
                (_, None) => {
                    D::process_error(&Error::Protocol(ProtocolError::MissingMessageInTrap))
                }
            };
            send_message_back(running_commands, &mut found_tag, message).await?;
        }
        WordCategory::Fatal => {
            error!("Fatal error from device")
        }
    }
    Ok(())
}

async fn send_message_back<D: ParsedMessage>(
    running_commands: &mut HashMap<u16, mpsc::Sender<D>>,
    found_tag: &mut Option<u16>,
    message: D,
) -> Result<(), ProtocolError> {
    let tag = found_tag.ok_or(ProtocolError::IncompleteSentence(MissingWord::Tag))?;
    if let Err(e) = running_commands
        .get(&tag)
        .ok_or(ProtocolError::UnknownTag(tag))?
        .send(message)
        .await
    {
        error!("Cannot send response on tag {tag}: {:?}", e);
        running_commands.remove(&tag);
    }
    Ok(())
}
