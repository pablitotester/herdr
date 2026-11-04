//! AgentNav: folder tree for the lower sidebar section.
//!
//! The agents section can switch between Herdr's flat list and a folder tree that places every
//! running agent under the directory its pane is in. Everything here is client state: the server
//! and the wire protocol are untouched, because each `ClientShellPane` already reports its cwd.
//!
//! Rendering never touches the filesystem. Directory listings are read on a short-lived thread,
//! one directory per request and never recursively, and come back through a channel that the
//! client timer drains (`tick_folder_tree`). Rows are rebuilt only when the snapshot revision, a
//! listing, or the tree state changes; render walks the cached rows.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde::{Deserialize, Serialize};

use super::render::{display_width, put_text};
use super::{ClientShellConfig, ClientShellInput, ClientShellState};
use crate::api::schema::AgentStatus;
use crate::protocol::ClientShellSnapshot;

/// Directory entries shown per folder; the rest collapse into a "+N more" row.
const MAX_CHILDREN: usize = 500;
/// Reads that take longer than this are shown as unresponsive (the result is still accepted).
const SLOW_READ: Duration = Duration::from_secs(5);
/// Concurrent directory reads.
const MAX_IN_FLIGHT: usize = 8;
/// Nesting guard for pathological agent paths.
const MAX_DEPTH: usize = 48;
/// Rows above the tree body: divider, title, root path.
const HEADER_ROWS: u16 = 3;

/// Persisted in the client chrome preferences. Every field is optional so an official Herdr
/// binary that drops it, or an older file without it, simply starts with the defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct FolderTreePreferences {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) expanded: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildDir {
    name: String,
    path: PathBuf,
    link: bool,
}

/// Finished listings. Reads in progress live in `FolderTree::pending`, so a folder that is
/// being refreshed keeps showing its previous listing until the new one arrives.
#[derive(Debug)]
enum DirState {
    Loaded {
        children: Vec<ChildDir>,
        more: usize,
    },
    Failed,
}

struct DirListing {
    key: String,
    result: Result<(Vec<ChildDir>, usize), ()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RowKind {
    Folder {
        path: PathBuf,
        expanded: bool,
        expandable: bool,
        link: bool,
        badge: String,
        status: Option<AgentStatus>,
    },
    Agent {
        pane_id: String,
        status: AgentStatus,
        focused: bool,
        state: &'static str,
    },
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) depth: u16,
    pub(super) label: String,
    pub(super) kind: RowKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FolderTreeHit {
    Toggle(PathBuf),
    NewWorkspace(PathBuf),
    Folder(PathBuf),
    Agent(String),
}

#[derive(Clone, Debug, Default)]
pub(super) struct FolderTreeHits {
    /// Switches between the flat agent list and the tree (drawn in both modes).
    pub(super) mode_toggle: Rect,
    pub(super) up: Rect,
    pub(super) root_label: Rect,
    pub(super) body: Rect,
    pub(super) rows: Vec<(Rect, FolderTreeHit)>,
    pub(super) max_scroll: usize,
}

pub(super) struct FolderTree {
    pub(super) active: bool,
    /// `None` until the automatic root is resolved; `root_manual` marks a user choice.
    root: Option<PathBuf>,
    root_manual: bool,
    expanded: HashSet<String>,
    collapsed: HashSet<String>,
    dirs: HashMap<String, DirState>,
    /// Reads in progress, keyed like `dirs`, with their start time.
    pending: HashMap<String, Instant>,
    last_tick: Option<Instant>,
    tx: mpsc::Sender<DirListing>,
    rx: mpsc::Receiver<DirListing>,
    rows: Vec<Row>,
    seen_revision: Option<(String, u64)>,
    dirty: bool,
    /// Root is home or a drive root: its first level hides dotfolders and links.
    hide_root_clutter: bool,
    pub(super) scroll: usize,
}

impl Default for FolderTree {
    fn default() -> Self {
        Self::from_preferences(None)
    }
}

impl FolderTree {
    pub(super) fn from_preferences(preferences: Option<FolderTreePreferences>) -> Self {
        let preferences = preferences.unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let root = preferences
            .root
            .filter(|root| !root.trim().is_empty())
            .map(PathBuf::from);
        Self {
            active: preferences.active,
            root_manual: root.is_some(),
            root,
            expanded: preferences.expanded.into_iter().collect(),
            collapsed: preferences.collapsed.into_iter().collect(),
            dirs: HashMap::new(),
            pending: HashMap::new(),
            last_tick: None,
            tx,
            rx,
            rows: Vec::new(),
            seen_revision: None,
            dirty: true,
            hide_root_clutter: false,
            scroll: 0,
        }
    }

    pub(super) fn preferences(&self) -> Option<FolderTreePreferences> {
        let mut expanded = self.expanded.iter().cloned().collect::<Vec<_>>();
        expanded.sort();
        let mut collapsed = self.collapsed.iter().cloned().collect::<Vec<_>>();
        collapsed.sort();
        let preferences = FolderTreePreferences {
            active: self.active,
            root: self
                .root
                .as_ref()
                .filter(|_| self.root_manual)
                .map(|root| root.to_string_lossy().into_owned()),
            expanded,
            collapsed,
        };
        (preferences != FolderTreePreferences::default()).then_some(preferences)
    }

    pub(super) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(super) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Drains finished reads; returns whether anything visible changed.
    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(listing) = self.rx.try_recv() {
            self.pending.remove(&listing.key);
            let state = match listing.result {
                Ok((children, more)) => DirState::Loaded { children, more },
                Err(()) => DirState::Failed,
            };
            self.dirs.insert(listing.key, state);
            changed = true;
        }
        changed
    }

