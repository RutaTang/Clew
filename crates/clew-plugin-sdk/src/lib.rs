//! What a Clew plugin is written against.
//!
//! A plugin implements [`Plugin`] for a type of its own and hands it to
//! [`export!`]; every method has a default, so it writes only what it adds.
//! What it may ask of Clew is in [`host`], [`project`] and [`git`]: calling
//! one of [`project`]'s or [`git`]'s functions is what makes the plugin need
//! that permission, which the user is asked for when it is installed.
//!
//! ```ignore
//! use clew_plugin_sdk::{project, Invocation, Item, Outcome, Plugin, View};
//!
//! struct Todos;
//!
//! impl Plugin for Todos {
//!     fn run(command: &str, _context: Invocation) -> Result<Outcome, String> {
//!         match command {
//!             "list" => {
//!                 let mut items = Vec::new();
//!                 for path in project::files() {
//!                     let Ok(text) = project::read(&path) else { continue };
//!                     for (n, line) in text.lines().enumerate() {
//!                         if line.contains("TODO") {
//!                             items.push(Item::new(line.trim()).at(&path, n as u32 + 1));
//!                         }
//!                     }
//!                 }
//!                 Ok(Outcome::Show(View::new("TODOs").items(items)))
//!             }
//!             _ => Err(format!("no command {command}")),
//!         }
//!     }
//! }
//!
//! clew_plugin_sdk::export!(Todos);
//! ```

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

#[doc(hidden)]
#[allow(missing_docs, clippy::all, clippy::pedantic, unsafe_op_in_unsafe_fn)]
#[rustfmt::skip]
pub mod bindings;
#[doc(hidden)]
pub mod rt;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use bindings::clew::plugin::types::{
    BlameLine, Block, Code, Commit, Fact, Graph, GraphEdge, GraphNode, Hit, Invocation, Item,
    Level, Location, Notice, Outcome, Query, Row, Symbol, Table, TreeRow, View,
};

/// What every plugin may use: its log, the status bar's progress, and a
/// small store of its own kept between runs. None of it needs a permission.
pub mod host {
    pub use crate::bindings::clew::plugin::host::{load, log, progress, save};

    use crate::Level;

    /// Write `message` to the plugin's log as information.
    pub fn info(message: &str) {
        log(Level::Info, message);
    }

    /// Write `message` to the plugin's log as a warning.
    pub fn warn(message: &str) {
        log(Level::Warning, message);
    }

    /// Write `message` to the plugin's log as an error.
    pub fn error(message: &str) {
        log(Level::Error, message);
    }
}

/// The open project's files, outline and search. Using any of these asks
/// for the permission to read the project's files.
pub mod project {
    pub use crate::bindings::clew::plugin::project::{
        files, info, outline, read, search, ProjectInfo,
    };
}

/// The project's git history. Using any of these asks for the permission to
/// read the project's history.
pub mod git {
    pub use crate::bindings::clew::plugin::git::{blame, log};
}

/// A plugin: what it does when Clew runs one of its commands or builds one
/// of its graphs. The ids are the ones its `plugin.toml` lists.
pub trait Plugin {
    /// Run the command `command`. `context` says where it was run from.
    fn run(command: &str, context: Invocation) -> Result<Outcome, String> {
        let _ = context;
        Err(format!("this plugin has no command \"{command}\""))
    }

    /// Build the graph `graph`.
    fn graph(graph: &str) -> Result<Graph, String> {
        Err(format!("this plugin has no graph \"{graph}\""))
    }
}

/// Export `$plugin`, a type implementing [`Plugin`], as the plugin. Once,
/// at the top of `src/lib.rs`.
#[macro_export]
macro_rules! export {
    ($plugin:ty) => {
        #[doc(hidden)]
        pub struct __ClewPluginExport;

        impl $crate::bindings::exports::clew::plugin::commands::Guest for __ClewPluginExport {
            fn run(
                command: ::std::string::String,
                context: $crate::Invocation,
            ) -> ::core::result::Result<$crate::Outcome, ::std::string::String> {
                <$plugin as $crate::Plugin>::run(&command, context)
            }
        }

        impl $crate::bindings::exports::clew::plugin::graphs::Guest for __ClewPluginExport {
            fn build(
                graph: ::std::string::String,
            ) -> ::core::result::Result<$crate::Graph, ::std::string::String> {
                <$plugin as $crate::Plugin>::graph(&graph)
            }
        }

        $crate::bindings::export!(__ClewPluginExport with_types_in $crate::bindings);
    };
}

impl Location {
    /// The line `line` (from 1) of the file `path`; 0 for the file itself.
    pub fn new(path: impl Into<String>, line: u32) -> Self {
        Location {
            path: path.into(),
            line,
        }
    }
}

impl Item {
    /// A row titled `title`.
    pub fn new(title: impl Into<String>) -> Self {
        Item {
            title: title.into(),
            detail: None,
            at: None,
            tag: None,
            level: None,
        }
    }

    /// A second, quieter line.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The place it opens: line `line` (from 1) of `path`.
    pub fn at(mut self, path: impl Into<String>, line: u32) -> Self {
        self.at = Some(Location::new(path, line));
        self
    }

    /// A short label at the row's end.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// How much it matters.
    pub fn level(mut self, level: Level) -> Self {
        self.level = Some(level);
        self
    }
}

