//! The FLOW tab: where a value comes from and where it goes. The language
//! server finds every occurrence of the identifier (its definition and its
//! references, project-wide); the text of each occurrence's line says what
//! that line does with it ([`flow::classify`]): declares or assigns it,
//! binds it as a parameter, passes it to a call, returns it, branches on it,
//! reaches into it, or reads it. A `Passed` node follows the value into the
//! callee: its definition is looked up, the argument's parameter read off
//! the declaration line ([`flow::parameter_at`]), and that parameter's
//! occurrences become the node's children — as deep as the reader cares to
//! go.
//!
//! Line text comes from the open panes when the file is open, else off disk
//! for a local project. A remote project's occurrences in files not open
//! here are shown by location, unclassified (the tab says so): reading
//! every file of a remote project for one trace is not worth the round
//! trips.
//!
//! Its messages, [`FlowMsg`], arrive through `App::update_flow`.

use crate::app::navigation::read_location_previews;
use crate::app::prelude::*;
use crate::graph::tree;
use crate::*;

pub(crate) use crate::flow::{Role, Use};

/// Occurrences a trace keeps at most (per level).
pub(crate) const MAX_FLOW_NODES: usize = 400;

/// One occurrence of a traced identifier.
#[derive(Debug, Clone)]
pub struct FlowNode {
    /// The identifier this node is an occurrence of (a followed parameter's
    /// name differs from the root's).
    pub symbol: String,
    pub role: Role,
    /// The callee for a `Passed`, what was assigned for an `Assigned`.
    pub detail: String,
    /// For a `Passed`: the argument's 0-based index, and the callee's name
    /// and char column on the line, for following the value in.
    pub argument: Option<usize>,
    pub callee: Option<(String, usize)>,
    pub abs: PathBuf,
    pub rel: String,
    /// 0-based line.
    pub line: usize,
    /// The occurrence's position in the server's encoding (a column once
    /// the line's text is known: `col`).
    pub character: usize,
    pub col: usize,
    /// The line's text, trimmed; empty until read.
    pub text: String,
    /// Whether `text` was read and the role classified from it.
    pub classified: bool,
    pub depth: usize,
    pub parent: Option<usize>,
    /// `None` until followed (a `Passed` node), else the children.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
}

impl tree::TreeNode for FlowNode {
    fn parent(&self) -> Option<usize> {
        self.parent
    }
    fn children(&self) -> Option<&[usize]> {
        self.children.as_deref()
    }
    fn expanded(&self) -> bool {
        self.expanded
    }
}

/// The trace: an arena of nodes, the root occurrences first.
#[derive(Debug, Clone)]
pub struct FlowTree {
    pub token: u64,
    pub symbol: String,
    pub lang: &'static str,
    /// Where the trace was asked for.
    pub origin: (PathBuf, usize),
    /// A note for the reader (occurrences left unclassified, a cap hit).
    pub note: Option<String>,
    nodes: Vec<FlowNode>,
    roots: Vec<usize>,
}

impl FlowTree {
    pub fn new(token: u64, symbol: String, lang: &'static str, origin: (PathBuf, usize)) -> Self {
        FlowTree {
            token,
            symbol,
            lang,
            origin,
            note: None,
            nodes: Vec::new(),
            roots: Vec::new(),
        }
    }

    pub fn push_root(&mut self, node: FlowNode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        self.roots.push(id);
        id
    }

    pub fn node(&self, id: usize) -> &FlowNode {
        &self.nodes[id]
    }

    pub fn get(&self, id: usize) -> Option<&FlowNode> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut FlowNode> {
        self.nodes.get_mut(id)
    }

    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The root occurrences under each role, in the roles' order, roles
    /// without any left out.
    pub fn grouped_roots(&self) -> Vec<(Role, Vec<usize>)> {
        Role::ALL
            .iter()
            .filter_map(|&role| {
                let ids: Vec<usize> = self
                    .roots
                    .iter()
                    .copied()
                    .filter(|&id| self.nodes[id].role == role)
                    .collect();
                (!ids.is_empty()).then_some((role, ids))
            })
            .collect()
    }