    /// True once per read, on the tick where it crosses `SLOW_READ` (to show "not responding").
    fn read_turned_slow(&mut self, now: Instant) -> bool {
        let last = self.last_tick.replace(now);
        let Some(last) = last else {
            return false;
        };
        self.pending.values().any(|started| {
            let slow_at = *started + SLOW_READ;
            slow_at > last && slow_at <= now
        })
    }

    /// Reads that still count against `MAX_IN_FLIGHT`. A read hung on a dead network path stops
    /// counting after `SLOW_READ`, so it can never starve the rest of the tree.
    fn active_reads(&self, now: Instant) -> usize {
        self.pending
            .values()
            .filter(|started| now.duration_since(**started) < SLOW_READ)
            .count()
    }

    /// Starts a read unless one is already running for that folder. Without `refresh`, folders
    /// with a finished listing are left alone.
    fn request(&mut self, path: &Path, refresh: bool) {
        let key = path_key(path);
        let now = Instant::now();
        if self.pending.contains_key(&key)
            || (!refresh && self.dirs.contains_key(&key))
            || self.active_reads(now) >= MAX_IN_FLIGHT
        {
            return;
        }
        self.pending.insert(key.clone(), now);
        let tx = self.tx.clone();
        let path = path.to_path_buf();
        let worker_key = key.clone();
        let spawned = crate::thread_spawn::spawn_named("agentnav-folder-tree", move || {
            let result = read_child_dirs(&path);
            let _ = tx.send(DirListing {
                key: worker_key,
                result,
            });
        });
        if spawned.is_err() {
            self.pending.remove(&key);
            self.dirs.insert(key, DirState::Failed);
        }
    }

    fn is_expanded(&self, key: &str, holds_agents: bool) -> bool {
        if self.collapsed.contains(key) {
            return false;
        }
        self.expanded.contains(key) || holds_agents
    }

