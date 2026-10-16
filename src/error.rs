pub use crate::protocol::error::ProtocolError;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("IO Error: {0}")]
    Io(#[from] Arc<std::io::Error>),
    #[error("Protocol Error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("Connection unexpected closed")]
    ConnectionClosed,
    /// the other end closed the connection before answering the login (the device, or a
    /// proxy on the way that refuses it)
    #[error("Connection closed at the login, before the device answered it")]
    ClosedAtLogin,
    #[error("Login failed")]
    LoginFailed,
}