impl TreeRow {
    /// A row `depth` levels down (0 is the top), titled `title`.
    pub fn new(depth: u32, title: impl Into<String>) -> Self {
        TreeRow {
            depth,
            title: title.into(),
            detail: None,
            at: None,
            tag: None,
        }
    }

    /// A second, quieter line.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The place it opens: line `line` (from 1) of `path`.
    pub fn at(mut self, path: impl Into<String>, line: u32) -> Self {
        self.at = Some(Location::new(path, line));
        self
    }

    /// A short label at the row's end.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }
}

impl Row {
    /// A table row of `cells`, one per column.
    pub fn new<S: Into<String>>(cells: impl IntoIterator<Item = S>) -> Self {
        Row {
            cells: cells.into_iter().map(Into::into).collect(),
            at: None,
        }
    }

    /// The place it opens: line `line` (from 1) of `path`.
    pub fn at(mut self, path: impl Into<String>, line: u32) -> Self {
        self.at = Some(Location::new(path, line));
        self
    }
}

impl Table {
    /// A table with these column headings and no rows yet.
    pub fn new<S: Into<String>>(columns: impl IntoIterator<Item = S>) -> Self {
        Table {
            columns: columns.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    /// Add `row`.
    pub fn row(mut self, row: Row) -> Self {
        self.rows.push(row);
        self
    }
}

impl View {
    /// An empty page titled `title`.
    pub fn new(title: impl Into<String>) -> Self {
        View {
            title: title.into(),
            blocks: Vec::new(),
        }
    }

    /// Add `block`.
    pub fn block(mut self, block: Block) -> Self {
        self.blocks.push(block);
        self
    }

    /// Add a heading.
    pub fn heading(self, text: impl Into<String>) -> Self {
        self.block(Block::Heading(text.into()))
    }

    /// Add a paragraph of Markdown.
    pub fn text(self, markdown: impl Into<String>) -> Self {
        self.block(Block::Text(markdown.into()))
    }

    /// Add a notice: a line set apart by how much it matters.
    pub fn notice(self, level: Level, text: impl Into<String>) -> Self {
        self.block(Block::Notice(Notice {
            level,
            text: text.into(),
        }))
    }

    /// Add a list of rows.
    pub fn items(self, items: Vec<Item>) -> Self {
        self.block(Block::Items(items))
    }

    /// Add a tree.
    pub fn tree(self, rows: Vec<TreeRow>) -> Self {
        self.block(Block::Tree(rows))
    }

    /// Add a table.
    pub fn table(self, table: Table) -> Self {
        self.block(Block::Table(table))
    }

    /// Add names and their values, as "Files: 120".
    pub fn facts<N: Into<String>, V: Into<String>>(
        self,
        facts: impl IntoIterator<Item = (N, V)>,
    ) -> Self {
        self.block(Block::Facts(
            facts
                .into_iter()
                .map(|(name, value)| Fact {
                    name: name.into(),
                    value: value.into(),
                })
                .collect(),
        ))
    }

    /// Add code, coloured as `language` (`rust`, `python`, …) if given.
    pub fn code(self, language: Option<&str>, text: impl Into<String>) -> Self {
        self.block(Block::Code(Code {
            language: language.map(ToString::to_string),
            text: text.into(),
            at: None,
        }))
    }
}

impl Query {
    /// Lines that contain `text`, as written, in any case.
    pub fn text(text: impl Into<String>) -> Self {
        Query {
            query: text.into(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    /// Lines that match the regular expression `pattern`.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Query {
            regex: true,
            ..Query::text(pattern)
        }
    }

    /// Match the case as given.
    pub fn case_sensitive(mut self) -> Self {
        self.case_sensitive = true;
        self
    }

    /// Match whole words only.
    pub fn whole_word(mut self) -> Self {
        self.whole_word = true;
        self
    }

    /// Search only files matching `glob` (`src/**/*.rs`); may be given more
    /// than once.
    pub fn include(mut self, glob: impl Into<String>) -> Self {
        self.include.push(glob.into());
        self
    }

    /// Leave out files matching `glob`.
    pub fn exclude(mut self, glob: impl Into<String>) -> Self {
        self.exclude.push(glob.into());
        self
    }
}

impl Graph {
    /// A graph with nothing on it yet.
    pub fn new() -> Self {
        Graph {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Add a node and return its number, for [`Graph::edge`].
    pub fn node(&mut self, kind: impl Into<String>, label: impl Into<String>) -> u32 {
        self.nodes.push(GraphNode {
            label: label.into(),
            kind: kind.into(),
            at: None,
            detail: String::new(),
        });
        self.nodes.len() as u32 - 1
    }

    /// Add a node defined at line `line` of `path`, and return its number.
    pub fn node_at(
        &mut self,
        kind: impl Into<String>,
        label: impl Into<String>,
        path: impl Into<String>,
        line: u32,
    ) -> u32 {
        let n = self.node(kind, label);
        self.nodes[n as usize].at = Some(Location::new(path, line));
        n
    }

    /// Add an arrow from node `from` to node `to`, saying what it is.
    pub fn edge(&mut self, from: u32, to: u32, label: impl Into<String>) {
        self.edges.push(GraphEdge {
            from,
            to,
            label: label.into(),
        });
    }
}

impl Default for Graph {
    fn default() -> Self {
        Graph::new()
    }
}
