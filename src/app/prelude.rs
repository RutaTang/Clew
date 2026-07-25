//! Shared imports for the `App` handler / service modules (`crate::app::*`).
//! Each such module also does `use crate::*;` for the types, enums, consts and
//! free functions defined in the crate root; this prelude adds the sibling-crate
//! and sibling-module re-exports plus the std / iced items they lean on.

pub(crate) use crate::finder::{Finder, FinderMode};
pub(crate) use crate::fs_scan::{DirNode, ScanResult};
pub(crate) use crate::highlight::HlLine;
pub(crate) use crate::history::{History, Loc};
pub(crate) use crate::index::SymbolEntry;
pub(crate) use crate::outline::Symbol;
pub(crate) use crate::search::SearchHit;
pub(crate) use crate::viewer::Viewer;

pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::Arc;

pub(crate) use iced::widget::operation;
pub(crate) use iced::widget::scrollable::{self, AbsoluteOffset};
pub(crate) use iced::{Element, Size, Subscription, Task, keyboard};
