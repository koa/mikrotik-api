pub mod command;
mod device;
pub mod error;
mod protocol;

pub mod simple;
pub mod prelude {
    pub use crate::command::{Command, Effect, EffectKind, RawCommand};
    pub use crate::error::Error;
    use crate::{device, protocol};
    pub use device::{
        Connection, Generation, ParsedMessage, Replies, LOGIN_TIMEOUT, REPLY_TIMEOUT,
    };
    pub use protocol::command::{CommandBuilder, QueryOperator};
    pub use protocol::word::{TrapCategory, TrapResult};
}