    fn toggle(&mut self, path: &Path) {
        let key = path_key(path);
        // Toggle what the user sees: a folder may be open because it holds agents, not because
        // it is in `expanded`, so the decision comes from the current row.
        let shown_expanded = self.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::Folder { path: row_path, expanded: true, .. }
                if path_key(row_path) == key)
        });
        if shown_expanded {
            self.expanded.remove(&key);
            self.collapsed.insert(key);
        } else {
            self.collapsed.remove(&key);
            self.expanded.insert(key);
            // Opening a folder re-reads it (new clones, mkdir); the old listing stays on screen
            // until the fresh one arrives, and a failed one is retried.
            self.request(path, true);
        }
        self.dirty = true;
    }

    /// Forgets every finished listing; open folders are read again on the next rebuild.
    fn forget_listings(&mut self) {
        self.dirs.clear();
        self.dirty = true;
    }

    fn set_root(&mut self, root: Option<PathBuf>) {
        self.root_manual = root.is_some();
        self.root = root;
        self.scroll = 0;
        self.dirty = true;
    }

    /// Resolves the automatic root once, and again only when it no longer contains every pane.
    fn ensure_root(&mut self, snapshot: &ClientShellSnapshot) {
        if self.root_manual {
            return;
        }
        let pane_paths = pane_paths(snapshot);
        let keep = self.root.as_deref().is_some_and(|root| {
            pane_paths
                .iter()
                .all(|path| relative_components(root, path).is_some())
        });
        if !keep {
            let workspace_paths = snapshot
                .workspaces
                .iter()
                .map(|workspace| clean_path(&workspace.new_workspace_cwd))
                .filter(|path| !path.as_os_str().is_empty())
                .collect::<Vec<_>>();
            self.root = Some(automatic_root(&pane_paths, &workspace_paths, home_dir()));
            self.scroll = 0;
        }
    }

    /// Rebuilds the cached rows and starts the directory reads they need.
    pub(super) fn rebuild(&mut self, snapshot: Option<&ClientShellSnapshot>, now: Instant) {
        self.dirty = false;
        let Some(snapshot) = snapshot else {
            self.rows.clear();
            return;
        };
        self.ensure_root(snapshot);
        let Some(root) = self.root.clone() else {
            self.rows.clear();
            return;
        };
        let (trie, outside) = agent_trie(&root, snapshot);
        let mut rows = Vec::new();
        let mut wanted = Vec::new();
        // Home and drive roots are full of dotfolders, junctions and `$Recycle.Bin`; their first
        // level hides those unless an agent lives below them.
        self.hide_root_clutter =
            root.parent().is_none() || path_key(&root) == path_key(&home_dir());
        self.emit_children(&root, &trie, 0, now, &mut rows, &mut wanted);
        if !outside.is_empty() {
            rows.push(Row {
                depth: 0,
                label: "outside root".to_owned(),
                kind: RowKind::Note,
            });
            rows.extend(outside.into_iter().map(|agent| agent.row(0)));
        }
        if rows.is_empty() {
            rows.push(Row {
                depth: 0,
                label: "no folders".to_owned(),
                kind: RowKind::Note,
            });
        }
        self.rows = rows;
        for path in wanted {
            self.request(&path, false);
        }
    }

    fn emit_children(
        &self,
        folder: &Path,
        node: &TrieNode,
        depth: usize,
        now: Instant,
        rows: &mut Vec<Row>,
        wanted: &mut Vec<PathBuf>,
    ) {
        let depth_u16 = depth.min(u16::MAX as usize) as u16;
        let mut agents = node.agents.iter().collect::<Vec<_>>();
        agents.sort_by(|left, right| left.label.cmp(&right.label));
        rows.extend(agents.into_iter().map(|agent| agent.row(depth_u16)));

        // Children: the listing (when loaded) merged with folders that lead to agents.
        let key = path_key(folder);
        let mut children = BTreeMap::<String, (ChildDir, Option<&TrieNode>)>::new();
        let mut more = 0;
        let mut note = None;
        match self.dirs.get(&key) {
            Some(DirState::Loaded {
                children: listed,
                more: hidden,
            }) => {
                for child in listed {
                    children.insert(child.name.to_lowercase(), (child.clone(), None));
                }
                more = *hidden;
            }
            Some(DirState::Failed) if !self.pending.contains_key(&key) => {
                note = Some("cannot read");
            }
            Some(DirState::Failed) | None => match self.pending.get(&key) {
                Some(started) if now.duration_since(*started) >= SLOW_READ => {
                    note = Some("not responding…");
                }
                Some(_) => note = Some("loading…"),
                None => {
                    wanted.push(folder.to_path_buf());
                    note = Some("loading…");
                }
            },
        }
        for (lower, (name, child_node)) in &node.children {
            children
                .entry(lower.clone())
                .and_modify(|entry| entry.1 = Some(child_node))
                .or_insert_with(|| {
                    (
                        ChildDir {
                            name: name.clone(),
                            path: folder.join(name),
                            link: false,
                        },
                        Some(child_node),
                    )
                });
        }

        for (_, (child, child_node)) in children {
            if depth == 0
                && self.hide_root_clutter
                && child_node.is_none()
                && (child.link || child.name.starts_with('.') || child.name.starts_with('$'))
            {
                continue;
            }
            let child_key = path_key(&child.path);
            let agent_count = child_node.map_or(0, TrieNode::agent_count);
            let expandable = !child.link || agent_count > 0;
            let expanded =
                expandable && depth < MAX_DEPTH && self.is_expanded(&child_key, agent_count > 0);
            rows.push(Row {
                depth: depth_u16,
                label: format!("{}/", child.name),
                kind: RowKind::Folder {
                    path: child.path.clone(),
                    expanded,
                    expandable,
                    link: child.link,
                    badge: if agent_count > 0 && !expanded {
                        format!("●{agent_count}")
                    } else {
                        String::new()
                    },
                    status: child_node.and_then(TrieNode::top_status),
                },
            });
            if expanded {
                let empty = TrieNode::default();
                let child_node = child_node.unwrap_or(&empty);
                if child.link {
                    // Never list through a link or junction: only the agent paths below it.
                    self.emit_trie_only(&child.path, child_node, depth + 1, rows);
                } else {
                    self.emit_children(&child.path, child_node, depth + 1, now, rows, wanted);
                }
            }
        }
        if let Some(note) = note {
            rows.push(Row {
                depth: depth_u16,
                label: note.to_owned(),
                kind: RowKind::Note,
            });
        } else if more > 0 {
            rows.push(Row {
                depth: depth_u16,
                label: format!("+{more} more"),
                kind: RowKind::Note,
            });
        }
    }

    fn emit_trie_only(&self, folder: &Path, node: &TrieNode, depth: usize, rows: &mut Vec<Row>) {
        let depth_u16 = depth.min(u16::MAX as usize) as u16;
        rows.extend(node.agents.iter().map(|agent| agent.row(depth_u16)));
        for (name, child) in node.children.values() {
            let path = folder.join(name);
            let expanded = depth < MAX_DEPTH && self.is_expanded(&path_key(&path), true);
            let count = child.agent_count();
            rows.push(Row {
                depth: depth_u16,
                label: format!("{name}/"),
                kind: RowKind::Folder {
                    path: path.clone(),
                    expanded,
                    expandable: true,
                    link: false,
                    badge: if expanded {
                        String::new()
                    } else {
                        format!("●{count}")
                    },
                    status: child.top_status(),
                },
            });
            if expanded {
                self.emit_trie_only(&path, child, depth + 1, rows);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct AgentLeaf {
    pane_id: String,
    label: String,
    status: AgentStatus,
    focused: bool,
}

impl AgentLeaf {
    fn row(&self, depth: u16) -> Row {
        Row {
            depth,
            label: self.label.clone(),
            kind: RowKind::Agent {
                pane_id: self.pane_id.clone(),
                status: self.status,
                focused: self.focused,
                state: super::status_text(self.status),
            },
        }
    }
}

#[derive(Debug, Default)]
struct TrieNode {
    agents: Vec<AgentLeaf>,
    /// Lowercased name → (display name, node).
    children: BTreeMap<String, (String, TrieNode)>,
}

impl TrieNode {
    fn agent_count(&self) -> usize {
        self.agents.len()
            + self
                .children
                .values()
                .map(|(_, child)| child.agent_count())
                .sum::<usize>()
    }

    fn top_status(&self) -> Option<AgentStatus> {
        self.agents
            .iter()
            .map(|agent| agent.status)
            .chain(
                self.children
                    .values()
                    .filter_map(|(_, child)| child.top_status()),
            )
            .max_by_key(|status| super::status_priority(*status))
    }
}

fn agent_trie(root: &Path, snapshot: &ClientShellSnapshot) -> (TrieNode, Vec<AgentLeaf>) {
    let mut trie = TrieNode::default();
    let mut outside = Vec::new();
    for agent in &snapshot.agents {
        let leaf = AgentLeaf {
            pane_id: agent.pane_id.clone(),
            label: agent
                .display_agent
                .as_deref()
                .or(agent.name.as_deref())
                .or(agent.agent.as_deref())
                .or(agent.title.as_deref())
                .unwrap_or("agent")
                .to_owned(),
            status: agent.agent_status,
            focused: agent.focused,
        };
        let path = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == agent.pane_id)
            .and_then(|pane| pane.foreground_cwd.as_deref().or(pane.cwd.as_deref()))
            .map(clean_path);
        let Some(components) = path
            .as_deref()
            .and_then(|path| relative_components(root, path))
        else {
            // Outside the tree there is no folder row to tell same-named agents apart, so the
            // label carries the agent's folder (or its workspace when the pane has no cwd).
            let place = path
                .as_deref()
                .map(|path| {
                    path.file_name().map_or_else(
                        || path.to_string_lossy().into_owned(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                })
                .or_else(|| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == agent.workspace_id)
                        .map(|workspace| workspace.label.clone())
                })
                .filter(|place| !place.is_empty());
            let mut leaf = leaf;
            if let Some(place) = place {
                leaf.label = format!("{} · {place}", leaf.label);
            }
            outside.push(leaf);
            continue;
        };
        let mut node = &mut trie;
        for name in components.into_iter().take(MAX_DEPTH) {
            node = &mut node
                .children
                .entry(name.to_lowercase())
                .or_insert_with(|| (name.clone(), TrieNode::default()))
                .1;
        }
        node.agents.push(leaf);
    }
    (trie, outside)
}

fn pane_paths(snapshot: &ClientShellSnapshot) -> Vec<PathBuf> {
    snapshot
        .panes
        .iter()
        .filter_map(|pane| pane.foreground_cwd.as_deref().or(pane.cwd.as_deref()))
        .map(clean_path)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

/// Deepest common ancestor of the panes (or, without panes, of the workspace cwds). When that
/// folder is itself a pane's folder (one project), its parent is used so sibling projects show
/// up too. Only different drives or no data at all fall back to the home folder.
fn automatic_root(pane_paths: &[PathBuf], workspace_paths: &[PathBuf], home: PathBuf) -> PathBuf {
    let paths = if pane_paths.is_empty() {
        workspace_paths
    } else {
        pane_paths
    };
    let Some(common) = common_ancestor(paths) else {
        return home;
    };
    let common = if paths
        .iter()
        .any(|path| relative_components(&common, path).is_some_and(|rest| rest.is_empty()))
    {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    } else {
        common
    };
    // A drive root (`C:\`) is kept: projects spread over a disk are the usual case, and home
    // would leave every agent "outside root".
    common
}

fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let (first, rest) = paths.split_first()?;
    let mut common = first.components().collect::<Vec<_>>();
    for path in rest {
        let shared = common
            .iter()
            .zip(path.components())
            .take_while(|(left, right)| same_component(left, right))
            .count();
        common.truncate(shared);
    }
    let has_root = common
        .iter()
        .any(|component| matches!(component, Component::RootDir));
    (has_root && !common.is_empty()).then(|| common.iter().collect())
}

fn same_component(left: &Component<'_>, right: &Component<'_>) -> bool {
    left == right
        || left
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

/// Folder names from `root` down to `path`, or `None` when `path` is not inside `root`.
/// Matching ignores ASCII case so `c:\proyectos` and `C:\Proyectos` agree.
fn relative_components(root: &Path, path: &Path) -> Option<Vec<String>> {
    let mut root_components = root.components();
    let mut path_components = path.components();
    loop {
        match (root_components.next(), path_components.clone().next()) {
            (None, _) => break,
            (Some(left), Some(right)) if same_component(&left, &right) => {
                path_components.next();
            }
            _ => return None,
        }
    }
    Some(
        path_components
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect(),
    )
}

/// Paths reported by panes, without the verbatim prefix or trailing separators.
fn clean_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed).to_owned());
    PathBuf::from(trimmed)
}

