mod code;
mod command;
mod link;
mod markdown;
pub(crate) mod math;
mod patch;
mod stream;
mod style;
mod table;
mod tool_display;
mod usage;
pub(crate) use code::*;
pub(crate) use command::*;
pub(crate) use link::*;
pub(crate) use markdown::*;
pub(crate) use patch::*;
pub(crate) use stream::*;
pub(crate) use style::*;
pub(crate) use table::*;
pub(crate) use tool_display::*;
pub(crate) use usage::*;

pub(crate) mod wait_spinner;

use crate::i18n::text as t;
use crate::llm::{ChatResult, ChatStreamChunk, ChatStreamKind, Usage};
use crate::render::wait_spinner::{braille_frame, SpinnerStyle, WaitSpinner, SPINNER_INTERVAL};
use crate::tools::CommandOutputStream;
use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, terminal};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Write};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningDisplayMode {
    Hidden,
    Summary,
    Full,
}

impl ReasoningDisplayMode {
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallDisplayMode {
    Hidden,
    Summary,
    Full,
}

impl ToolCallDisplayMode {
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

#[cfg(test)]
mod tests;
