//! What a command does ([`Effect`]) and what it sends ([`Command`]), for
//! [`Connection::run`](crate::prelude::Connection::run).
use crate::{
    device::{ParsedMessage, REPLY_TIMEOUT},
    protocol::command::CommandBuilder,
    simple::SimpleResult,
};
use std::{borrow::Cow, marker::PhantomData, time::Duration};

/// What a command does to the device.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Effect {
    /// reads configuration or state, without side effect; may be cached (`print`)
    Read,
    /// reads live values, without side effect; never cached (`monitor once`, `/ping`)
    Observe,
    /// changes the device (`add`, `set`, `remove`, `move`, `/execute`, `reboot`)
    Write,
    /// acts only while its connection lasts (`/system/device-mode/update`, `monitor`)
    SessionBound,
}

/// The [`Effect`] of a command as a type, so a caller can take only commands of one effect
/// (`C: Command<Effect = Read>`).
pub trait EffectKind {
    const EFFECT: Effect;
}

/// [`Effect::Read`]
#[derive(Debug, Copy, Clone)]
pub struct Read;
/// [`Effect::Observe`]
#[derive(Debug, Copy, Clone)]
pub struct Observe;
/// [`Effect::Write`]
#[derive(Debug, Copy, Clone)]
pub struct Write;
/// [`Effect::SessionBound`]
#[derive(Debug, Copy, Clone)]
pub struct SessionBound;

impl EffectKind for Read {
    const EFFECT: Effect = Effect::Read;
}
impl EffectKind for Observe {
    const EFFECT: Effect = Effect::Observe;
}
impl EffectKind for Write {
    const EFFECT: Effect = Effect::Write;
}
impl EffectKind for SessionBound {
    const EFFECT: Effect = Effect::SessionBound;
}

/// A command to a device: its path, its attributes, what it does and how its replies read.
pub trait Command {
    type Effect: EffectKind;
    type Reply: ParsedMessage;
    /// the whole path (`/system/resource/print`)
    fn path(&self) -> Cow<'_, [u8]>;
    /// adds the attributes
    fn build(&self, builder: CommandBuilder) -> CommandBuilder {
        builder
    }
    /// what reading its replies needs
    fn context(&self) -> <Self::Reply as ParsedMessage>::Context;
    /// how long it may go without a reply; `None` as long as the connection lasts
    fn reply_timeout(&self) -> Option<Duration> {
        Some(REPLY_TIMEOUT)
    }
}

/// A command without a type of its own, read as [`SimpleResult`]. A code smell: each one is
/// a command missing from mikrotik-model's catalog; if they multiply, the concept of typed
/// commands doesn't carry.
#[derive(Debug, Clone)]
pub struct RawCommand<E: EffectKind> {
    path: Box<[u8]>,
    attributes: Vec<RawAttribute>,
    reply_timeout: Option<Duration>,
    effect: PhantomData<E>,
}

#[derive(Debug, Clone)]
struct RawAttribute {
    key: Box<[u8]>,
    value: Box<[u8]>,
}

impl<E: EffectKind> RawCommand<E> {
    pub fn new(path: impl Into<Box<[u8]>>) -> Self {
        Self {
            path: path.into(),
            attributes: Vec::new(),
            reply_timeout: Some(REPLY_TIMEOUT),
            effect: PhantomData,
        }
    }
    pub fn attribute(mut self, key: impl Into<Box<[u8]>>, value: impl Into<Box<[u8]>>) -> Self {
        self.attributes.push(RawAttribute {
            key: key.into(),
            value: value.into(),
        });
        self
    }
    /// how long it may go without a reply ([`REPLY_TIMEOUT`] unless set); `None` as long as
    /// the connection lasts
    pub fn reply_timeout(mut self, reply_timeout: Option<Duration>) -> Self {
        self.reply_timeout = reply_timeout;
        self
    }
}

impl<E: EffectKind> Command for RawCommand<E> {
    type Effect = E;
    type Reply = SimpleResult;
    fn path(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.path)
    }
    fn build(&self, builder: CommandBuilder) -> CommandBuilder {
        self.attributes
            .iter()
            .fold(builder, |builder, RawAttribute { key, value }| {
                builder.attribute(key.as_ref(), value.as_ref())
            })
    }
    fn context(&self) {}
    fn reply_timeout(&self) -> Option<Duration> {
        self.reply_timeout
    }
}
