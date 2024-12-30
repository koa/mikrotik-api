use crate::{
    error::Error,
    prelude::ParsedMessage,
    protocol::word::{TrapCategory, TrapResult},
};
use encoding_rs::mem::decode_latin1;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum SimpleResult {
    Sentence(HashMap<Box<str>, Option<Box<str>>>),
    Error(Error),
    Trap {
        category: TrapCategory,
        message: Box<str>,
    },
}

impl ParsedMessage for SimpleResult {
    fn parse_message<'a>(sentence: &[(&[u8], Option<&[u8]>)]) -> Self {
        let mut ret = HashMap::new();
        for (key, value) in sentence {
            ret.insert(
                Box::from(decode_latin1(key)),
                value.as_ref().map(|v| Box::from(decode_latin1(v))),
            );
        }
        SimpleResult::Sentence(ret)
    }

    fn process_error(error: &Error) -> Self {
        SimpleResult::Error(error.clone())
    }

    fn process_trap(TrapResult { category, message }: TrapResult) -> Self {
        SimpleResult::Trap {
            category,
            message: Box::from(decode_latin1(message)),
        }
    }
}