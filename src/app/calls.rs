//! The Calls sidebar tab: the language-server call hierarchy for the symbol
//! under the cursor, its lazily fetched children, direction flip and expand-all.
//!
//! Its messages, [`CallsMsg`], arrive through `App::update_calls`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// Prepare a call hierarchy at a (display line, col) in `pane`, gated on the
    /// server actually supporting it. Shared by `gc` and the context menu.
    pub(crate) fn call_hierarchy_at(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        let Some((lang, path, source_line)) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| {
                v.lang_key.map(|l| {
                    (
                        l,
                        v.abs.clone(),
                        v.source_line(line).unwrap_or("").to_string(),
                    )
                })
            })
        else {
            return Task::none();
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        if !client.call_hierarchy {
            self.status = format!("Call hierarchy isn't supported for {lang}");
            return Task::none();
        }
        // The display column, in the server's own position encoding.
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        self.status = "Building call hierarchy…".into();
        let direction = callgraph::Direction::Incoming;
        // Mint this request's identity; only the awaited prepare installs.
        self.call_token += 1;
        let token = self.call_token;
        self.proj.call_pending = Some(token);
        let stamp = self.stamp();
        Task::perform(
            async move { client.prepare_call_hierarchy(&path, line, character).await },
            move |items| {
                Message::Calls(CallsMsg::Prepared {
                    stamp: stamp.clone(),
                    token,
                    direction,
                    lang,
                    items,
                })
            },
        )
    }

    pub(crate) fn on_call_hierarchy_prepared(
        &mut self,
        token: u64,
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    ) -> Task<Message> {
        if items.is_empty() {
            self.status = "No call hierarchy for the symbol under the cursor".into();
            return Task::none();
        }
        // The new tree inherits the prepare request's token (already unique).
        self.proj.call_graph = Some(callgraph::CallTree::new(token, direction, lang, items));
        self.sidebar = SidebarTab::Calls;
        // The tree is now the feedback; clear the transient "Building…"
        // status so it doesn't linger after results appear.
        self.status.clear();
        let roots = self.proj.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(
            roots
                .into_iter()
                .map(|r| self.fetch_children(r))
                .collect::<Vec<_>>(),
        )
    }

    /// Mark a node loading, then kick its fetch — the panel shows a spinner in
    /// the gap before the children arrive.
    pub(crate) fn fetch_children(&mut self, id: usize) -> Task<Message> {
        if let Some(t) = &mut self.proj.call_graph {
            t.set_loading(id);
        }
        self.call_fetch_task(id)
    }

    /// Off-thread fetch of a call-tree node's callers/callees (direction from
    /// the tree), delivered as `CallsMsg::Children`.
    pub(crate) fn call_fetch_task(&self, id: usize) -> Task<Message> {
        let Some(tree) = &self.proj.call_graph else {
            return Task::none();
        };
        let client = match self.proj.link.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return Task::none(),
        };
        let raw = tree.raw_of(id);
        let direction = tree.direction;
        // Children carry the identity of the tree they were fetched for.
        let token = tree.token;
        let stamp = self.stamp();
        Task::perform(
            async move {
                match direction {
                    callgraph::Direction::Incoming => client.incoming_calls(raw).await,
                    callgraph::Direction::Outgoing => client.outgoing_calls(raw).await,
                }
            },
            move |items| {
                Message::Calls(CallsMsg::Children {
                    stamp: stamp.clone(),
                    token,
                    id,
                    items,
                })
            },
        )
    }

    pub(crate) fn on_call_hierarchy_children(
        &mut self,
        id: usize,
        items: Vec<lsp::client::CallItem>,
    ) -> Task<Message> {
        // Keep only project-internal callers/callees — don't descend into
        // external libraries / std.
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        let items: Vec<_> = match &root {
            Some(r) => items
                .into_iter()
                .filter(|i| i.path.starts_with(r))
                .collect(),
            None => items,
        };
        let new_ids = match &mut self.proj.call_graph {
            Some(t) => t.set_children(id, items),
            None => return Task::none(),
        };
        // The tree's node cap left callers/callees out: say so.
        if let Some(note) = self
            .proj
            .call_graph
            .as_ref()
            .and_then(|t| t.hidden_note(id))
        {
            self.status = note;
        }
        // In "expand all" mode, recurse into the new project-internal
        // children until the frontier is empty or the node cap is hit.
        let recurse = self
            .proj
            .call_graph
            .as_ref()
            .is_some_and(|t| t.full && t.node_count() < callgraph::MAX_NODES);
        if recurse {
            let to_fetch: Vec<usize> = new_ids
                .into_iter()
                .filter(|&cid| {
                    self.proj
                        .call_graph
                        .as_ref()
                        .is_some_and(|t| t.needs_fetch(cid))
                })
                .collect();
            Task::batch(
                to_fetch
                    .into_iter()
                    .map(|cid| self.fetch_children(cid))
                    .collect::<Vec<_>>(),
            )
        } else {
            Task::none()
        }
    }

    pub(crate) fn on_call_hierarchy_expand(&mut self, id: usize) -> Task<Message> {
        // Node ids are bare indices into the CURRENT tree, which is replaced
        // asynchronously (a prepared hierarchy, a direction flip). An id from
        // the tree the view was drawn from may not exist in this one, and
        // every tree operation below indexes with it — out of range was a
        // panic that closed every window.
        if self
            .proj
            .call_graph
            .as_ref()
            .is_none_or(|t| id >= t.node_count())
        {
            return Task::none();
        }
        let needs = self
            .proj
            .call_graph
            .as_ref()
            .is_some_and(|t| t.needs_fetch(id));
        if needs {
            self.fetch_children(id)
        } else {
            if let Some(t) = &mut self.proj.call_graph {
                t.toggle(id);
            }
            Task::none()
        }
    }

    pub(crate) fn on_call_hierarchy_direction(&mut self) -> Task<Message> {
        let Some(tree) = &self.proj.call_graph else {
            return Task::none();
        };
        let toggled = tree.direction.toggled();
        let lang = tree.lang;
        let was_full = tree.full;
        let root_items: Vec<_> = tree
            .roots()
            .iter()
            .map(|&r| tree.node(r).item.clone())
            .collect();
        // The flipped tree is a new identity: in-flight children fetched from
        // the old one carry its token and can no longer graft onto this one.
        self.call_token += 1;
        let mut rebuilt = callgraph::CallTree::new(self.call_token, toggled, lang, root_items);
        rebuilt.full = was_full; // keep "expand all" across a direction flip
        self.proj.call_graph = Some(rebuilt);
        let roots = self.proj.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(
            roots
                .into_iter()
                .map(|r| self.fetch_children(r))
                .collect::<Vec<_>>(),
        )
    }

    pub(crate) fn on_call_hierarchy_expand_all(&mut self) -> Task<Message> {
        let frontier = match &mut self.proj.call_graph {
            Some(t) => {
                t.full = true;
                t.unfetched_frontier()
            }
            None => return Task::none(),
        };
        Task::batch(
            frontier
                .into_iter()
                .map(|id| self.fetch_children(id))
                .collect::<Vec<_>>(),
        )
    }
}

