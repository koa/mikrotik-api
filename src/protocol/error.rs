use crate::protocol::word::WordType;
use std::num::ParseIntError;
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum ProtocolError {
    #[error("Error parsing number of tag: {0}")]
    InvalidTag(#[from] ParseIntError),
    #[error("Error parsing tag: {content}",content=String::from_utf8_lossy(.0))]
    InvalidTagDigits(Box<[u8]>),
    #[error("Unknown Category: {content}",content=String::from_utf8_lossy(.0))]
    InvalidCategory(Box<[u8]>),
    #[error("There is an invalid length found")]
    PrefixLength,
    #[error("The next sentence is not yet complete")]
    Incomplete,
    #[error("The next sentence is closed but still incomplete: {0:?}")]
    IncompleteSentence(MissingWord),
    #[error("Word type found: {word:?}, expected: {expected:?}")]
    WordSequence {
        word: WordType,
        expected: &'static [WordType],
    },
    #[error("No response from tag {0} expected")]
    UnknownTag(u16),
    #[error("Unexpected Attribute in trap: {key}",key=String::from_utf8_lossy(.0))]
    InvalidAttributeInTrap(Box<[u8]>),
    #[error("Missing category field in trap")]
    MissingCategoryInTrap,
    #[error("Missing message field in trap")]
    MissingMessageInTrap,
}
/// Types of words that can be missing from a response.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MissingWord {
    /// Missing `.tag` in the response. All responses must have a tag.
    Tag,
    /// Missing category (`!done`, `!repl`, `!trap`, `!fatal`) in the response.
    Category,
    /// Missing message in a [`CommandResponse::FatalResponse`]
    Message,
}
