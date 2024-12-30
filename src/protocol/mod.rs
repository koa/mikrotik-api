use std::borrow::Cow;

pub mod command;
pub mod error;
pub mod word;

/// a data type can be written as a word into miktrotik API
pub trait WordContent {
    /// count of bytes to be written
    fn byte_count(&self) -> usize;
    /// write the bytes
    fn write_to_buffer(&self, buffer: &mut Vec<u8>);
}
impl WordContent for [u8] {
    fn byte_count(&self) -> usize {
        self.len()
    }
    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(self);
    }
}
impl WordContent for &[u8] {
    fn byte_count(&self) -> usize {
        self.len()
    }
    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(self);
    }
}
impl<const N: usize> WordContent for [u8; N] {
    fn byte_count(&self) -> usize {
        N
    }
    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(self);
    }
}
impl<const N: usize> WordContent for &[u8; N] {
    fn byte_count(&self) -> usize {
        N
    }
    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(*self);
    }
}

impl WordContent for &str {
    fn byte_count(&self) -> usize {
        self.bytes().len()
    }
    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        assert!(
            self.is_ascii(),
            "There is a non ascii character in the string"
        );
        buffer.extend_from_slice(self.as_bytes());
    }
}

pub enum WordSequenceItem<'a> {
    DataRef(&'a [u8]),
    Data(Box<[u8]>),
    Sequence(&'a [WordSequenceItem<'a>]),
}
impl<'a> From<&'a [u8]> for WordSequenceItem<'a> {
    fn from(value: &'a [u8]) -> Self {
        WordSequenceItem::DataRef(value)
    }
}
impl<'a, const N: usize> From<&'a [u8; N]> for WordSequenceItem<'a> {
    fn from(value: &'a [u8; N]) -> Self {
        WordSequenceItem::DataRef(value)
    }
}
impl<'a> From<&'a [WordSequenceItem<'a>]> for WordSequenceItem<'a> {
    fn from(value: &'a [WordSequenceItem<'a>]) -> Self {
        WordSequenceItem::Sequence(value)
    }
}
impl<'a, const N: usize> From<&'a [WordSequenceItem<'a>; N]> for WordSequenceItem<'a> {
    fn from(value: &'a [WordSequenceItem<'a>; N]) -> Self {
        WordSequenceItem::Sequence(value)
    }
}
impl<'a> From<Cow<'a, [u8]>> for WordSequenceItem<'a> {
    fn from(value: Cow<'a, [u8]>) -> Self {
        match value {
            Cow::Borrowed(d) => WordSequenceItem::DataRef(d),
            Cow::Owned(d) => WordSequenceItem::Data(d.into()),
        }
    }
}
impl WordContent for WordSequenceItem<'_> {
    fn byte_count(&self) -> usize {
        match self {
            WordSequenceItem::DataRef(d) => d.len(),
            WordSequenceItem::Sequence(parts) => parts.iter().map(|x| x.byte_count()).sum(),
            WordSequenceItem::Data(d) => d.len(),
        }
    }

    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        match self {
            WordSequenceItem::DataRef(d) => {
                buffer.extend_from_slice(d);
            }
            WordSequenceItem::Sequence(parts) => {
                for item in parts.iter() {
                    item.write_to_buffer(buffer);
                }
            }
            WordSequenceItem::Data(d) => {
                buffer.extend_from_slice(d);
            }
        }
    }
}
impl WordContent for [WordSequenceItem<'_>] {
    fn byte_count(&self) -> usize {
        self.iter().map(|x| x.byte_count()).sum()
    }

    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        self.iter().for_each(|x| x.write_to_buffer(buffer));
    }
}
impl<const N: usize> WordContent for [WordSequenceItem<'_>; N] {
    fn byte_count(&self) -> usize {
        self.iter().map(|x| x.byte_count()).sum()
    }

    fn write_to_buffer(&self, buffer: &mut Vec<u8>) {
        self.iter().for_each(|x| x.write_to_buffer(buffer));
    }
}
