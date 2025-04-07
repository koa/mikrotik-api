mod device;
pub mod error;
mod protocol;

pub mod simple;
pub mod prelude {
    use crate::{device, protocol};
    pub use device::{MikrotikDevice, ParsedMessage};
    pub use protocol::command::{CommandBuilder, QueryOperator};
    pub use protocol::word::{TrapCategory, TrapResult};
}
