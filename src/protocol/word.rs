use crate::protocol::error::ProtocolError;
use std::fmt;

#[derive(Debug, PartialEq)]
pub enum Word<'a> {
    /// A category word, such as `!done`, `!re`, `!trap`, or `!fatal`.
    Category(WordCategory),
    /// A tag word, such as `.tag=123`.
    Tag(u16),
    /// An attribute word, such as `=name=ether1`.
    Attribute {
        key: &'a [u8],
        value: Option<&'a [u8]>,
    },
    /// An unrecognized word. Usually this is a `!fatal` reason message.
    Message(&'a [u8]),
}
/// Represents the type of a word in a response.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum WordType {
    /// Tag word.
    Tag,
    /// Category word.
    Category,
    /// Attribute word.
    Attribute,
    /// Message word.
    Message,
}

impl Word<'_> {
    pub fn word_type(&self) -> WordType {
        match self {
            Word::Category(_) => WordType::Category,
            Word::Tag(_) => WordType::Tag,
            Word::Attribute { .. } => WordType::Attribute,
            Word::Message(_) => WordType::Message,
        }
    }
}

impl<'a> TryFrom<&'a [u8]> for Word<'a> {
    type Error = ProtocolError;

    fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
        if let Some(tag) = value.strip_prefix(b".tag=") {
            if !tag.is_ascii() {
                Err(ProtocolError::InvalidTagDigits(Box::from(tag)))
            } else {
                Ok(Word::Tag(String::from_utf8_lossy(tag).parse()?))
            }
        } else if let Some(attribute_content) = value.strip_prefix(b"=") {
            let mut parts = attribute_content.splitn(2, |&b| b == b'=');
            let key = parts.next().expect("Error taking first part");
            let value = parts.next();
            Ok(Word::Attribute { key, value })
        } else if let Some(category) = value.strip_prefix(b"!") {
            Ok(Word::Category(WordCategory::try_from(category)?))
        } else {
            Ok(Word::Message(value))
        }
    }
}

/// Represents the type of of a response.
/// The type is derived from the first [`Word`] in a [`Sentence`].
/// Valid types are `!done`, `!re`, `!trap`, and `!fatal`.
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum WordCategory {
    /// Represents a `!done` response.
    Done,
    /// Represents a `!re` response.
    Reply,
    /// Represents a `!trap` response.
    Trap,
    /// Represents a `!fatal` response.
    Fatal,
}

impl TryFrom<&[u8]> for WordCategory {
    type Error = ProtocolError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        match value {
            b"done" => Ok(Self::Done),
            b"re" => Ok(Self::Reply),
            b"trap" => Ok(Self::Trap),
            b"fatal" => Ok(Self::Fatal),
            _ => Err(ProtocolError::InvalidCategory(Box::from(value))),
        }
    }
}

impl fmt::Display for WordCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WordCategory::Done => "!done",
            WordCategory::Reply => "!re",
            WordCategory::Trap => "!trap",
            WordCategory::Fatal => "!fatal",
        })
    }
}
struct WordIterator<'a> {
    data: &'a [u8],
    idx: usize,
}
#[derive(Debug)]
enum WordParserItem<'a> {
    Terminator,
    Word(Word<'a>),
    Error(ProtocolError),
}
impl<'a> Iterator for WordIterator<'a> {
    type Item = WordParserItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.data.len() {
            None
        } else {
            match read_length(&self.data[self.idx..]) {
                Ok((length, inc)) => {
                    if length as usize + inc + self.idx > self.data.len() {
                        None
                    } else {
                        self.idx += inc;
                        Some(if length == 0 {
                            WordParserItem::Terminator
                        } else {
                            let current_word_pos = self.idx;
                            self.idx += length as usize;
                            match Word::try_from(&self.data[current_word_pos..self.idx]) {
                                Ok(w) => WordParserItem::Word(w),
                                Err(e) => WordParserItem::Error(e),
                            }
                        })
                    }
                }
                Err(e) => Some(WordParserItem::Error(e)),
            }
        }
    }
}

/// Returns the length and the number of bytes read.
fn read_length(data: &[u8]) -> Result<(u32, usize), ProtocolError> {
    let mut c: u32 = data[0] as u32;
    if c & 0x80 == 0x00 {
        Ok((c, 1))
    } else if c & 0xC0 == 0x80 {
        c &= !0xC0;
        c <<= 8;
        c += data[1] as u32;
        return Ok((c, 2));
    } else if c & 0xE0 == 0xC0 {
        c &= !0xE0;
        c <<= 8;
        c += data[1] as u32;
        c <<= 8;
        c += data[2] as u32;
        return Ok((c, 3));
    } else if c & 0xF0 == 0xE0 {
        c &= !0xF0;
        c <<= 8;
        c += data[1] as u32;
        c <<= 8;
        c += data[2] as u32;
        c <<= 8;
        c += data[3] as u32;
        return Ok((c, 4));
    } else if c & 0xF8 == 0xF0 {
        c = data[1] as u32;
        c <<= 8;
        c += data[2] as u32;
        c <<= 8;
        c += data[3] as u32;
        c <<= 8;
        c += data[4] as u32;
        return Ok((c, 5));
    } else {
        Err(ProtocolError::PrefixLength)
    }
}

pub fn next_sentence(data: &[u8]) -> Result<(Vec<Word>, usize), ProtocolError> {
    let mut iterator = WordIterator { data, idx: 0 };
    let mut sentence = Vec::new();
    while let Some(item) = iterator.next() {
        match item {
            WordParserItem::Terminator => {
                return Ok((sentence, iterator.idx));
            }
            WordParserItem::Word(w) => {
                sentence.push(w);
            }
            WordParserItem::Error(e) => return Err(e),
        }
    }
    Err(ProtocolError::Incomplete)
}

pub struct TrapResult<'a> {
    pub category: TrapCategory,
    pub message: &'a [u8],
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TrapCategory {
    MissingItemOrCommand,
    ArgumentValueFailure,
    ExecutionInterrupted,
    ScriptingError,
    GeneralError,
    ApiError,
    TtyError,
    ReturnValue,
}
impl TryFrom<&[u8]> for TrapCategory {
    type Error = ();

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        match value {
            b"0" => Ok(Self::MissingItemOrCommand),
            b"1" => Ok(Self::ArgumentValueFailure),
            b"2" => Ok(Self::ExecutionInterrupted),
            b"3" => Ok(Self::ScriptingError),
            b"4" => Ok(Self::GeneralError),
            b"5" => Ok(Self::ApiError),
            b"6" => Ok(Self::TtyError),
            b"7" => Ok(Self::ReturnValue),
            &_ => Err(()),
        }
    }
}