    /// The node's children, display-ordered under it; a `Passed` node's
    /// followed parameter first.
    pub fn set_children(&mut self, id: usize, children: Vec<FlowNode>) -> Vec<usize> {
        let Some(node) = self.nodes.get_mut(id) else {
            return Vec::new();
        };
        node.loading = false;
        if node.children.is_some() {
            node.expanded = true;
            return Vec::new();
        }
        let depth = node.depth + 1;
        let ids: Vec<usize> = children
            .into_iter()
            .take(MAX_FLOW_NODES)
            .map(|mut child| {
                child.depth = depth;
                child.parent = Some(id);
                let cid = self.nodes.len();
                self.nodes.push(child);
                cid
            })
            .collect();
        let node = &mut self.nodes[id];
        node.children = Some(ids.clone());
        node.expanded = true;
        ids
    }

    pub fn toggle(&mut self, id: usize) {
        if let Some(n) = self.nodes.get_mut(id)
            && n.children.is_some()
        {
            n.expanded = !n.expanded;
        }
    }

    /// The nodes under `root`, display-ordered (the root itself first).
    pub fn visible_under(&self, root: usize) -> Vec<usize> {
        tree::visible(&self.nodes, &[root])
    }

    /// How many occurrences are still unclassified (their line unread).
    pub fn unclassified(&self) -> usize {
        self.nodes.iter().filter(|n| !n.classified).count()
    }
}

/// What following a `Passed` node found: the callee's parameter, where it
/// is declared, and its occurrences with their lines' texts where read.
#[derive(Debug, Clone)]
pub struct Followed {
    pub symbol: String,
    pub declared: lsp::client::Target,
    pub decl_text: String,
    pub refs: Vec<lsp::client::Target>,
    /// Line texts read off disk, by index into `refs`.
    pub texts: Vec<(usize, String)>,
}

/// A node for `target`, classified when `text` (the line, untrimmed) is
/// known; `role_hint` overrides the classification (a definition is
/// `Declared` whatever its line says).
fn node_for(
    symbol: &str,
    target: &lsp::client::Target,
    rel: String,
    text: Option<&str>,
    encoding: clew_core::lsp::client::PositionEncoding,
    role_hint: Option<Role>,
) -> FlowNode {
    let mut node = FlowNode {
        symbol: symbol.to_string(),
        role: role_hint.unwrap_or(Role::Read),
        detail: String::new(),
        argument: None,
        callee: None,
        abs: target.path.clone(),
        rel,
        line: target.line,
        character: target.character,
        col: 0,
        text: String::new(),
        classified: false,
        depth: 0,
        parent: None,
        children: None,
        expanded: false,
        loading: false,
    };
    if let Some(text) = text {
        classify_node(&mut node, text, encoding, role_hint);
    }
    node
}

fn classify_node(
    node: &mut FlowNode,
    text: &str,
    encoding: clew_core::lsp::client::PositionEncoding,
    role_hint: Option<Role>,
) {
    let col = viewer::Col::from_offset(text, node.character, encoding).0;
    node.col = col;
    node.text = text.trim().to_string();
    node.classified = true;
    if let Some(role) = role_hint {
        node.role = role;
        return;
    }
    let Use {
        role,
        detail,
        argument,
        callee,
    } = crate::flow::classify(text, col, &node.symbol);
    node.role = role;
    node.detail = detail;
    node.argument = argument;
    node.callee = callee;
}