impl App {
    /// Handle a [`CallsMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_calls(&mut self, message: CallsMsg) -> Task<Message> {
        match message {
            CallsMsg::Requested => {
                let pane = self.proj.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    return Task::none();
                };
                self.call_hierarchy_at(pane, line, col)
            }
            CallsMsg::FromMenu => {
                let Some(menu) = self.proj.context_menu.take() else {
                    return Task::none();
                };
                self.call_hierarchy_at(menu.pane, menu.line, menu.col)
            }
            CallsMsg::Prepared {
                token,
                direction,
                lang,
                items,
                ..
            } => {
                // Only the awaited prepare may install a tree.
                if self.proj.call_pending.take_if(|t| *t == token).is_none() {
                    return Task::none();
                }
                self.on_call_hierarchy_prepared(token, direction, lang, items)
            }
            CallsMsg::ExpandNode { token, id } => {
                // The view named the tree it drew; a click that raced a
                // replacement (direction flip, a new hierarchy) acts on nothing.
                if self.proj.call_graph.as_ref().map(|t| t.token) != Some(token) {
                    return Task::none();
                }
                self.on_call_hierarchy_expand(id)
            }
            CallsMsg::Children {
                token, id, items, ..
            } => {
                // Children may only attach to the exact tree they were fetched
                // for — node ids are bare indices into it.
                if self.proj.call_graph.as_ref().map(|t| t.token) != Some(token) {
                    return Task::none();
                }
                self.on_call_hierarchy_children(id, items)
            }
            CallsMsg::Direction => self.on_call_hierarchy_direction(),
            CallsMsg::ExpandAll => self.on_call_hierarchy_expand_all(),
        }
    }
}