/// Separator- and case-insensitive identity for a folder (`C:\A/b` and `c:\a\B` agree).
fn path_key(path: &Path) -> String {
    let mut key = String::new();
    for component in path.components() {
        match component {
            Component::RootDir => key.push('/'),
            Component::Prefix(prefix) => key.push_str(&prefix.as_os_str().to_string_lossy()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !key.ends_with('/') {
                    key.push('/');
                }
                key.push_str("..");
            }
            Component::Normal(name) => {
                if !key.is_empty() && !key.ends_with('/') {
                    key.push('/');
                }
                key.push_str(&name.to_string_lossy());
            }
        }
    }
    key.to_lowercase()
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// One directory, one level, bounded. Links and junctions are listed but marked, so the tree
/// never descends through them.
fn read_child_dirs(path: &Path) -> Result<(Vec<ChildDir>, usize), ()> {
    let mut children = Vec::new();
    let mut more = 0;
    for entry in std::fs::read_dir(path).map_err(|_| ())?.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let link = file_type.is_symlink();
        let is_dir = if link {
            std::fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir())
        } else {
            file_type.is_dir()
        };
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if children.len() >= MAX_CHILDREN {
            more += 1;
            continue;
        }
        children.push(ChildDir {
            path: entry.path(),
            name,
            link,
        });
    }
    children.sort_by_key(|child| child.name.to_lowercase());
    Ok((children, more))
}

// ---------------------------------------------------------------------------------------------
// Rendering (pure: reads the cached rows, never the filesystem)
// ---------------------------------------------------------------------------------------------