impl App {
    /// Trace the identifier at `(line, col)` of `pane`: its definition and
    /// references from the language server, then their lines.
    pub(crate) fn flow_at(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        let Some((lang, path, word, start, source_line)) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| {
                let lang = v.lang_key?;
                let word = analyze::word_at(&v.lines, line, col)?;
                let (start, _) = analyze::word_range_at(&v.lines, line, col)?;
                Some((
                    lang,
                    v.abs.clone(),
                    word,
                    start,
                    v.source_line(line).unwrap_or("").to_string(),
                ))
            })
        else {
            self.status = "Put the cursor on an identifier to trace it".into();
            return Task::none();
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready — a value trace needs one");
                return Task::none();
            }
        };
        let character = viewer::Col(start).to_offset(&source_line, client.encoding);
        self.status = format!("Tracing `{word}`…");
        self.flow_token += 1;
        let token = self.flow_token;
        self.proj.flow_pending = Some(token);
        self.proj.flow = Some(FlowTree::new(
            token,
            word.clone(),
            lang,
            (path.clone(), line),
        ));
        self.sidebar = SidebarTab::Flow;
        let stamp = self.stamp();
        let reveal = ui::reveal_sidebar_tab(SidebarTab::Flow);
        let found = Task::perform(
            async move {
                let defs = client
                    .navigate("textDocument/definition", &path, line, character)
                    .await
                    .unwrap_or_default();
                let refs = client
                    .navigate("textDocument/references", &path, line, character)
                    .await?;
                Ok((defs, refs))
            },
            move |result| {
                Message::Flow(FlowMsg::Found {
                    stamp: stamp.clone(),
                    token,
                    symbol: word.clone(),
                    result,
                })
            },
        );
        Task::batch([reveal, found])
    }

    pub(crate) fn on_flow_found(
        &mut self,
        token: u64,
        symbol: String,
        result: Result<(Vec<lsp::client::Target>, Vec<lsp::client::Target>), String>,
    ) -> Task<Message> {
        if self.proj.flow_pending != Some(token) {
            return Task::none();
        }
        self.proj.flow_pending = None;
        let Some(lang) = self.proj.flow.as_ref().map(|t| t.lang) else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        let (defs, refs) = match result {
            Ok(found) => found,
            Err(e) => {
                self.status = format!("Trace failed: {e}");
                self.proj.flow = None;
                return Task::none();
            }
        };
        let mut nodes: Vec<FlowNode> = Vec::new();
        let mut to_read: Vec<(usize, PathBuf, usize)> = Vec::new();
        let mut seen: HashSet<(PathBuf, usize, usize)> = HashSet::new();
        for (target, hint) in defs
            .iter()
            .map(|t| (t, Some(Role::Declared)))
            .chain(refs.iter().map(|t| (t, None)))
            .take(MAX_FLOW_NODES)
        {
            if !seen.insert((target.path.clone(), target.line, target.character)) {
                continue;
            }
            let rel = self.rel_of(&target.path);
            let text = self.pane_line(&target.path, target.line);
            if text.is_none() {
                to_read.push((nodes.len(), target.path.clone(), target.line));
            }
            nodes.push(node_for(
                &symbol,
                target,
                rel,
                text.as_deref(),
                encoding,
                hint,
            ));
        }
        let local = self.local_project_state();
        let Some(tree) = self.proj.flow.as_mut() else {
            return Task::none();
        };
        let n = nodes.len();
        for node in nodes {
            tree.push_root(node);
        }
        if refs.len() > MAX_FLOW_NODES {
            tree.note = Some(format!(
                "{} occurrences; the first {MAX_FLOW_NODES} are shown",
                refs.len()
            ));
        } else if !local && !to_read.is_empty() {
            tree.note = Some(format!(
                "{} occurrences in files not open here are listed by location only; open a \
                 file to classify its lines",
                to_read.len()
            ));
        }
        self.status = format!("`{symbol}`: {n} occurrences");
        if to_read.is_empty() || !local {
            return Task::none();
        }
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || read_location_previews(to_read))
                    .await
                    .unwrap_or_default()
            },
            move |lines| {
                Message::Flow(FlowMsg::Lines {
                    stamp: stamp.clone(),
                    token,
                    lines,
                })
            },
        )
    }

    /// The text of line `line` (0-based) of `path`, from an open pane.
    fn pane_line(&self, path: &Path, line: usize) -> Option<String> {
        self.proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == path)
            .and_then(|v| v.source_line(line).map(str::to_string))
    }

    pub(crate) fn on_flow_lines(
        &mut self,
        token: u64,
        lines: Vec<(usize, String)>,
    ) -> Task<Message> {
        let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        for (id, text) in lines {
            if let Some(node) = tree.node_mut(id)
                && !node.classified
            {
                let hint = (node.role == Role::Declared).then_some(Role::Declared);
                classify_node(node, &text, encoding, hint);
            }
        }
        Task::none()
    }

    /// Follow a `Passed` node into the callee's parameter.
    pub(crate) fn on_flow_expand(&mut self, token: u64, id: usize) -> Task<Message> {
        // What the node says, read and released before anything else of
        // the app is touched.
        let (lang, symbol, callee, callee_col, argument, abs, line, line_text) = {
            let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) else {
                return Task::none();
            };
            let lang = tree.lang;
            let symbol = tree.symbol.clone();
            let Some(node) = tree.node_mut(id) else {
                return Task::none();
            };
            if node.children.is_some() {
                node.expanded = !node.expanded;
                return Task::none();
            }
            let (Some((callee, callee_col)), Some(argument)) = (node.callee.clone(), node.argument)
            else {
                return Task::none();
            };
            if node.loading {
                return Task::none();
            }
            (
                lang,
                symbol,
                callee,
                callee_col,
                argument,
                node.abs.clone(),
                node.line,
                node.text.clone(),
            )
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        let local = self.local_project_state();
        // The callee's column on the line, in the server's encoding: the
        // node's text is the trimmed line, so the raw line is preferred when
        // the file is open here; else the trimmed one serves, its leading
        // whitespace already cut.
        let raw = self.pane_line(&abs, line);
        let (base, shift) = match &raw {
            Some(raw) => (
                raw.clone(),
                raw.chars().count() - raw.trim_start().chars().count(),
            ),
            None => (line_text.clone(), 0),
        };
        let character = viewer::Col(callee_col + shift).to_offset(&base, client.encoding);
        // Signature lines of the files open here, in case the callee is in one.
        let open_lines: Vec<(PathBuf, Vec<String>)> = self
            .proj
            .panes
            .iter()
            .flatten()
            .map(|v| {
                (
                    v.abs.clone(),
                    (0..v.lines.len()).map(|l| v.line_text(l + 1)).collect(),
                )
            })
            .collect();
        if let Some(node) = self.proj.flow.as_mut().and_then(|t| t.node_mut(id)) {
            node.loading = true;
        }
        let stamp = self.stamp();
        self.status = format!("Following `{symbol}` into `{callee}`…");
        Task::perform(
            async move {
                let defs = client
                    .navigate("textDocument/definition", &abs, line, character)
                    .await?;
                let declared = defs
                    .into_iter()
                    .next()
                    .ok_or_else(|| format!("no definition of `{callee}` found"))?;
                let sig = open_lines
                    .iter()
                    .find(|(p, _)| *p == declared.path)
                    .and_then(|(_, lines)| lines.get(declared.line).cloned())
                    .or_else(|| {
                        local
                            .then(|| {
                                read_location_previews(vec![(
                                    0,
                                    declared.path.clone(),
                                    declared.line,
                                )])
                                .into_iter()
                                .next()
                                .map(|(_, t)| t)
                            })
                            .flatten()
                    })
                    .ok_or_else(|| {
                        format!(
                            "open {} to follow into `{callee}`",
                            declared
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        )
                    })?;
                let (param, param_col) = crate::flow::parameter_at(&sig, argument, lang)
                    .ok_or_else(|| format!("`{callee}` has no parameter {}", argument + 1))?;
                let param_character = viewer::Col(param_col).to_offset(&sig, client.encoding);
                let refs = client
                    .navigate(
                        "textDocument/references",
                        &declared.path,
                        declared.line,
                        param_character,
                    )
                    .await?;
                let texts = if local {
                    let wanted: Vec<(usize, PathBuf, usize)> = refs
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (i, t.path.clone(), t.line))
                        .collect();
                    read_location_previews(wanted)
                } else {
                    Vec::new()
                };
                Ok(Followed {
                    symbol: param,
                    declared: lsp::client::Target {
                        path: declared.path,
                        line: declared.line,
                        character: param_character,
                    },
                    decl_text: sig,
                    refs,
                    texts,
                })
            },
            move |result| {
                Message::Flow(FlowMsg::Expanded {
                    stamp: stamp.clone(),
                    token,
                    id,
                    result,
                })
            },
        )
    }

    pub(crate) fn on_flow_expanded(
        &mut self,
        token: u64,
        id: usize,
        result: Result<Followed, String>,
    ) -> Task<Message> {
        let Some(lang) = self
            .proj
            .flow
            .as_ref()
            .filter(|t| t.token == token)
            .map(|t| t.lang)
        else {
            return Task::none();
        };
        let encoding = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.encoding,
            _ => clew_core::lsp::client::PositionEncoding::Utf16,
        };
        let followed = match result {
            Ok(f) => f,
            Err(e) => {
                if let Some(node) = self.proj.flow.as_mut().and_then(|t| t.node_mut(id)) {
                    node.loading = false;
                }
                self.status = format!("Couldn't follow the value: {e}");
                return Task::none();
            }
        };
        let mut children: Vec<FlowNode> = Vec::new();
        let rel = self.rel_of(&followed.declared.path);
        children.push(node_for(
            &followed.symbol,
            &followed.declared,
            rel,
            Some(&followed.decl_text),
            encoding,
            Some(Role::Parameter),
        ));
        let texts: HashMap<usize, String> = followed.texts.into_iter().collect();
        let mut seen: HashSet<(PathBuf, usize, usize)> = HashSet::new();
        seen.insert((
            followed.declared.path.clone(),
            followed.declared.line,
            followed.declared.character,
        ));
        for (i, target) in followed.refs.iter().enumerate() {
            if !seen.insert((target.path.clone(), target.line, target.character)) {
                continue;
            }
            let rel = self.rel_of(&target.path);
            let text = texts
                .get(&i)
                .cloned()
                .or_else(|| self.pane_line(&target.path, target.line));
            children.push(node_for(
                &followed.symbol,
                target,
                rel,
                text.as_deref(),
                encoding,
                None,
            ));
        }
        let n = children.len();
        if let Some(tree) = self.proj.flow.as_mut() {
            tree.set_children(id, children);
        }
        self.status = format!("`{}`: {n} occurrences", followed.symbol);
        Task::none()
    }

    pub(crate) fn update_flow(&mut self, message: FlowMsg) -> Task<Message> {
        match message {
            FlowMsg::FromMenu => {
                let Some(menu) = self.proj.context_menu.take() else {
                    return Task::none();
                };
                self.flow_at(menu.pane, menu.line, menu.col)
            }
            FlowMsg::AtCursor => {
                let pane = self.proj.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    return Task::none();
                };
                self.flow_at(pane, line, col)
            }
            FlowMsg::Found {
                token,
                symbol,
                result,
                ..
            } => self.on_flow_found(token, symbol, result),
            FlowMsg::Lines { token, lines, .. } => self.on_flow_lines(token, lines),
            FlowMsg::Expand { token, id } => self.on_flow_expand(token, id),
            FlowMsg::Expanded {
                token, id, result, ..
            } => self.on_flow_expanded(token, id, result),
            FlowMsg::Toggle { token, id } => {
                if let Some(tree) = self.proj.flow.as_mut().filter(|t| t.token == token) {
                    tree.toggle(id);
                }
                Task::none()
            }
            FlowMsg::Clear => {
                self.proj.flow = None;
                self.proj.flow_pending = None;
                Task::none()
            }
        }
    }
}
