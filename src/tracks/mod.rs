//! The track model (`docs/track-sheets.md`): what identifies a track (its AudioId, see
//! `analysis/audio_id.rs`), what the analyzers measured (observations), what someone stated
//! (the sheet, a `.codj` file per track) and the view that combines them.

// wired to the bus in a later step
#![cfg_attr(not(test), allow(dead_code))]

pub mod sections;
pub mod sheet;
pub mod store;
pub mod view;

use serde::Serialize;

/// Who made an observation: a change of any part makes it stale (I7)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Producer {
    pub analyzer: String,
    pub version: u32,
    pub params: String,
}

/// I4: "not measured" and "measured, nothing found" are different values
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Value<T> {
    Unknown,
    Known(T),
}

impl<T> Value<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Value::Known(v) => Some(v),
            Value::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Observation<T> {
    pub producer: Producer,
    pub value: Value<T>,
}