/// Draws the "tree" switch in the flat agent list header, left of the sort label.
pub(super) fn render_list_toggle(
    buffer: &mut Buffer,
    title_row: Rect,
    sort_rect: Rect,
    config: &ClientShellConfig,
    hits: &mut FolderTreeHits,
) {
    if !config.mouse_capture {
        return;
    }
    // Same weight as the sort label, bracketed so it reads as a button. Narrow sidebars get the
    // short form; it always ends one column before the sort label so neither covers the other.
    let min_x = title_row.x.saturating_add(8); // after " agents "
    let Some((label, width, x)) = ["[tree]", "[t]"].into_iter().find_map(|label| {
        let width = display_width(label);
        let x = sort_rect.x.checked_sub(width + 1)?;
        (x >= min_x).then_some((label, width, x))
    }) else {
        return;
    };
    hits.mode_toggle = Rect::new(x, title_row.y, width, 1);
    put_text(
        buffer,
        x,
        title_row.y,
        width,
        label,
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_tree_panel(
    buffer: &mut Buffer,
    area: Rect,
    tree: &FolderTree,
    config: &ClientShellConfig,
    hits: &mut FolderTreeHits,
) {
    let palette = &config.palette;
    if area.height == 0 {
        return;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(palette.surface_dim),
    );
    if area.height < 2 {
        return;
    }
    let title_y = area.y + 1;
    put_text(
        buffer,
        area.x,
        title_y,
        area.width,
        " folders",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    if config.mouse_capture {
        let label = "[list]";
        let width = display_width(label).min(area.width);
        hits.mode_toggle = Rect::new(area.right().saturating_sub(width), title_y, width, 1);
        put_text(
            buffer,
            hits.mode_toggle.x,
            title_y,
            width,
            label,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        );
    }
    if area.height < 3 {
        return;
    }

    let root_y = area.y + 2;
    let up_label = " ..";
    let up_width = display_width(up_label).min(area.width);
    hits.up = Rect::new(area.right().saturating_sub(up_width), root_y, up_width, 1);
    put_text(
        buffer,
        hits.up.x,
        root_y,
        up_width,
        up_label,
        Style::default().fg(palette.accent),
    );
    let root_width = area.width.saturating_sub(up_width + 1);
    hits.root_label = Rect::new(area.x, root_y, root_width, 1);
    if let Some(root) = tree.root() {
        let text = root.to_string_lossy();
        let max = root_width.saturating_sub(1) as usize;
        let shown = truncate_start(&text, max);
        put_text(
            buffer,
            area.x.saturating_add(1),
            root_y,
            root_width.saturating_sub(1),
            shown,
            Style::default().fg(palette.subtext0),
        );
        if shown.len() < text.len() && root_width > 1 {
            put_text(
                buffer,
                area.x.saturating_add(1),
                root_y,
                1,
                "…",
                Style::default().fg(palette.overlay0),
            );
        }
    }

    let body = Rect::new(
        area.x,
        area.y.saturating_add(HEADER_ROWS),
        area.width,
        area.height.saturating_sub(HEADER_ROWS + 1),
    );
    hits.body = body;
    let rows = tree.rows();
    let visible = body.height as usize;
    hits.max_scroll = rows.len().saturating_sub(visible);
    let scroll = tree.scroll.min(hits.max_scroll);
    for (offset, row) in rows.iter().skip(scroll).take(visible).enumerate() {
        let y = body.y + offset as u16;
        let rect = Rect::new(body.x, y, body.width, 1);
        render_row(buffer, rect, row, config, hits);
    }
}

fn render_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row,
    config: &ClientShellConfig,
    hits: &mut FolderTreeHits,
) {
    let palette = &config.palette;
    let indent = (1 + row.depth.saturating_mul(2)).min(rect.width);
    let x = rect.x.saturating_add(indent);
    let right = rect.right();
    match &row.kind {
        RowKind::Folder {
            path,
            expanded,
            expandable,
            link,
            badge,
            status,
        } => {
            let arrow = if *link && !*expandable {
                "↪"
            } else if *expanded {
                "▾"
            } else {
                "▸"
            };
            put_text(
                buffer,
                x,
                rect.y,
                right.saturating_sub(x).min(1),
                arrow,
                Style::default().fg(palette.overlay1),
            );
            let plus_width = u16::from(config.mouse_capture && rect.width > 6) * 2;
            let badge_width = display_width(badge);
            let name_x = x.saturating_add(2);
            let name_right = right
                .saturating_sub(plus_width)
                .saturating_sub(badge_width + u16::from(badge_width > 0));
            put_text(
                buffer,
                name_x,
                rect.y,
                name_right.saturating_sub(name_x),
                &row.label,
                Style::default().fg(if *link {
                    palette.overlay1
                } else {
                    palette.text
                }),
            );
            if badge_width > 0 {
                put_text(
                    buffer,
                    name_right.saturating_add(1),
                    rect.y,
                    badge_width,
                    badge,
                    Style::default().fg(status.map_or(palette.overlay0, |status| {
                        super::status_color(status, palette)
                    })),
                );
            }
            let name_rect = Rect::new(rect.x, rect.y, name_right.saturating_sub(rect.x), 1);
            if *expandable {
                hits.rows
                    .push((name_rect, FolderTreeHit::Toggle(path.clone())));
            } else {
                hits.rows
                    .push((name_rect, FolderTreeHit::Folder(path.clone())));
            }
            if plus_width > 0 {
                let plus = Rect::new(right.saturating_sub(plus_width), rect.y, plus_width, 1);
                put_text(
                    buffer,
                    plus.x,
                    rect.y,
                    plus.width,
                    " +",
                    Style::default().fg(palette.overlay0),
                );
                hits.rows
                    .push((plus, FolderTreeHit::NewWorkspace(path.clone())));
            }
        }
        RowKind::Agent {
            pane_id,
            status,
            focused,
            state,
        } => {
            if *focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            let icon = super::status_icon(*status, config.status_indicators);
            put_text(
                buffer,
                x,
                rect.y,
                right.saturating_sub(x).min(1),
                icon,
                Style::default().fg(super::status_color(*status, palette)),
            );
            let state_width = display_width(state);
            let label_x = x.saturating_add(2);
            let label_right = right.saturating_sub(state_width + 1);
            put_text(
                buffer,
                label_x,
                rect.y,
                label_right.saturating_sub(label_x),
                &row.label,
                Style::default()
                    .fg(if *focused {
                        palette.text
                    } else {
                        palette.subtext0
                    })
                    .add_modifier(if *focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            );
            if label_right > label_x.saturating_add(4) {
                put_text(
                    buffer,
                    label_right,
                    rect.y,
                    state_width,
                    state,
                    Style::default().fg(super::status_color(*status, palette)),
                );
            }
            hits.rows
                .push((rect, FolderTreeHit::Agent(pane_id.clone())));
        }
        RowKind::Note => {
            put_text(
                buffer,
                x.saturating_add(2),
                rect.y,
                right.saturating_sub(x.saturating_add(2)),
                &row.label,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
    }
}

/// Keeps the tail of `text` that fits in `max` columns (paths read best from the end).
fn truncate_start(text: &str, max: usize) -> &str {
    if max == 0 {
        return "";
    }
    if (display_width(text) as usize) <= max {
        return text;
    }
    let mut start = text.len();
    let mut width = 1; // room for the leading ellipsis
    for (index, character) in text.char_indices().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width + char_width > max {
            break;
        }
        width += char_width;
        start = index;
    }
    &text[start..]
}

// ---------------------------------------------------------------------------------------------
// Client shell hooks
// ---------------------------------------------------------------------------------------------

impl ClientShellState {
    /// Runs on the client timer (≤100 ms). Cheap when idle: inactive trees return at once, and
    /// an active tree only compares the snapshot revision and polls the channel.
    pub(crate) fn tick_folder_tree(&mut self) -> bool {
        if !self.folder_tree.active {
            return false;
        }
        let now = Instant::now();
        let mut changed = self.folder_tree.drain() || self.folder_tree.dirty;
        let revision_changed = match (self.snapshot.as_deref(), &self.folder_tree.seen_revision) {
            (Some(snapshot), Some((boot_id, revision))) => {
                *boot_id != snapshot.boot_id || *revision != snapshot.revision
            }
            (Some(_), None) => true,
            (None, seen) => seen.is_some(),
        };
        if revision_changed {
            self.folder_tree.seen_revision = self
                .snapshot
                .as_deref()
                .map(|snapshot| (snapshot.boot_id.clone(), snapshot.revision));
            changed = true;
        }
        if !self.folder_tree.read_turned_slow(now) && !changed {
            return false;
        }
        let previous = std::mem::take(&mut self.folder_tree.rows);
        self.folder_tree.rebuild(self.snapshot.as_deref(), now);
        previous != self.folder_tree.rows
    }

    fn refresh_folder_tree(&mut self, outcome: &mut ClientShellInput) {
        self.folder_tree
            .rebuild(self.snapshot.as_deref(), Instant::now());
        self.persist_chrome_preferences(outcome);
        outcome.repaint = true;
    }

    /// Left and right clicks on the folder tree and on its list/tree switch.
    pub(super) fn handle_folder_tree_click(
        &mut self,
        point: (u16, u16),
        right_button: bool,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let hits = &self.hits.folder_tree;
        if !right_button && super::contains(hits.mode_toggle, point) {
            self.folder_tree.active = !self.folder_tree.active;
            self.folder_tree.scroll = 0;
            self.agent_scroll = 0;
            self.refresh_folder_tree(outcome);
            return true;
        }
        if !self.folder_tree.active {
            return false;
        }
        if !right_button && super::contains(hits.up, point) {
            let parent = self
                .folder_tree
                .root()
                .and_then(Path::parent)
                .map(Path::to_path_buf);
            if let Some(parent) = parent {
                self.folder_tree.set_root(Some(parent));
                self.refresh_folder_tree(outcome);
            }
            return true;
        }
        if !right_button && super::contains(hits.root_label, point) {
            // Back to the automatic root, re-reading every folder.
            self.folder_tree.forget_listings();
            self.folder_tree.set_root(None);
            self.refresh_folder_tree(outcome);
            return true;
        }
        let Some(hit) = hits
            .rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, hit)| hit.clone())
        else {
            return false;
        };
        match (hit, right_button) {
            (FolderTreeHit::Toggle(path) | FolderTreeHit::Folder(path), true) => {
                self.folder_tree.set_root(Some(path));
                self.refresh_folder_tree(outcome);
            }
            (FolderTreeHit::Toggle(path), false) => {
                self.folder_tree.toggle(&path);
                self.refresh_folder_tree(outcome);
            }
            (FolderTreeHit::Folder(_), false) => {}
            (FolderTreeHit::NewWorkspace(path), false) => {
                self.push_endpoint_method(
                    crate::api::schema::Method::WorkspaceCreate(
                        crate::api::schema::WorkspaceCreateParams {
                            source_workspace_id: None,
                            cwd: Some(path.to_string_lossy().into_owned()),
                            focus: true,
                            label: None,
                            env: Default::default(),
                        },
                    ),
                    outcome,
                );
                outcome.repaint = true;
            }
            (FolderTreeHit::Agent(pane_id), false) => {
                self.push_endpoint_method(
                    crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                        pane_id,
                    }),
                    outcome,
                );
            }
            (FolderTreeHit::NewWorkspace(_) | FolderTreeHit::Agent(_), true) => return false,
        }
        true
    }

    /// Mouse wheel over the tree body. Returns false when the pointer is elsewhere.
    pub(super) fn scroll_folder_tree(&mut self, point: (u16, u16), down: bool) -> bool {
        if !self.folder_tree.active || !super::contains(self.hits.folder_tree.body, point) {
            return false;
        }
        let max = self.hits.folder_tree.max_scroll;
        let current = self.folder_tree.scroll.min(max);
        self.folder_tree.scroll = if down {
            current.saturating_add(1).min(max)
        } else {
            current.saturating_sub(1)
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    #[test]
    fn preferences_round_trip_and_default_to_none() {
        let tree = FolderTree::from_preferences(None);
        assert_eq!(tree.preferences(), None);
        let saved = FolderTreePreferences {
            active: true,
            root: Some("/work".to_owned()),
            expanded: vec!["/work/a".to_owned()],
            collapsed: vec![],
        };
        let tree = FolderTree::from_preferences(Some(saved.clone()));
        assert_eq!(tree.preferences(), Some(saved));
    }

    #[test]
    fn old_preferences_without_folder_tree_still_load() {
        let json = r#"{"sidebar_width":30,"collapsed_groups":["/repo"]}"#;
        let preferences: super::super::preferences::ClientChromePreferences =
            serde_json::from_str(json).unwrap();
        let tree = FolderTree::from_preferences(preferences.folder_tree);
        assert!(!tree.active);
        assert!(tree.root().is_none());
    }

    #[test]
    fn relative_components_ignore_ascii_case() {
        assert_eq!(
            relative_components(&p("/Work/Repo"), &p("/work/repo/src/app")),
            Some(vec!["src".to_owned(), "app".to_owned()])
        );
        assert_eq!(
            relative_components(&p("/work/repo"), &p("/work/repo")),
            Some(vec![])
        );
        assert_eq!(
            relative_components(&p("/work/repo"), &p("/work/other")),
            None
        );
    }

    #[test]
    fn automatic_root_uses_parent_of_a_single_project() {
        let home = p("/home/me");
        assert_eq!(
            automatic_root(&[p("/work/repo")], &[], home.clone()),
            p("/work")
        );
        assert_eq!(
            automatic_root(&[p("/work/repo/src"), p("/work/repo")], &[], home.clone()),
            p("/work")
        );
        assert_eq!(
            automatic_root(&[p("/work/a/x"), p("/work/b")], &[], home.clone()),
            p("/work")
        );
    }

    #[test]
    fn automatic_root_falls_back_to_home_only_without_a_common_ancestor() {
        let home = p("/home/me");
        assert_eq!(automatic_root(&[], &[], home.clone()), home);
        // A filesystem root shared by every pane is kept.
        assert_eq!(
            automatic_root(&[p("/a"), p("/b")], &[], home.clone()),
            p("/")
        );
        assert_eq!(automatic_root(&[p("/")], &[], home.clone()), p("/"));
        #[cfg(windows)]
        {
            assert_eq!(
                automatic_root(
                    &[
                        p(r"C:\Proyectos\agentnav"),
                        p(r"C:\Proyectos\ytmamp"),
                        p(r"C:\")
                    ],
                    &[],
                    home.clone()
                ),
                p(r"C:\")
            );
            // Different drives share no ancestor.
            assert_eq!(
                automatic_root(&[p(r"C:\work"), p(r"D:\work")], &[], home.clone()),
                home
            );
        }
    }

    #[test]
    fn clean_path_strips_verbatim_prefix() {
        assert_eq!(clean_path(r"\\?\C:\Work"), p(r"C:\Work"));
        assert_eq!(clean_path(r"\\?\UNC\server\share"), p(r"\\server\share"));
        assert_eq!(clean_path(" /work "), p("/work"));
    }

    #[test]
    fn truncate_start_keeps_the_tail() {
        assert_eq!(truncate_start("/a/b/c", 10), "/a/b/c");
        assert_eq!(truncate_start("/abc/def", 5), "/def");
        assert_eq!(truncate_start("/abc/def", 0), "");
    }

    #[test]
    fn read_child_dirs_lists_one_level_of_folders() {
        let base = std::env::temp_dir().join(format!(
            "agentnav-folder-tree-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(base.join("b/inner")).unwrap();
        std::fs::create_dir_all(base.join("A")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::write(base.join("file.txt"), "x").unwrap();
        let (children, more) = read_child_dirs(&base).unwrap();
        let names = children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["A", "b"]);
        assert_eq!(more, 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rebuild_places_agents_under_their_folders_without_reading_disk_for_them() {
        let mut tree = FolderTree::from_preferences(Some(FolderTreePreferences {
            active: true,
            root: Some("/nonexistent-agentnav-root".to_owned()),
            ..Default::default()
        }));
        let snapshot = test_snapshot(&[
            ("p1", "/nonexistent-agentnav-root/repo/src", "claude"),
            ("p2", "/nonexistent-agentnav-root/repo", "codex"),
            ("p3", "/elsewhere", "gemini"),
        ]);
        tree.rebuild(Some(&snapshot), Instant::now());
        let labels = tree
            .rows()
            .iter()
            .map(|row| (row.depth, row.label.as_str()))
            .collect::<Vec<_>>();
        assert!(labels.contains(&(0, "repo/")), "{labels:?}");
        assert!(labels.contains(&(1, "codex")), "{labels:?}");
        assert!(labels.contains(&(1, "src/")), "{labels:?}");
        assert!(labels.contains(&(2, "claude")), "{labels:?}");
        assert!(labels.contains(&(0, "outside root")), "{labels:?}");
        assert!(labels.contains(&(0, "gemini · elsewhere")), "{labels:?}");
    }

    #[test]
    fn reopening_a_loaded_folder_refreshes_it_and_keeps_the_old_listing_meanwhile() {
        let base = std::env::temp_dir().join(format!(
            "agentnav-folder-tree-refresh-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/old")).unwrap();
        let mut tree = FolderTree::from_preferences(Some(FolderTreePreferences {
            active: true,
            root: Some(base.to_string_lossy().into_owned()),
            ..Default::default()
        }));
        let snapshot = test_snapshot(&[]);
        let wait_for = |tree: &mut FolderTree, label: &str| {
            for _ in 0..200 {
                tree.drain();
                tree.rebuild(Some(&snapshot), Instant::now());
                if tree.rows().iter().any(|row| row.label == label) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("{label} never appeared: {:?}", tree.rows());
        };
        wait_for(&mut tree, "repo/");
        tree.toggle(&base.join("repo"));
        wait_for(&mut tree, "old/");

        std::fs::create_dir_all(base.join("repo/new")).unwrap();
        tree.toggle(&base.join("repo"));
        tree.rebuild(Some(&snapshot), Instant::now());
        assert!(!tree.rows().iter().any(|row| row.label == "old/"));
        tree.toggle(&base.join("repo"));
        tree.rebuild(Some(&snapshot), Instant::now());
        // While the refresh runs, the previous listing stays visible.
        assert!(tree.rows().iter().any(|row| row.label == "old/"));
        wait_for(&mut tree, "new/");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hung_reads_stop_counting_against_the_limit() {
        let mut tree = FolderTree::default();
        let long_ago = Instant::now() - SLOW_READ - Duration::from_secs(1);
        for index in 0..MAX_IN_FLIGHT {
            tree.pending.insert(format!("/hung/{index}"), long_ago);
        }
        assert_eq!(tree.active_reads(Instant::now()), 0);
        tree.pending.insert("/fresh".to_owned(), Instant::now());
        assert_eq!(tree.active_reads(Instant::now()), 1);
    }

    #[test]
    fn slow_reads_trigger_one_rebuild_not_one_per_tick() {
        let mut tree = FolderTree::default();
        let start = Instant::now();
        tree.pending.insert("/slow".to_owned(), start);
        assert!(!tree.read_turned_slow(start));
        assert!(!tree.read_turned_slow(start + Duration::from_millis(100)));
        assert!(tree.read_turned_slow(start + SLOW_READ + Duration::from_millis(50)));
        assert!(!tree.read_turned_slow(start + SLOW_READ + Duration::from_millis(150)));
    }

    #[test]
    fn drive_root_hides_dotfolders_and_links_unless_they_hold_agents() {
        let root = std::env::temp_dir()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        let mut tree = FolderTree::from_preferences(Some(FolderTreePreferences {
            active: true,
            root: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        }));
        let child = |name: &str, link: bool| ChildDir {
            name: name.to_owned(),
            path: root.join(name),
            link,
        };
        tree.dirs.insert(
            path_key(&root),
            DirState::Loaded {
                children: vec![
                    child(".cache", false),
                    child(".agentwork", false),
                    child("$Recycle.Bin", false),
                    child("Junction", true),
                    child("Proyectos", false),
                ],
                more: 0,
            },
        );
        let cwd = root.join(".agentwork").to_string_lossy().into_owned();
        let snapshot = test_snapshot(&[("p1", &cwd, "claude")]);
        tree.rebuild(Some(&snapshot), Instant::now());
        let labels = tree
            .rows()
            .iter()
            .filter(|row| row.depth == 0)
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, [".agentwork/", "Proyectos/"]);
    }

    #[test]
    fn collapsing_a_folder_with_agents_shows_a_badge() {
        let mut tree = FolderTree::from_preferences(Some(FolderTreePreferences {
            active: true,
            root: Some("/nonexistent-agentnav-root".to_owned()),
            ..Default::default()
        }));
        let snapshot = test_snapshot(&[("p1", "/nonexistent-agentnav-root/repo", "claude")]);
        tree.rebuild(Some(&snapshot), Instant::now());
        tree.toggle(&p("/nonexistent-agentnav-root/repo"));
        tree.rebuild(Some(&snapshot), Instant::now());
        let repo = tree.rows().iter().find(|row| row.label == "repo/").unwrap();
        assert!(matches!(
            &repo.kind,
            RowKind::Folder { expanded: false, badge, .. } if badge == "●1"
        ));
        assert!(!tree.rows().iter().any(|row| row.label == "claude"));
    }

    fn test_snapshot(agents: &[(&str, &str, &str)]) -> ClientShellSnapshot {
        let mut snapshot: ClientShellSnapshot = serde_json::from_value(serde_json::json!({
            "boot_id": "boot",
            "revision": 1,
            "config_diagnostic": null,
            "product_announcement": null,
            "update_available": null,
            "update_install_command": "",
            "server_keybindings_toml": null,
            "latest_release_notes_available": false,
            "integration_updates_available": false,
            "worktree_directory": "",
            "release_notes": null,
            "focused_workspace_id": null,
            "focused_tab_id": null,
            "focused_pane_id": null,
            "tab_bar_right": [],
            "tab_bar_right_separator": " ",
            "agent_view_label": null,
            "agent_order": [],
            "workspaces": [],
            "tabs": [],
            "panes": [],
            "agents": [],
            "commands": []
        }))
        .unwrap();
        for (pane_id, cwd, name) in agents {
            snapshot.panes.push(crate::protocol::ClientShellPane {
                pane_id: (*pane_id).to_owned(),
                workspace_id: "w".to_owned(),
                tab_id: "t".to_owned(),
                label: None,
                cwd: Some((*cwd).to_owned()),
                foreground_cwd: None,
                focused: false,
                right_click_passthrough: false,
            });
            snapshot.agents.push(crate::protocol::ClientShellAgent {
                pane_id: (*pane_id).to_owned(),
                workspace_id: "w".to_owned(),
                tab_id: "t".to_owned(),
                name: Some((*name).to_owned()),
                display_agent: None,
                agent: None,
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                agent_status: AgentStatus::Idle,
                state_change_seq: 0,
                state_labels: vec![],
                tokens: vec![],
                focused: false,
            });
        }
        snapshot
    }
}
