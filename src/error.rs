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
    #[error("Login failed")]
    LoginFailed,
}
