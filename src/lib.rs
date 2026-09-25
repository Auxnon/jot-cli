use std::{
    env, fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent};
use serde::{Deserialize, Serialize};

#[cfg(feature = "google")]
pub mod google;
#[cfg(feature = "google")]
pub mod sync;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
    #[serde(default)]
    pub children: Vec<TodoItem>,
    /// When true, this item's children are hidden in the list.
    #[serde(default)]
    pub folded: bool,
    /// Link to a Google task once this item has been synced. Always present in
    /// the data model so files stay compatible whether or not the `google`
    /// feature is compiled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncMeta>,
    /// Stable identity used to match this item across concurrent sessions.
    /// 0 = not yet assigned (legacy file); `normalize` fills it in.
    #[serde(default)]
    pub id: u64,
    /// Unix-millisecond time of the last edit to this item's own fields
    /// (title, done, folded, position). Newest copy wins on merge.
    #[serde(default)]
    pub modified: u64,
}

/// What was last synced to Google for an item, so the next sync can tell which
/// side (local or remote) changed since.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncMeta {
    /// The Google task id this item is linked to.
    pub google_id: String,
    /// Title as of the last successful sync.
    pub synced_title: String,
    /// Done state as of the last successful sync.
    pub synced_done: bool,
}

impl TodoItem {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            done: false,
            children: Vec::new(),
            folded: false,
            sync: None,
            id: new_id(),
            modified: now_ms(),
        }
    }
}

/// Current unix time in milliseconds — the resolution item merges compare at.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A random id for a newly created item or workspace.
fn new_id() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish().max(1)
}

/// A deterministic id for pre-id (legacy) data, derived from where the item
/// sits and what it says. Concurrent sessions loading the same legacy file
/// must assign identical ids, or their first merge would duplicate everything.
fn stable_id(ws_index: usize, path: &[usize], text: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    (ws_index, path, text).hash(&mut hasher);
    hasher.finish().max(1)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
    #[serde(default)]
    pub items: Vec<TodoItem>,
    /// When set, this workspace mirrors a Google task list with the given id.
    /// `@default` is Google's alias for the account's default list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google_tasklist: Option<String>,
    /// When true, completed items (and their subtrees) are hidden from the
    /// list. Toggled with `h`; persisted per workspace.
    #[serde(default)]
    pub hide_completed: bool,
    /// Stable identity for cross-session merging; 0 = not yet assigned.
    #[serde(default)]
    pub id: u64,
    /// Unix-millisecond time of the last edit to this workspace's own
    /// metadata (name, hide flag, Google link) — not its items.
    #[serde(default)]
    pub modified: u64,
}

impl Workspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            items: Vec::new(),
            google_tasklist: None,
            hide_completed: false,
            id: new_id(),
            modified: now_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Store {
    #[serde(default = "default_workspaces")]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub selected_workspace: usize,
    /// When true, the app syncs Google-linked workspaces automatically on
    /// launch and on quit. Toggled with Shift+S; persisted across runs.
    #[serde(default)]
    pub auto_sync: bool,
}

fn default_workspaces() -> Vec<Workspace> {
    vec![Workspace::new("Inbox")]
}

impl Default for Store {
    fn default() -> Self {
        Self {
            workspaces: default_workspaces(),
            selected_workspace: 0,
            auto_sync: false,
        }
    }
}

impl Store {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let mut store = serde_json::from_str::<Store>(&contents)
                    .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
                store.normalize();
                Ok(store)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let payload = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        // Write-then-rename so a concurrent session never reads a half-written
        // file (multiple sessions poll and merge this file).
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, payload)?;
        fs::rename(&tmp, path)
    }

    pub fn normalize(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces = default_workspaces();
        }

        if self.selected_workspace >= self.workspaces.len() {
            self.selected_workspace = self.workspaces.len().saturating_sub(1);
        }

        // Give pre-id (legacy) data deterministic ids so every session that
        // loads the same file assigns the same identities.
        for (ws_index, ws) in self.workspaces.iter_mut().enumerate() {
            if ws.id == 0 {
                ws.id = stable_id(ws_index, &[], &ws.name);
            }
            let mut path = Vec::new();
            assign_stable_item_ids(ws_index, &mut ws.items, &mut path);
        }
    }

    /// Fold another session's copy of the data into this one. Items and
    /// workspaces are matched by id; whichever copy was edited most recently
    /// wins. Presence conflicts (one side has it, the other doesn't) are
    /// decided against `last_synced_ms` — the last time this session read or
    /// wrote the file: newer than that means "created/edited since, keep",
    /// older means "the other side deleted it, drop". List order follows the
    /// side whose layout changed most recently (creations, moves, and deletes
    /// stamp every entry whose position shifted).
    pub fn merge_from(&mut self, disk: Store, last_synced_ms: u64) {
        let local = std::mem::take(&mut self.workspaces);
        let local_newest = local.iter().map(|ws| ws.modified).max().unwrap_or(0);
        let disk_newest = disk.workspaces.iter().map(|ws| ws.modified).max().unwrap_or(0);
        let (primary, secondary) = if disk_newest > local_newest {
            (disk.workspaces, local)
        } else {
            (local, disk.workspaces)
        };

        let secondary_order: Vec<u64> = secondary.iter().map(|ws| ws.id).collect();
        let mut secondary_slots: Vec<Option<Workspace>> = secondary.into_iter().map(Some).collect();
        let mut merged = Vec::new();
        for ws in primary {
            let matched = secondary_slots
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|other| other.id == ws.id))
                .and_then(Option::take);
            match matched {
                Some(other) => merged.push(merge_workspace(ws, other, last_synced_ms)),
                None if newest_workspace_change(&ws) >= last_synced_ms => merged.push(ws),
                None => {}
            }
        }
        for (index, slot) in secondary_slots.into_iter().enumerate() {
            let Some(ws) = slot else { continue };
            if newest_workspace_change(&ws) >= last_synced_ms {
                let at = anchored_position(&secondary_order[..index], |id| {
                    merged.iter().position(|m| m.id == id)
                });
                merged.insert(at, ws);
            }
        }

        self.workspaces = merged;
        // An item moved to different parents by both sessions shows up twice;
        // keep the newest copy and splice the other's children into its place.
        dedup_items_by_id(&mut self.workspaces);
        self.normalize();
    }

    /// Resolve a workspace by name to its index. With no `workspace` the first
    /// (top) workspace is used; a named workspace that doesn't exist is an
    /// error. `normalize` guarantees at least one workspace exists.
    pub fn workspace_index(&self, workspace: Option<&str>) -> Result<usize, String> {
        match workspace {
            Some(name) => self
                .workspaces
                .iter()
                .position(|ws| ws.name == name)
                .ok_or_else(|| format!("workspace not found: {name}")),
            None => Ok(0),
        }
    }

    /// The display name of the workspace `add_item` would target.
    pub fn workspace_name(&self, workspace: Option<&str>) -> Result<String, String> {
        Ok(self.workspaces[self.workspace_index(workspace)?].name.clone())
    }

    /// Append a top-level item to a workspace from the command line. Returns
    /// the target workspace's name. See [`Store::workspace_index`] for how the
    /// workspace is chosen.
    pub fn add_item(&mut self, title: &str, workspace: Option<&str>) -> Result<String, String> {
        let index = self.workspace_index(workspace)?;
        let ws = &mut self.workspaces[index];
        ws.items.push(TodoItem::new(title));
        Ok(ws.name.clone())
    }

    /// Ensure a workspace exists that mirrors the Google default task list.
    /// Creates a "Google" workspace bound to `@default` if none is linked yet.
    #[cfg(feature = "google")]
    pub fn ensure_google_workspace(&mut self) {
        if self
            .workspaces
            .iter()
            .any(|ws| ws.google_tasklist.is_some())
        {
            return;
        }
        let mut ws = Workspace::new("Google");
        ws.google_tasklist = Some(String::from("@default"));
        self.workspaces.push(ws);
    }
}

fn assign_stable_item_ids(ws_index: usize, items: &mut [TodoItem], path: &mut Vec<usize>) {
    for (index, item) in items.iter_mut().enumerate() {
        path.push(index);
        if item.id == 0 {
            item.id = stable_id(ws_index, path, &item.title);
        }
        assign_stable_item_ids(ws_index, &mut item.children, path);
        path.pop();
    }
}

/// The most recent edit anywhere in the item's subtree. Presence decisions
/// use this so editing a child protects its whole chain from a stale delete.
fn newest_item_change(item: &TodoItem) -> u64 {
    item.children
        .iter()
        .map(newest_item_change)
        .fold(item.modified, u64::max)
}

fn newest_workspace_change(ws: &Workspace) -> u64 {
    ws.items
        .iter()
        .map(newest_item_change)
        .fold(ws.modified, u64::max)
}

/// Insertion point for an item from the secondary list: right after the
/// nearest of its original predecessors (`priors`, in list order) that made
/// it into the merged list, or the front when none did.
fn anchored_position(priors: &[u64], position_of: impl Fn(u64) -> Option<usize>) -> usize {
    priors
        .iter()
        .rev()
        .find_map(|&id| position_of(id).map(|pos| pos + 1))
        .unwrap_or(0)
}

/// Merge two copies of the same workspace (either side may be the local
/// one — the merge is symmetric). Metadata follows the newer copy; the item
/// trees merge item by item.
fn merge_workspace(mut a: Workspace, mut b: Workspace, last_synced_ms: u64) -> Workspace {
    let a_items = std::mem::take(&mut a.items);
    let b_items = std::mem::take(&mut b.items);
    let mut merged = if b.modified > a.modified { b } else { a };
    merged.items = merge_items(a_items, b_items, last_synced_ms);
    merged
}

fn merge_items(a: Vec<TodoItem>, b: Vec<TodoItem>, last_synced_ms: u64) -> Vec<TodoItem> {
    // The side with the most recently stamped member dictates the order:
    // creating, moving, or deleting an entry stamps everything whose
    // position shifted, so the newer layout is what a user actually saw.
    let a_newest = a.iter().map(|item| item.modified).max().unwrap_or(0);
    let b_newest = b.iter().map(|item| item.modified).max().unwrap_or(0);
    let (primary, secondary) = if b_newest > a_newest { (b, a) } else { (a, b) };

    let secondary_order: Vec<u64> = secondary.iter().map(|item| item.id).collect();
    let mut secondary_slots: Vec<Option<TodoItem>> = secondary.into_iter().map(Some).collect();
    let mut merged = Vec::new();

    for mut item in primary {
        let matched = secondary_slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|other| other.id == item.id))
            .and_then(Option::take);
        match matched {
            Some(mut other) => {
                let item_children = std::mem::take(&mut item.children);
                let other_children = std::mem::take(&mut other.children);
                let mut keep = if other.modified > item.modified { other } else { item };
                keep.children = merge_items(item_children, other_children, last_synced_ms);
                merged.push(keep);
            }
            None if newest_item_change(&item) >= last_synced_ms => merged.push(item),
            None => {}
        }
    }
    // Survivors that exist only on the secondary side keep their place
    // relative to their own neighbors instead of being dumped at the end.
    for (index, slot) in secondary_slots.into_iter().enumerate() {
        let Some(item) = slot else { continue };
        if newest_item_change(&item) >= last_synced_ms {
            let at = anchored_position(&secondary_order[..index], |id| {
                merged.iter().position(|m| m.id == id)
            });
            merged.insert(at, item);
        }
    }
    merged
}

/// Drop all but the newest copy of any id that appears more than once,
/// splicing a dropped copy's children into its place so nothing is lost.
fn dedup_items_by_id(workspaces: &mut [Workspace]) {
    use std::collections::{HashMap, HashSet};

    fn collect_newest(items: &[TodoItem], newest: &mut HashMap<u64, u64>) {
        for item in items {
            let entry = newest.entry(item.id).or_insert(0);
            *entry = (*entry).max(item.modified);
            collect_newest(&item.children, newest);
        }
    }

    fn prune(items: &mut Vec<TodoItem>, newest: &HashMap<u64, u64>, kept: &mut HashSet<u64>) {
        let mut index = 0;
        while index < items.len() {
            let item = &items[index];
            let is_newest = item.modified >= newest.get(&item.id).copied().unwrap_or(0);
            if is_newest && kept.insert(item.id) {
                prune(&mut items[index].children, newest, kept);
                index += 1;
            } else {
                let removed = items.remove(index);
                for (offset, child) in removed.children.into_iter().enumerate() {
                    items.insert(index + offset, child);
                }
                // Re-examine from the same index: the spliced children land here.
            }
        }
    }

    let mut newest = HashMap::new();
    for ws in workspaces.iter() {
        collect_newest(&ws.items, &mut newest);
    }
    let mut kept = HashSet::new();
    for ws in workspaces.iter_mut() {
        prune(&mut ws.items, &newest, &mut kept);
    }
}

pub fn default_data_path() -> PathBuf {
    if let Ok(path) = env::var("JOT_CLI_DATA_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("jot-cli")
            .join("state.json");
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("jot-cli")
            .join("state.json");
    }

    PathBuf::from(".jot-cli-state.json")
}

/// Directory holding jot-cli's config files (state, Google credentials/token).
/// Mirrors the resolution order of [`default_data_path`].
pub fn config_dir() -> PathBuf {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join("jot-cli");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".config").join("jot-cli");
    }
    PathBuf::from(".")
}

pub fn default_config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Optional user configuration. Lives at [`default_config_path`] unless
/// `--config` points elsewhere; a missing file just means defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Where the notes file lives (e.g. a folder synced by iCloud/Dropbox).
    /// `~/` expands to the home directory; a relative path resolves against
    /// the config file's own directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_path: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("bad config {}: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }
}

/// Expand a leading `~/` to the home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(home) = env::var("HOME")
        && let Ok(rest) = path.strip_prefix("~")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    NewWorkspace,
    NewSibling,
    NewChild,
    RenameSelected,
    RenameWorkspace,
}

impl EditTarget {
    /// Whether this edit produces a workspace name, which is capped at
    /// [`WORKSPACE_NAME_MAX`] characters.
    fn edits_workspace_name(&self) -> bool {
        matches!(self, EditTarget::NewWorkspace | EditTarget::RenameWorkspace)
    }
}

/// Longest allowed workspace name, in characters.
pub const WORKSPACE_NAME_MAX: usize = 24;

/// Text being typed in an edit dialog plus the cursor position, counted in
/// characters (not bytes) so multi-byte input moves and deletes cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditField {
    pub text: String,
    /// Char index in `0..=char_count()`; insertion happens at this position.
    pub cursor: usize,
}

impl EditField {
    /// A field pre-filled with `text`, cursor at the end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_offset(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, ch: char) {
        let at = self.byte_offset(self.cursor);
        self.text.insert(at, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_offset(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Remove the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_offset(self.cursor - 1);
        let end = self.byte_offset(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Remove the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_offset(self.cursor);
        let end = self.byte_offset(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Move up one visual row when the text is hard-wrapped at `width`
    /// characters per line; on the first row the cursor jumps to the start.
    pub fn move_up(&mut self, width: usize) {
        if width == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_sub(width);
    }

    /// Move down one visual row under the same hard-wrap; past the last row
    /// the cursor lands at the end.
    pub fn move_down(&mut self, width: usize) {
        if width == 0 {
            return;
        }
        self.cursor = (self.cursor + width).min(self.char_count());
    }

    /// Split for rendering: text before the cursor, the character under it
    /// (`None` when the cursor sits at the end), and the text after.
    pub fn split_at_cursor(&self) -> (&str, Option<char>, &str) {
        let (before, rest) = self.text.split_at(self.byte_offset(self.cursor));
        let mut chars = rest.chars();
        match chars.next() {
            Some(ch) => (before, Some(ch), chars.as_str()),
            None => (before, None, ""),
        }
    }
}

/// Where a moved item will land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveDest {
    /// Drop relative to `anchor` in the source workspace — after it as a
    /// sibling, or appended as its child when `as_child` is set.
    Item { anchor: Vec<usize>, as_child: bool },
    /// Append to the top level of the currently selected workspace.
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Editing { target: EditTarget, input: EditField },
    ConfirmDelete,
    /// Confirming "unfold every item in the current workspace".
    ConfirmUnfoldAll,
    /// Confirming "delete every hidden (completed) item in this workspace".
    ConfirmDeleteHidden,
    /// Confirming "turn on auto-sync" (only enabling needs confirmation).
    #[cfg(feature = "google")]
    ConfirmEnableAutoSync,
    /// Relocating the item at `origin` (which lives in `src_ws`) to `dest`.
    Moving {
        src_ws: usize,
        origin: Vec<usize>,
        dest: MoveDest,
    },
    /// Reordering workspaces: the workspace at `origin` previews moving to
    /// index `target`. The actual reorder happens on Enter (cancel-safe).
    MovingWorkspace { origin: usize, target: usize },
}

#[cfg(not(feature = "google"))]
pub const CONTROLS: &str = "q quit • ←/→ focus • ↑/↓ move • a add • o child • e rename • x toggle • z fold • Z unfold-all • h hide-done • H delete-hidden • m move • ⌃c copy • ⌃v paste • ⌃z undo • d delete • w workspace • ? help";

#[cfg(feature = "google")]
pub const CONTROLS: &str = "q quit • ←/→ focus • ↑/↓ move • a add • o child • e rename • x toggle • z fold • Z unfold-all • h hide-done • H delete-hidden • m move • ⌃c copy • ⌃v paste • ⌃z undo • d delete • w workspace • s sync • S auto-sync • ? help";

/// Which panel currently receives up/down navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Workspaces,
    Tasks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatItem {
    pub path: Vec<usize>,
    pub depth: usize,
    pub title: String,
    pub done: bool,
    pub has_children: bool,
    pub folded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    None,
    Save,
    Quit,
    /// The user asked to sync now; the event loop performs the network sync.
    #[cfg(feature = "google")]
    Sync,
}

/// How many edits the in-memory undo history retains.
const UNDO_DEPTH: usize = 20;

/// A point-in-time copy of everything an undo restores. Kept in memory only —
/// never persisted to disk.
#[derive(Debug, Clone)]
struct Snapshot {
    store: Store,
    selected_path: Option<Vec<usize>>,
}

/// A transient status-line message that restores the previous text when it
/// expires (unless something else has replaced the status meanwhile).
#[derive(Debug, Clone)]
struct Flash {
    shown: String,
    restore: String,
    until: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct App {
    pub store: Store,
    pub selected_path: Option<Vec<usize>>,
    pub mode: Mode,
    pub focus: Focus,
    pub status: String,
    flash: Option<Flash>,
    /// Bounded history of pre-edit snapshots, newest last. Capped at
    /// [`UNDO_DEPTH`]; in memory only.
    undo_stack: Vec<Snapshot>,
    /// How many characters fit on one line of the editing dialog. The render
    /// side reports it (it depends on the terminal size) so Up/Down can move
    /// the cursor by exactly one visual row.
    edit_wrap_width: usize,
}

impl App {
    pub fn new(mut store: Store) -> Self {
        store.normalize();
        let mut app = Self {
            store,
            selected_path: None,
            mode: Mode::Normal,
            focus: Focus::Tasks,
            status: String::from(CONTROLS),
            flash: None,
            undo_stack: Vec::new(),
            edit_wrap_width: 40,
        };
        app.ensure_selection();
        app
    }

    pub fn current_workspace(&self) -> &Workspace {
        &self.store.workspaces[self.store.selected_workspace]
    }

    fn current_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.store.workspaces[self.store.selected_workspace]
    }

    pub fn flattened_items(&self) -> Vec<FlatItem> {
        let mut flat = Vec::new();
        let mut path = Vec::new();
        let ws = self.current_workspace();
        flatten_items(&ws.items, ws.hide_completed, 0, &mut path, &mut flat);
        flat
    }

    /// Title of the selected item, for placing on the clipboard. Updates the
    /// status line to reflect the outcome.
    pub fn copy_selected(&mut self) -> Option<String> {
        let title = self.selected_item().map(|item| item.title.clone());
        self.status = match &title {
            Some(text) => format!("Copied: {text}"),
            None => String::from("Nothing to copy"),
        };
        title
    }

    /// Handle pasted text. While editing, the content is appended to the current
    /// input; otherwise it pre-fills a new "add item" dialog awaiting the user's
    /// confirmation.
    pub fn paste(&mut self, content: String) {
        let sanitized = content.replace(['\n', '\r'], " ");
        if let Mode::Editing { input, .. } = &mut self.mode {
            input.insert_str(&sanitized);
            return;
        }
        self.focus = Focus::Tasks;
        self.mode = Mode::Editing {
            target: EditTarget::NewSibling,
            input: EditField::new(sanitized.trim()),
        };
        self.status = String::from("Pasted — Enter to add item, Esc to cancel");
    }

    /// Replace the status line text (used by the event loop after a sync).
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// Show `text` in the status line for `duration`, then restore whatever
    /// was there — unless something else overwrote the status meanwhile.
    pub fn flash_status(&mut self, text: impl Into<String>, duration: std::time::Duration) {
        let shown = text.into();
        // Chained flashes restore the original pre-flash status, not a flash.
        let restore = match self.flash.take() {
            Some(flash) if flash.shown == self.status => flash.restore,
            _ => self.status.clone(),
        };
        self.flash = Some(Flash {
            shown: shown.clone(),
            restore,
            until: std::time::Instant::now() + duration,
        });
        self.status = shown;
    }

    /// Expire a finished flash. Called regularly by the event loop's tick.
    pub fn expire_flash(&mut self) {
        if let Some(flash) = &self.flash
            && std::time::Instant::now() >= flash.until
        {
            if self.status == flash.shown {
                self.status = flash.restore.clone();
            }
            self.flash = None;
        }
    }

    /// Report the editing dialog's inner width so Up/Down move by visual row.
    pub fn set_edit_wrap_width(&mut self, width: usize) {
        self.edit_wrap_width = width.max(1);
    }

    /// Select a workspace directly (mouse click) and focus its pane.
    pub fn select_workspace(&mut self, index: usize) {
        if index >= self.store.workspaces.len() {
            return;
        }
        self.store.selected_workspace = index;
        self.ensure_selection();
        self.set_focus(Focus::Workspaces);
    }

    /// Select the visible item at `path` directly (mouse click) and focus the
    /// tasks pane. Ignores paths that aren't currently shown.
    pub fn select_task(&mut self, path: Vec<usize>) {
        if self.flattened_items().iter().any(|item| item.path == path) {
            self.selected_path = Some(path);
            self.set_focus(Focus::Tasks);
        }
    }

    /// Re-validate selection after a sync may have added or removed items.
    #[cfg(feature = "google")]
    pub fn refresh_after_sync(&mut self) {
        self.store.normalize();
        self.ensure_selection();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Update {
        // Capture the pre-edit state, then keep it only if the key actually
        // mutated something (signalled by `Update::Save`). Undo itself is
        // handled outside this path, so it never records onto the stack.
        let before = self.snapshot();
        let update = match self.mode.clone() {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Editing { target, input } => self.handle_editing_key(key, target, input),
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key),
            Mode::ConfirmUnfoldAll => self.handle_confirm_unfold_all_key(key),
            Mode::ConfirmDeleteHidden => self.handle_confirm_delete_hidden_key(key),
            #[cfg(feature = "google")]
            Mode::ConfirmEnableAutoSync => self.handle_confirm_enable_auto_sync_key(key),
            Mode::Moving {
                src_ws,
                origin,
                dest,
            } => self.handle_moving_key(key, src_ws, origin, dest),
            Mode::MovingWorkspace { origin, target } => {
                self.handle_moving_workspace_reorder_key(key, origin, target)
            }
        };
        if matches!(update, Update::Save) {
            self.stamp_changes(&before.store);
            self.record_undo(before);
        }
        update
    }

    /// Set `modified = now` on every item and workspace whose own fields or
    /// position changed relative to `before`. Centralized here so each edit
    /// path doesn't have to remember to stamp what it touched.
    fn stamp_changes(&mut self, before: &Store) {
        use std::collections::HashMap;

        type ItemPrint = (u64, Option<u64>, usize, String, bool, bool);
        fn collect(
            ws_id: u64,
            parent: Option<u64>,
            items: &[TodoItem],
            out: &mut HashMap<u64, ItemPrint>,
        ) {
            for (index, item) in items.iter().enumerate() {
                out.insert(
                    item.id,
                    (ws_id, parent, index, item.title.clone(), item.done, item.folded),
                );
                collect(ws_id, Some(item.id), &item.children, out);
            }
        }

        fn stamp(
            ws_id: u64,
            parent: Option<u64>,
            items: &mut [TodoItem],
            old: &HashMap<u64, ItemPrint>,
            now: u64,
        ) {
            for (index, item) in items.iter_mut().enumerate() {
                let print = (ws_id, parent, index, item.title.clone(), item.done, item.folded);
                if old.get(&item.id) != Some(&print) {
                    item.modified = now;
                }
                let id = item.id;
                stamp(ws_id, Some(id), &mut item.children, old, now);
            }
        }

        let now = now_ms();
        let mut old_items = HashMap::new();
        let mut old_meta = HashMap::new();
        for (index, ws) in before.workspaces.iter().enumerate() {
            old_meta.insert(ws.id, (index, ws.name.clone(), ws.hide_completed));
            collect(ws.id, None, &ws.items, &mut old_items);
        }
        for (index, ws) in self.store.workspaces.iter_mut().enumerate() {
            let meta = (index, ws.name.clone(), ws.hide_completed);
            if old_meta.get(&ws.id) != Some(&meta) {
                ws.modified = now;
            }
            stamp(ws.id, None, &mut ws.items, &old_items, now);
        }
    }

    /// Fold in a copy of the data file written by another session. Returns
    /// whether anything visible changed. See [`Store::merge_from`].
    pub fn merge_from_disk(&mut self, disk: Store, last_synced_ms: u64) -> bool {
        let before = self.store.clone();
        self.store.merge_from(disk, last_synced_ms);
        let changed = self.store != before;
        if changed {
            self.ensure_selection();
        }
        changed
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            store: self.store.clone(),
            selected_path: self.selected_path.clone(),
        }
    }

    fn record_undo(&mut self, snapshot: Snapshot) {
        self.undo_stack.push(snapshot);
        if self.undo_stack.len() > UNDO_DEPTH {
            // Drop the oldest entry so the history stays bounded.
            self.undo_stack.remove(0);
        }
    }

    /// Restore the most recent pre-edit snapshot. Returns whether anything was
    /// undone (so the caller can persist the reverted state).
    pub fn undo(&mut self) -> bool {
        match self.undo_stack.pop() {
            Some(snapshot) => {
                let before_undo = self.store.clone();
                self.store = snapshot.store;
                self.selected_path = snapshot.selected_path;
                // Stamp what the undo changed: without this the restored
                // (older) state would lose the cross-session merge against
                // the very edit it just undid.
                self.stamp_changes(&before_undo);
                self.store.normalize();
                self.ensure_selection();
                self.status = format!("Undo • {} more available", self.undo_stack.len());
                true
            }
            None => {
                self.status = String::from("Nothing to undo");
                false
            }
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Update {
        match key.code {
            KeyCode::Char('q') => Update::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_down();
                Update::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_up();
                Update::None
            }
            KeyCode::Left => {
                self.set_focus(Focus::Workspaces);
                Update::None
            }
            KeyCode::Right => {
                self.set_focus(Focus::Tasks);
                Update::None
            }
            KeyCode::Char('h') => {
                self.toggle_hide_completed();
                Update::Save
            }
            KeyCode::Char('H') => {
                let hidden = self.hidden_count();
                if self.current_workspace().hide_completed && hidden > 0 {
                    self.mode = Mode::ConfirmDeleteHidden;
                    self.status = format!(
                        "Delete all {hidden} hidden item(s) in this workspace? y/n (Enter = yes)"
                    );
                } else {
                    self.status = String::from("No hidden items to delete");
                }
                Update::None
            }
            KeyCode::Tab => {
                self.set_focus(match self.focus {
                    Focus::Workspaces => Focus::Tasks,
                    Focus::Tasks => Focus::Workspaces,
                });
                Update::None
            }
            KeyCode::Char('a') => {
                self.mode = Mode::Editing {
                    target: EditTarget::NewSibling,
                    input: EditField::default(),
                };
                self.status = String::from("New item name");
                Update::None
            }
            KeyCode::Char('o') => {
                self.mode = Mode::Editing {
                    target: EditTarget::NewChild,
                    input: EditField::default(),
                };
                self.status = String::from("New child item name");
                Update::None
            }
            KeyCode::Char('w') => {
                if self.focus == Focus::Workspaces {
                    self.mode = Mode::Editing {
                        target: EditTarget::NewWorkspace,
                        input: EditField::default(),
                    };
                    self.status = String::from("New workspace name");
                } else {
                    self.focus = Focus::Workspaces;
                    self.status = format!("Workspace: {}", self.current_workspace().name);
                }
                Update::None
            }
            KeyCode::Char('e') => {
                // Edit whatever the focused panel points at: the workspace's
                // own name on the workspaces pane, the selected item's title
                // on the tasks pane.
                if self.focus == Focus::Workspaces {
                    self.mode = Mode::Editing {
                        target: EditTarget::RenameWorkspace,
                        input: EditField::new(self.current_workspace().name.clone()),
                    };
                    self.status = String::from("Rename workspace");
                } else {
                    let current_title = self
                        .selected_item()
                        .map(|item| item.title.clone())
                        .unwrap_or_default();
                    self.mode = Mode::Editing {
                        target: EditTarget::RenameSelected,
                        input: EditField::new(current_title),
                    };
                    self.status = String::from("Rename selected item");
                }
                Update::None
            }
            KeyCode::Char('x') | KeyCode::Char(' ') => {
                if self.toggle_selected() {
                    Update::Save
                } else {
                    Update::None
                }
            }
            KeyCode::Char('z') => {
                if self.toggle_fold() {
                    Update::Save
                } else {
                    Update::None
                }
            }
            KeyCode::Char('Z') => {
                self.mode = Mode::ConfirmUnfoldAll;
                self.status =
                    String::from("Unfold all items in this workspace? y/n (Enter = yes)");
                Update::None
            }
            #[cfg(feature = "google")]
            KeyCode::Char('s') => {
                // Sync now — the event loop performs the network round-trip.
                self.status = String::from("Syncing with Google…");
                Update::Sync
            }
            #[cfg(feature = "google")]
            KeyCode::Char('S') => {
                if self.store.auto_sync {
                    // Turning auto-sync off needs no confirmation.
                    self.store.auto_sync = false;
                    self.status = String::from("Auto-sync disabled");
                    Update::Save
                } else {
                    // Enabling is confirmed first (it adds network calls on
                    // every launch and quit).
                    self.mode = Mode::ConfirmEnableAutoSync;
                    self.status = String::from(
                        "Enable auto-sync on launch/quit? y/n (Enter = yes)",
                    );
                    Update::None
                }
            }
            KeyCode::Char('m') => {
                // On the workspaces pane, `m` reorders workspaces; on the tasks
                // pane it moves the selected item.
                if self.focus == Focus::Workspaces {
                    if self.store.workspaces.len() < 2 {
                        self.status = String::from("Need at least two workspaces to reorder");
                    } else {
                        let origin = self.store.selected_workspace;
                        self.mode = Mode::MovingWorkspace {
                            origin,
                            target: origin,
                        };
                        self.status = self.reorder_workspace_status(origin);
                    }
                    return Update::None;
                }
                match self.selected_path.clone() {
                    Some(origin) => {
                        self.focus = Focus::Tasks;
                        self.mode = Mode::Moving {
                            src_ws: self.store.selected_workspace,
                            origin: origin.clone(),
                            dest: MoveDest::Item {
                                anchor: origin,
                                as_child: false,
                            },
                        };
                        self.status = self.move_status(false);
                    }
                    None => self.status = String::from("Nothing to move"),
                }
                Update::None
            }
            KeyCode::Char('?') => {
                self.status = String::from(CONTROLS);
                Update::None
            }
            KeyCode::Char('d') => {
                match self.selected_item() {
                    Some(item) => {
                        let title = item.title.clone();
                        self.mode = Mode::ConfirmDelete;
                        self.status = format!("Delete \"{title}\"? d/y = yes, n/Esc = no");
                    }
                    None => self.status = String::from("Nothing to delete"),
                }
                Update::None
            }
            _ => Update::None,
        }
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent) -> Update {
        match key.code {
            KeyCode::Char('d') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Normal;
                if self.remove_selected() {
                    Update::Save
                } else {
                    Update::None
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Delete canceled");
                Update::None
            }
            _ => Update::None,
        }
    }

    fn handle_confirm_unfold_all_key(&mut self, key: KeyEvent) -> Update {
        match key.code {
            // Enter defaults to yes.
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Normal;
                if self.unfold_all() {
                    Update::Save
                } else {
                    Update::None
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Unfold all canceled");
                Update::None
            }
            _ => Update::None,
        }
    }

    fn handle_confirm_delete_hidden_key(&mut self, key: KeyEvent) -> Update {
        match key.code {
            // Enter defaults to yes.
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Normal;
                if self.delete_hidden() {
                    Update::Save
                } else {
                    Update::None
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Delete hidden canceled");
                Update::None
            }
            _ => Update::None,
        }
    }

    #[cfg(feature = "google")]
    fn handle_confirm_enable_auto_sync_key(&mut self, key: KeyEvent) -> Update {
        match key.code {
            // Enter defaults to yes; enabling also kicks off a sync now.
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Normal;
                self.store.auto_sync = true;
                self.status = String::from("Auto-sync enabled — syncing now…");
                Update::Sync
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Auto-sync left off");
                Update::None
            }
            _ => Update::None,
        }
    }

    fn handle_moving_key(
        &mut self,
        key: KeyEvent,
        src_ws: usize,
        origin: Vec<usize>,
        dest: MoveDest,
    ) -> Update {
        match dest {
            MoveDest::Item { anchor, as_child } => {
                self.handle_moving_item_key(key, src_ws, origin, anchor, as_child)
            }
            MoveDest::Workspace => self.handle_moving_workspace_key(key, src_ws, origin),
        }
    }

    fn handle_moving_item_key(
        &mut self,
        key: KeyEvent,
        src_ws: usize,
        origin: Vec<usize>,
        anchor: Vec<usize>,
        as_child: bool,
    ) -> Update {
        let mut anchor = anchor;
        let mut as_child = as_child;
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Move canceled");
                return Update::None;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let dest_ws = self.store.selected_workspace;
                let moved = if dest_ws == src_ws {
                    self.confirm_move(&origin, &anchor, as_child)
                } else {
                    self.confirm_move_cross(src_ws, &origin, dest_ws, &anchor, as_child)
                };
                return if moved { Update::Save } else { Update::None };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                anchor = self.selected_path.clone().unwrap_or(anchor);
                as_child = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                anchor = self.selected_path.clone().unwrap_or(anchor);
                as_child = false;
            }
            KeyCode::Right | KeyCode::Char('l') => as_child = true,
            KeyCode::Left | KeyCode::Char('h') => {
                if as_child {
                    // First step out of nesting back to a sibling drop.
                    as_child = false;
                } else if anchor.len() > 1 {
                    // Climb to the parent level.
                    anchor.truncate(anchor.len() - 1);
                } else {
                    // Already at the top level: step back out to workspace
                    // choice so a different workspace can be picked.
                    return self.enter_workspace_dest(origin);
                }
            }
            KeyCode::Char('z') => {
                // Fold/unfold the anchor item to navigate large trees while
                // positioning; selection and drop target are unchanged.
                self.toggle_fold();
            }
            KeyCode::Char('w') => return self.enter_workspace_dest(origin),
            _ => {}
        }

        self.focus = Focus::Tasks;
        self.selected_path = Some(anchor.clone());
        self.status = self.move_status(as_child);
        self.mode = Mode::Moving {
            src_ws,
            origin,
            dest: MoveDest::Item { anchor, as_child },
        };
        Update::None
    }

    fn handle_moving_workspace_key(
        &mut self,
        key: KeyEvent,
        src_ws: usize,
        origin: Vec<usize>,
    ) -> Update {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Move canceled");
                return Update::None;
            }
            KeyCode::Enter => {
                let dest_ws = self.store.selected_workspace;
                self.mode = Mode::Normal;
                return if self.confirm_move_to_workspace(src_ws, &origin, dest_ws) {
                    Update::Save
                } else {
                    Update::None
                };
            }
            // Up/Down picks which workspace; Left/Right move the item in and
            // out of the highlighted workspace (they do NOT change workspace).
            KeyCode::Up | KeyCode::Char('k') => self.move_workspace(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_workspace(1),
            KeyCode::Right | KeyCode::Char('l') => {
                // Step into the highlighted workspace's tree to position
                // precisely — works for any workspace, not just the source.
                if !self.current_workspace().items.is_empty() {
                    let anchor = vec![0];
                    self.focus = Focus::Tasks;
                    self.selected_path = Some(anchor.clone());
                    self.status = self.move_status(false);
                    self.mode = Mode::Moving {
                        src_ws,
                        origin,
                        dest: MoveDest::Item {
                            anchor,
                            as_child: false,
                        },
                    };
                    return Update::None;
                }
                // Empty workspace: nothing to position against — Enter drops in.
                self.status = format!(
                    "Workspace \"{}\" is empty • Enter to drop in • ↑/↓ pick • Esc",
                    self.current_workspace().name
                );
            }
            _ => {}
        }

        self.focus = Focus::Workspaces;
        self.status = self.move_workspace_status();
        self.mode = Mode::Moving {
            src_ws,
            origin,
            dest: MoveDest::Workspace,
        };
        Update::None
    }

    fn enter_workspace_dest(&mut self, origin: Vec<usize>) -> Update {
        let src_ws = match &self.mode {
            Mode::Moving { src_ws, .. } => *src_ws,
            _ => self.store.selected_workspace,
        };
        self.focus = Focus::Workspaces;
        // Keep the currently-viewed workspace highlighted so the user can pick a
        // different one with ↑/↓ — don't snap back to the source.
        self.status = self.move_workspace_status();
        self.mode = Mode::Moving {
            src_ws,
            origin,
            dest: MoveDest::Workspace,
        };
        Update::None
    }

    fn move_status(&self, as_child: bool) -> String {
        let target = self
            .selected_item()
            .map(|item| item.title.clone())
            .unwrap_or_default();
        if as_child {
            format!("Move: nest under \"{target}\" • ← out • z fold • Enter • Esc")
        } else {
            format!("Move: after \"{target}\" • → nest • ← out • z fold • Enter • Esc")
        }
    }

    fn move_workspace_status(&self) -> String {
        let name = self.current_workspace().name.clone();
        format!("Move to workspace \"{name}\" • ↑/↓ pick • → into list • Enter • Esc")
    }

    fn reorder_workspace_status(&self, origin: usize) -> String {
        let name = self
            .store
            .workspaces
            .get(origin)
            .map(|ws| ws.name.clone())
            .unwrap_or_default();
        format!("Reorder \"{name}\" • ↑/↓ move • Enter confirm • Esc cancel")
    }

    /// Drive workspace reordering. `target` only previews where the workspace
    /// at `origin` will land; the vector isn't reordered until Enter, so Esc
    /// leaves everything untouched.
    fn handle_moving_workspace_reorder_key(
        &mut self,
        key: KeyEvent,
        origin: usize,
        mut target: usize,
    ) -> Update {
        let last = self.store.workspaces.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Move canceled");
                return Update::None;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if origin == target {
                    self.status = String::from("Workspace left in place");
                    return Update::None;
                }
                let ws = self.store.workspaces.remove(origin);
                self.store.workspaces.insert(target, ws);
                self.store.selected_workspace = target;
                self.status = format!("Moved workspace to position {}", target + 1);
                return Update::Save;
            }
            KeyCode::Up | KeyCode::Char('k') => target = target.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => target = (target + 1).min(last),
            _ => {}
        }
        self.mode = Mode::MovingWorkspace { origin, target };
        self.status = self.reorder_workspace_status(origin);
        Update::None
    }

    fn confirm_move(&mut self, origin: &[usize], target: &[usize], as_child: bool) -> bool {
        if target == origin {
            self.status = if as_child {
                String::from("Can't nest an item under itself")
            } else {
                String::from("Item left in place")
            };
            return false;
        }
        if target.starts_with(origin) {
            self.status = String::from("Can't move an item into its own subtree");
            return false;
        }

        let target_adj = adjust_after_removal(target, origin);
        match relocate(
            &mut self.current_workspace_mut().items,
            origin,
            &target_adj,
            as_child,
        ) {
            Some((new_path, title)) => {
                self.selected_path = Some(new_path);
                self.status = format!("Moved: {title}");
                true
            }
            None => {
                self.status = String::from("Move failed");
                false
            }
        }
    }

    /// Move the item from `src_ws` at `origin` into a *different* workspace
    /// `dest_ws`, placed relative to `target` (a path within `dest_ws`). No
    /// self/subtree checks are needed since the two trees are distinct.
    fn confirm_move_cross(
        &mut self,
        src_ws: usize,
        origin: &[usize],
        dest_ws: usize,
        target: &[usize],
        as_child: bool,
    ) -> bool {
        let Some(src) = self.store.workspaces.get_mut(src_ws) else {
            self.status = String::from("Move failed");
            return false;
        };
        let Some(moved) = take_at_path(&mut src.items, origin) else {
            self.status = String::from("Move failed");
            return false;
        };

        let Some(dest) = self.store.workspaces.get_mut(dest_ws) else {
            // Destination vanished; put it back where it came from.
            self.store.workspaces[src_ws].items.push(moved);
            self.status = String::from("Move failed");
            return false;
        };

        // Keep a copy so the item can be restored if the (rare) insert fails.
        let restore = moved.clone();
        match insert_relative(&mut dest.items, moved, target, as_child) {
            Some((new_path, title)) => {
                self.store.selected_workspace = dest_ws;
                self.selected_path = Some(new_path);
                self.focus = Focus::Tasks;
                self.status =
                    format!("Moved \"{title}\" to {}", self.store.workspaces[dest_ws].name);
                true
            }
            None => {
                // Insert target was invalid; restore the item to its source.
                self.store.workspaces[src_ws].items.push(restore);
                self.status = String::from("Move failed");
                false
            }
        }
    }

    fn confirm_move_to_workspace(
        &mut self,
        src_ws: usize,
        origin: &[usize],
        dest_ws: usize,
    ) -> bool {
        let Some(src) = self.store.workspaces.get_mut(src_ws) else {
            self.status = String::from("Move failed");
            return false;
        };
        let Some(moved) = take_at_path(&mut src.items, origin) else {
            self.status = String::from("Move failed");
            return false;
        };
        let title = moved.title.clone();

        let Some(dest) = self.store.workspaces.get_mut(dest_ws) else {
            // Destination vanished; put it back where it came from.
            self.store.workspaces[src_ws].items.push(moved);
            self.status = String::from("Move failed");
            return false;
        };
        dest.items.push(moved);
        let new_index = dest.items.len() - 1;

        self.store.selected_workspace = dest_ws;
        self.selected_path = Some(vec![new_index]);
        self.focus = Focus::Tasks;
        self.status = format!("Moved \"{title}\" to {}", self.store.workspaces[dest_ws].name);
        true
    }

    fn handle_editing_key(
        &mut self,
        key: KeyEvent,
        target: EditTarget,
        mut input: EditField,
    ) -> Update {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Canceled");
                return Update::None;
            }
            KeyCode::Enter => {
                let mut value = input.text.trim().to_string();
                self.mode = Mode::Normal;
                if value.is_empty() {
                    self.status = String::from("Ignored empty input");
                    return Update::None;
                }
                if target.edits_workspace_name() {
                    // Backstop for input that bypassed the typing cap (paste,
                    // pre-limit names prefilled into the rename dialog).
                    value = truncate_chars(&value, WORKSPACE_NAME_MAX)
                        .trim_end()
                        .to_string();
                }

                let changed = match target {
                    EditTarget::NewWorkspace => self.add_workspace(value),
                    EditTarget::NewSibling => self.add_sibling(value),
                    EditTarget::NewChild => self.add_child(value),
                    EditTarget::RenameSelected => self.rename_selected(value),
                    EditTarget::RenameWorkspace => self.rename_workspace(value),
                };

                return if changed { Update::Save } else { Update::None };
            }
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.move_left(),
            KeyCode::Right => input.move_right(),
            KeyCode::Up => input.move_up(self.edit_wrap_width),
            KeyCode::Down => input.move_down(self.edit_wrap_width),
            KeyCode::Home => input.move_home(),
            KeyCode::End => input.move_end(),
            KeyCode::Char(ch) => {
                let at_cap = target.edits_workspace_name()
                    && input.char_count() >= WORKSPACE_NAME_MAX;
                if at_cap {
                    self.status = format!("Workspace names max {WORKSPACE_NAME_MAX} characters");
                } else {
                    input.insert(ch);
                }
            }
            // Any other key just preserves the in-progress input.
            _ => {}
        }
        self.mode = Mode::Editing { target, input };
        Update::None
    }

    fn ensure_selection(&mut self) {
        let flat = self.flattened_items();
        if flat.is_empty() {
            self.selected_path = None;
        } else if self
            .selected_path
            .as_ref()
            .and_then(|path| flat.iter().find(|item| &item.path == path))
            .is_none()
        {
            self.selected_path = Some(flat[0].path.clone());
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.status = match focus {
            Focus::Workspaces => format!("Workspace: {}", self.current_workspace().name),
            Focus::Tasks => self
                .selected_item()
                .map(|item| item.title.clone())
                .unwrap_or_else(|| String::from("Tasks")),
        };
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Workspaces => self.move_workspace(1),
            Focus::Tasks => self.move_selection(1),
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Workspaces => self.move_workspace(-1),
            Focus::Tasks => self.move_selection(-1),
        }
    }

    fn move_workspace(&mut self, delta: isize) {
        let current = self.store.selected_workspace as isize;
        let next = (current + delta).clamp(0, self.store.workspaces.len() as isize - 1) as usize;
        self.store.selected_workspace = next;
        self.ensure_selection();
        self.status = format!("Workspace: {}", self.current_workspace().name);
    }

    fn move_selection(&mut self, delta: isize) {
        let flat = self.flattened_items();
        if flat.is_empty() {
            self.selected_path = None;
            return;
        }

        let current = self
            .selected_path
            .as_ref()
            .and_then(|path| flat.iter().position(|item| &item.path == path))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, flat.len() as isize - 1) as usize;
        self.selected_path = Some(flat[next].path.clone());
        self.status = flat[next].title.clone();
    }

    fn add_workspace(&mut self, name: String) -> bool {
        self.store.workspaces.push(Workspace::new(name.clone()));
        self.store.selected_workspace = self.store.workspaces.len() - 1;
        self.selected_path = None;
        self.focus = Focus::Workspaces;
        self.status = format!("Created workspace: {name}");
        true
    }

    fn add_sibling(&mut self, title: String) -> bool {
        let selected_path = self.selected_path.clone();
        let items = &mut self.current_workspace_mut().items;

        match selected_path {
            Some(path) if !path.is_empty() => {
                let parent_path = &path[..path.len() - 1];
                let index = path[path.len() - 1] + 1;
                if let Some(list) = list_mut(items, parent_path) {
                    list.insert(index, TodoItem::new(title.clone()));
                    let mut new_path = parent_path.to_vec();
                    new_path.push(index);
                    self.selected_path = Some(new_path);
                }
            }
            _ => {
                items.push(TodoItem::new(title.clone()));
                self.selected_path = Some(vec![items.len() - 1]);
            }
        }

        self.focus = Focus::Tasks;
        self.status = format!("Added item: {title}");
        true
    }

    fn add_child(&mut self, title: String) -> bool {
        let selected_path = self.selected_path.clone();
        let items = &mut self.current_workspace_mut().items;

        match selected_path {
            Some(path) => {
                if let Some(item) = item_mut(items, &path) {
                    item.children.push(TodoItem::new(title.clone()));
                    let mut new_path = path;
                    new_path.push(item.children.len() - 1);
                    self.selected_path = Some(new_path);
                } else {
                    items.push(TodoItem::new(title.clone()));
                    self.selected_path = Some(vec![items.len() - 1]);
                }
            }
            None => {
                items.push(TodoItem::new(title.clone()));
                self.selected_path = Some(vec![items.len() - 1]);
            }
        }

        self.focus = Focus::Tasks;
        self.status = format!("Added child item: {title}");
        true
    }

    fn rename_selected(&mut self, title: String) -> bool {
        if let Some(item) = self.selected_item_mut() {
            item.title = title.clone();
            self.status = format!("Renamed item: {title}");
            true
        } else {
            false
        }
    }

    fn rename_workspace(&mut self, name: String) -> bool {
        self.current_workspace_mut().name = name.clone();
        self.status = format!("Renamed workspace: {name}");
        true
    }

    fn toggle_fold(&mut self) -> bool {
        let outcome = match self.selected_item_mut() {
            Some(item) if item.children.is_empty() => None,
            Some(item) => {
                item.folded = !item.folded;
                Some((item.folded, item.title.clone()))
            }
            None => return false,
        };

        match outcome {
            Some((true, title)) => {
                self.status = format!("Folded: {title}");
                true
            }
            Some((false, title)) => {
                self.status = format!("Unfolded: {title}");
                true
            }
            None => {
                self.status = String::from("No nested items to fold");
                false
            }
        }
    }

    /// Unfold every item in the current workspace. Returns whether anything
    /// was folded (and thus changed).
    fn unfold_all(&mut self) -> bool {
        let changed = unfold_items(&mut self.current_workspace_mut().items);
        self.status = if changed {
            String::from("Unfolded all items")
        } else {
            String::from("Nothing was folded")
        };
        changed
    }

    /// Toggle hiding of completed items in the current workspace, then fix up
    /// the selection in case the selected item just disappeared.
    fn toggle_hide_completed(&mut self) -> bool {
        let ws = self.current_workspace_mut();
        ws.hide_completed = !ws.hide_completed;
        let hidden = ws.hide_completed;
        self.ensure_selection();
        self.status = if hidden {
            String::from("Hiding completed items")
        } else {
            String::from("Showing completed items")
        };
        true
    }

    /// Number of items currently hidden by the completed-filter (counts whole
    /// subtrees under a completed item).
    pub fn hidden_count(&self) -> usize {
        count_hidden_completed(&self.current_workspace().items)
    }

    /// Delete every completed item (and its subtree) in the current workspace.
    fn delete_hidden(&mut self) -> bool {
        let before = count_items(&self.current_workspace().items);
        remove_completed(&mut self.current_workspace_mut().items);
        let removed = before - count_items(&self.current_workspace().items);
        self.ensure_selection();
        if removed > 0 {
            self.status = format!("Deleted {removed} hidden item(s)");
            true
        } else {
            self.status = String::from("No hidden items to delete");
            false
        }
    }

    fn toggle_selected(&mut self) -> bool {
        // Remember where the selected row sits, in case completing it hides it.
        let old_meta = self.selected_path.clone().and_then(|path| {
            self.flattened_items()
                .iter()
                .enumerate()
                .find(|(_, item)| item.path == path)
                .map(|(pos, item)| (pos, item.depth))
        });

        let Some(item) = self.selected_item_mut() else {
            return false;
        };
        // Completing or reopening a parent cascades to its whole subtree.
        let new_done = !item.done;
        let had_children = !item.children.is_empty();
        set_done_recursive(item, new_done);
        let title = item.title.clone();

        let label = if new_done { "Completed" } else { "Reopened" };
        let status = if had_children {
            format!("{label} \"{title}\" and its subtree")
        } else {
            format!("{label}: {title}")
        };

        // If hiding is on, the just-completed item disappears from the list.
        // Move the selection the same way deleting a row does, rather than
        // letting it jump to the top.
        let still_visible = self
            .flattened_items()
            .iter()
            .any(|item| Some(&item.path) == self.selected_path.as_ref());
        if !still_visible {
            let flat = self.flattened_items();
            self.selected_path =
                old_meta.and_then(|(pos, depth)| select_after_vanish(&flat, pos, depth));
            self.ensure_selection();
        }
        self.status = status;
        true
    }

    fn remove_selected(&mut self) -> bool {
        let Some(path) = self.selected_path.clone() else {
            return false;
        };

        // Remember the deleted row's position and depth in the flattened list.
        // After the item (and any children) are gone, the row that slides into
        // that same position is normally the natural next selection.
        let removed_meta = self
            .flattened_items()
            .iter()
            .enumerate()
            .find(|(_, item)| item.path == path)
            .map(|(pos, item)| (pos, item.depth));
        let items = &mut self.current_workspace_mut().items;
        let removed = remove_at_path(items, &path);
        if removed {
            let flat = self.flattened_items();
            self.selected_path = removed_meta
                .and_then(|(pos, depth)| select_after_vanish(&flat, pos, depth));
            self.ensure_selection();
            self.status = String::from("Removed item");
        }
        removed
    }

    pub fn selected_item(&self) -> Option<&TodoItem> {
        item_ref(
            &self.current_workspace().items,
            self.selected_path.as_deref()?,
        )
    }

    fn selected_item_mut(&mut self) -> Option<&mut TodoItem> {
        let path = self.selected_path.clone()?;
        item_mut(&mut self.current_workspace_mut().items, &path)
    }
}

fn flatten_items(
    items: &[TodoItem],
    hide_completed: bool,
    depth: usize,
    path: &mut Vec<usize>,
    flat: &mut Vec<FlatItem>,
) {
    for (index, item) in items.iter().enumerate() {
        // When hiding completed items, skip a done item and its whole subtree.
        if hide_completed && item.done {
            continue;
        }
        path.push(index);
        flat.push(FlatItem {
            path: path.clone(),
            depth,
            title: item.title.clone(),
            done: item.done,
            has_children: !item.children.is_empty(),
            folded: item.folded,
        });
        if !item.folded {
            flatten_items(&item.children, hide_completed, depth + 1, path, flat);
        }
        path.pop();
    }
}

/// Count every node in the tree.
/// First `max` characters of `s` (not bytes — names can hold any UTF-8).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Word-wrap `text` into lines of at most `width` characters. Breaks at
/// spaces; a single word longer than `width` is hard-split. Always returns at
/// least one line so callers can render empty titles.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else if current_len == 0 && word_len <= width {
            current.push_str(word);
            current_len = word_len;
        } else {
            // The word doesn't fit next to the current line. Flush, then
            // hard-split it if it can't fit on a line of its own.
            if current_len > 0 {
                lines.push(std::mem::take(&mut current));
            }
            let mut rest: &str = word;
            while rest.chars().count() > width {
                let split = rest
                    .char_indices()
                    .nth(width)
                    .map(|(idx, _)| idx)
                    .unwrap_or(rest.len());
                lines.push(rest[..split].to_string());
                rest = &rest[split..];
            }
            current.push_str(rest);
            current_len = rest.chars().count();
        }
    }

    if current_len > 0 || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn count_items(items: &[TodoItem]) -> usize {
    items
        .iter()
        .map(|item| 1 + count_items(&item.children))
        .sum()
}

/// Count nodes that the completed-filter hides: each completed item plus its
/// whole subtree (descend only through items that stay visible).
fn count_hidden_completed(items: &[TodoItem]) -> usize {
    items
        .iter()
        .map(|item| {
            if item.done {
                1 + count_items(&item.children)
            } else {
                count_hidden_completed(&item.children)
            }
        })
        .sum()
}

/// Remove every completed item (and its subtree) from the tree.
fn remove_completed(items: &mut Vec<TodoItem>) {
    items.retain(|item| !item.done);
    for item in items {
        remove_completed(&mut item.children);
    }
}

/// Pick the selection after the row that was at `pos` (depth `depth`) in the
/// old flattened list vanished. `flat` is the new flattened list. The row that
/// slides into `pos` is the natural pick, unless it belongs to a shallower
/// level — then climb to the previous row rather than jump out to it.
fn select_after_vanish(flat: &[FlatItem], pos: usize, depth: usize) -> Option<Vec<usize>> {
    let prev = pos.checked_sub(1).and_then(|index| flat.get(index));
    match flat.get(pos) {
        Some(next) if next.depth < depth => prev.or(Some(next)),
        Some(next) => Some(next),
        None => prev,
    }
    .map(|item| item.path.clone())
}

fn list_mut<'a>(items: &'a mut Vec<TodoItem>, path: &[usize]) -> Option<&'a mut Vec<TodoItem>> {
    let mut current = items;
    for &index in path {
        current = &mut current.get_mut(index)?.children;
    }
    Some(current)
}

fn item_ref<'a>(items: &'a [TodoItem], path: &[usize]) -> Option<&'a TodoItem> {
    let (first, rest) = path.split_first()?;
    let item = items.get(*first)?;
    if rest.is_empty() {
        Some(item)
    } else {
        item_ref(&item.children, rest)
    }
}

/// Set `done` on an item and every descendant in its subtree.
fn set_done_recursive(item: &mut TodoItem, done: bool) {
    item.done = done;
    for child in &mut item.children {
        set_done_recursive(child, done);
    }
}

/// Clear `folded` on every item in the tree. Returns whether any item was
/// actually folded (and thus changed).
fn unfold_items(items: &mut [TodoItem]) -> bool {
    let mut changed = false;
    for item in items {
        if item.folded {
            item.folded = false;
            changed = true;
        }
        if unfold_items(&mut item.children) {
            changed = true;
        }
    }
    changed
}

fn item_mut<'a>(items: &'a mut [TodoItem], path: &[usize]) -> Option<&'a mut TodoItem> {
    let (first, rest) = path.split_first()?;
    let item = items.get_mut(*first)?;
    if rest.is_empty() {
        Some(item)
    } else {
        item_mut(&mut item.children, rest)
    }
}

fn remove_at_path(items: &mut Vec<TodoItem>, path: &[usize]) -> bool {
    let Some((&index, parent_path)) = path.split_last() else {
        return false;
    };

    let Some(list) = list_mut(items, parent_path) else {
        return false;
    };

    if index < list.len() {
        list.remove(index);
        true
    } else {
        false
    }
}

fn take_at_path(items: &mut Vec<TodoItem>, path: &[usize]) -> Option<TodoItem> {
    let (&index, parent_path) = path.split_last()?;
    let list = list_mut(items, parent_path)?;
    if index < list.len() {
        Some(list.remove(index))
    } else {
        None
    }
}

/// Recompute `path` after the item at `removed` is deleted from the tree.
/// Only siblings that followed `removed` in the same list shift down by one.
fn adjust_after_removal(path: &[usize], removed: &[usize]) -> Vec<usize> {
    let mut result = path.to_vec();
    let Some((&removed_index, parent)) = removed.split_last() else {
        return result;
    };
    let pos = parent.len();
    if path.len() > pos && path[..pos] == removed[..pos] && path[pos] > removed_index {
        result[pos] -= 1;
    }
    result
}

/// Remove the item at `origin`, then re-insert it relative to `target_adj`
/// (a path already adjusted for the removal). Returns the moved item's new
/// path and title.
fn relocate(
    items: &mut Vec<TodoItem>,
    origin: &[usize],
    target_adj: &[usize],
    as_child: bool,
) -> Option<(Vec<usize>, String)> {
    let moved = take_at_path(items, origin)?;
    insert_relative(items, moved, target_adj, as_child)
}

/// Insert `moved` into `items` relative to `target`: as the last child of the
/// item at `target` when `as_child`, otherwise as the next sibling after it.
/// Returns the inserted item's new path and title.
fn insert_relative(
    items: &mut Vec<TodoItem>,
    moved: TodoItem,
    target: &[usize],
    as_child: bool,
) -> Option<(Vec<usize>, String)> {
    let title = moved.title.clone();

    if as_child {
        let parent = item_mut(items, target)?;
        parent.folded = false;
        parent.children.push(moved);
        let mut new_path = target.to_vec();
        new_path.push(parent.children.len() - 1);
        Some((new_path, title))
    } else {
        let (&target_index, parent_path) = target.split_last()?;
        let insert_index = target_index + 1;
        let list = list_mut(items, parent_path)?;
        let insert_index = insert_index.min(list.len());
        list.insert(insert_index, moved);
        let mut new_path = parent_path.to_vec();
        new_path.push(insert_index);
        Some((new_path, title))
    }
}

/// Parsed command-line invocation. When `add` is set the program performs a
/// one-shot add and exits; when `prompt_add` is set it shows an inline input
/// field; otherwise it launches the full TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    /// `--data-path` as given; the final location comes from
    /// [`CliArgs::resolve_data_path`].
    pub data_path: Option<PathBuf>,
    /// `--config` as given; defaults to [`default_config_path`].
    pub config_path: Option<PathBuf>,
    /// Title of a task to add directly from the command line (`-a`/`--add`).
    pub add: Option<String>,
    /// Target workspace by name (`-w`/`--workspace`); defaults to the top one.
    pub workspace: Option<String>,
    /// `-w`/`--workspace` was given without `-a`: show an inline input field.
    pub prompt_add: bool,
    /// Suppress success output (`--silent`); errors are still reported.
    pub silent: bool,
    /// Sync Google-linked workspaces and exit (`--sync`).
    pub sync: bool,
}

impl CliArgs {
    /// The notes file to use: `--data-path` beats the `JOT_CLI_DATA_PATH`
    /// environment variable, which beats the config file's `data_path`,
    /// which beats the default location. Errors only on an unreadable or
    /// malformed config file.
    pub fn resolve_data_path(&self) -> io::Result<PathBuf> {
        if let Some(path) = &self.data_path {
            return Ok(expand_tilde(path));
        }
        if let Ok(path) = env::var("JOT_CLI_DATA_PATH") {
            return Ok(expand_tilde(Path::new(&path)));
        }

        let config_path = self
            .config_path
            .as_ref()
            .map(|path| expand_tilde(path))
            .unwrap_or_else(default_config_path);
        if let Some(data_path) = Config::load(&config_path)?.data_path {
            let data_path = expand_tilde(&data_path);
            return Ok(if data_path.is_relative() {
                config_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(data_path)
            } else {
                data_path
            });
        }

        Ok(default_data_path())
    }
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliArgs, String> {
    let mut args = args.into_iter().peekable();
    let _ = args.next();
    let mut data_path = None;
    let mut config_path = None;
    let mut add = None;
    let mut workspace = None;
    let mut prompt_add = false;
    let mut silent = false;
    let mut sync = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sync" => sync = true,
            "--data-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| String::from("expected a path after --data-path"))?;
                data_path = Some(PathBuf::from(value));
            }
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| String::from("expected a path after --config"))?;
                config_path = Some(PathBuf::from(value));
            }
            "-a" | "--add" => {
                let value = args
                    .next()
                    .ok_or_else(|| String::from("expected a task after --add"))?;
                add = Some(value);
            }
            "-w" | "--workspace" => {
                prompt_add = true;
                // The workspace name is optional: consume the next argument as
                // the name only when it isn't another flag. Bare `-w` targets
                // the top workspace.
                if let Some(next) = args.peek()
                    && !next.starts_with('-')
                {
                    workspace = args.next();
                }
            }
            "--silent" => silent = true,
            "--help" | "-h" => {
                return Err(String::from(
                    "Usage: jot-cli [--config <path>] [--data-path <path>] [-a|--add <task>] [-w|--workspace [name]] [--sync] [--silent]\n\nFiles:\n  --config <path>           use this config file instead of\n                            ~/.config/jot-cli/config.json\n  --data-path <path>        use this notes file, overriding the config\n                            (order: --data-path > JOT_CLI_DATA_PATH >\n                            config data_path > default state.json)\n  The config file is JSON; set where the notes live with e.g.\n    { \"data_path\": \"~/Library/Mobile Documents/com~apple~CloudDocs/jot/state.json\" }\n\nAdd a task without the full TUI:\n  -a, --add <task>          add a task and exit (defaults to the top workspace)\n  -w, --workspace [name]    open an inline input field to add a task, then exit\n                            (defaults to the top workspace; name is optional)\n  --sync                    sync Google-linked workspaces and exit\n                            (requires a build with --features google)\n  --silent                  print nothing on success (errors still shown)\n\nControls:\n  ←/→         focus workspaces / tasks pane\n  Tab         toggle focused pane\n  mouse click select an item or workspace\n  ↑/↓ or k/j  move within focused pane\n  a add item\n  o add child item\n  e rename item, or the workspace on the workspaces pane\n  x toggle done\n  z fold/unfold nested items\n  Z unfold all items\n  h hide/show completed items\n  H delete hidden (completed) items\n  m move item (→ nest as child), or reorder workspace on the workspaces pane\n  d delete item\n  w new workspace\n  ? show controls\n  q quit",
                ));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(CliArgs {
        data_path,
        config_path,
        add,
        workspace,
        prompt_add,
        silent,
        sync,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir()
            .join("jot-cli-tests")
            .join(format!("{name}-{unique}.json"))
    }

    #[test]
    fn store_round_trip_persists_nested_items() {
        let path = temp_path("store-roundtrip");
        let store = Store {
            workspaces: vec![Workspace {
                name: String::from("Work"),
                items: vec![TodoItem {
                    title: String::from("Parent"),
                    done: true,
                    children: vec![TodoItem::new("Child")],
                    folded: false,
                    sync: None,
                    id: 7,
                    modified: 1,
                }],
                google_tasklist: None,
                hide_completed: false,
                id: 3,
                modified: 1,
            }],
            selected_workspace: 0,
            auto_sync: false,
        };

        store.save(&path).expect("save store");
        let loaded = Store::load(&path).expect("load store");

        assert_eq!(loaded, store);
    }

    /// A bare item with explicit identity, for merge tests.
    fn item(id: u64, modified: u64, title: &str) -> TodoItem {
        TodoItem {
            title: String::from(title),
            done: false,
            children: Vec::new(),
            folded: false,
            sync: None,
            id,
            modified,
        }
    }

    fn one_ws_store(items: Vec<TodoItem>) -> Store {
        let mut ws = Workspace::new("W");
        ws.id = 99;
        ws.modified = 1;
        ws.items = items;
        Store {
            workspaces: vec![ws],
            selected_workspace: 0,
            auto_sync: false,
        }
    }

    fn titles(store: &Store) -> Vec<&str> {
        store.workspaces[0]
            .items
            .iter()
            .map(|item| item.title.as_str())
            .collect()
    }

    #[test]
    fn merge_newer_copy_of_an_item_wins() {
        let mut local = one_ws_store(vec![item(1, 100, "old title")]);
        let disk = one_ws_store(vec![item(1, 200, "new title")]);
        local.merge_from(disk, 50);
        assert_eq!(titles(&local), ["new title"]);

        let mut local = one_ws_store(vec![item(1, 300, "mine is newer")]);
        let disk = one_ws_store(vec![item(1, 200, "stale")]);
        local.merge_from(disk, 50);
        assert_eq!(titles(&local), ["mine is newer"]);
    }

    #[test]
    fn merge_keeps_additions_from_both_sides() {
        // Synced at t=1000; each side added something afterwards.
        let mut local = one_ws_store(vec![item(1, 500, "shared"), item(2, 1500, "local new")]);
        let disk = one_ws_store(vec![item(1, 500, "shared"), item(3, 1600, "disk new")]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["shared", "local new", "disk new"]);
    }

    #[test]
    fn merge_keeps_an_item_created_in_the_middle_in_the_middle() {
        // The other session inserted "m" between x and y (stamping m, y, z
        // whose positions shifted). It must not land at the bottom here.
        let mut local = one_ws_store(vec![
            item(1, 100, "x"),
            item(2, 100, "y"),
            item(3, 100, "z"),
        ]);
        let disk = one_ws_store(vec![
            item(1, 100, "x"),
            item(9, 2000, "m"),
            item(2, 2000, "y"),
            item(3, 2000, "z"),
        ]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["x", "m", "y", "z"]);
    }

    #[test]
    fn merge_adopts_a_reorder_from_the_newer_side() {
        let mut local = one_ws_store(vec![
            item(1, 100, "a"),
            item(2, 100, "b"),
            item(3, 100, "c"),
        ]);
        let disk = one_ws_store(vec![
            item(3, 2000, "c"),
            item(1, 2000, "a"),
            item(2, 2000, "b"),
        ]);
        local.merge_from(disk, 50);
        assert_eq!(titles(&local), ["c", "a", "b"]);
    }

    #[test]
    fn merge_anchors_additions_from_both_sides() {
        // Both sessions inserted mid-list since the last sync (t=1000);
        // each new item stays next to the neighbor it was created under.
        let mut local = one_ws_store(vec![
            item(1, 100, "x"),
            item(2, 100, "y"),
            item(8, 2000, "q"), // local: q inserted after y
            item(3, 100, "z"),
        ]);
        let disk = one_ws_store(vec![
            item(1, 100, "x"),
            item(9, 1500, "p"), // other session: p inserted after x
            item(2, 100, "y"),
            item(3, 100, "z"),
        ]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["x", "p", "y", "q", "z"]);
    }

    #[test]
    fn merge_adopts_workspace_reorder_from_the_newer_side() {
        let mut ws_a = Workspace::new("A");
        ws_a.id = 1;
        ws_a.modified = 100;
        let mut ws_b = Workspace::new("B");
        ws_b.id = 2;
        ws_b.modified = 100;

        let mut local = Store {
            workspaces: vec![ws_a.clone(), ws_b.clone()],
            selected_workspace: 0,
            auto_sync: false,
        };
        // The other session moved B above A (stamping both positions).
        ws_a.modified = 2000;
        ws_b.modified = 2000;
        let disk = Store {
            workspaces: vec![ws_b, ws_a],
            selected_workspace: 0,
            auto_sync: false,
        };

        local.merge_from(disk, 50);
        let names: Vec<&str> = local.workspaces.iter().map(|ws| ws.name.as_str()).collect();
        assert_eq!(names, ["B", "A"]);
    }

    #[test]
    fn merge_applies_deletions_from_the_other_session() {
        // Disk lacks item 2, which we haven't touched since the last sync —
        // the other session deleted it.
        let mut local = one_ws_store(vec![item(1, 500, "kept"), item(2, 500, "deleted there")]);
        let disk = one_ws_store(vec![item(1, 500, "kept")]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["kept"]);
    }

    #[test]
    fn merge_edit_beats_delete() {
        // Disk lacks item 2, but we edited it after the last sync: keep it.
        let mut local = one_ws_store(vec![item(1, 500, "kept"), item(2, 1500, "edited here")]);
        let disk = one_ws_store(vec![item(1, 500, "kept")]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["kept", "edited here"]);
    }

    #[test]
    fn merge_editing_a_child_protects_its_parent_chain() {
        // The other session deleted parent 2; we edited its child since.
        let mut parent = item(2, 500, "parent");
        parent.children.push(item(3, 1500, "edited child"));
        let mut local = one_ws_store(vec![item(1, 500, "kept"), parent]);
        let disk = one_ws_store(vec![item(1, 500, "kept")]);
        local.merge_from(disk, 1000);
        assert_eq!(titles(&local), ["kept", "parent"]);
        assert_eq!(local.workspaces[0].items[1].children.len(), 1);
    }

    #[test]
    fn merge_workspace_metadata_follows_newer_copy() {
        let mut local = one_ws_store(vec![]);
        let mut disk = one_ws_store(vec![]);
        disk.workspaces[0].name = String::from("Renamed");
        disk.workspaces[0].modified = 900;
        local.merge_from(disk, 50);
        assert_eq!(local.workspaces[0].name, "Renamed");
    }

    #[test]
    fn edits_stamp_item_timestamps() {
        let mut app = App::new(Store::default());
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Enter);

        app.store.workspaces[0].items[0].modified = 5;
        app.store.workspaces[0].items[1].modified = 5;

        // Toggling the selected (second) item stamps it and only it.
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.store.workspaces[0].items[0].modified, 5);
        assert!(app.store.workspaces[0].items[1].modified > 5);
    }

    #[test]
    fn undo_stamps_the_restored_state() {
        let mut app = App::new(Store::default());
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Enter);

        app.store.workspaces[0].items[0].modified = 5;
        press(&mut app, KeyCode::Char(' ')); // toggle done (stamps to now)
        assert!(app.undo());

        let item = &app.store.workspaces[0].items[0];
        assert!(!item.done);
        // Without stamping, the restored copy would carry modified = 5 and
        // lose the merge against the very state it undid.
        assert!(item.modified > 5);
    }

    #[test]
    fn legacy_files_get_identical_ids_in_every_session() {
        let path = temp_path("legacy-ids");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"workspaces":[{"name":"W","items":[
                {"title":"a","done":false,"children":[{"title":"b","done":false}]},
                {"title":"c","done":false}
            ]}]}"#,
        )
        .unwrap();

        let first = Store::load(&path).expect("load once");
        let second = Store::load(&path).expect("load twice");

        assert_eq!(first.workspaces[0].id, second.workspaces[0].id);
        let ids = |store: &Store| -> Vec<u64> {
            let items = &store.workspaces[0].items;
            vec![items[0].id, items[0].children[0].id, items[1].id]
        };
        assert_eq!(ids(&first), ids(&second));
        assert!(ids(&first).iter().all(|&id| id != 0));
    }

    #[test]
    fn flash_status_restores_after_expiry() {
        let mut app = App::new(Store::default());
        app.set_status("base");
        app.flash_status("⟳ synced", std::time::Duration::ZERO);
        assert_eq!(app.status, "⟳ synced");
        app.expire_flash();
        assert_eq!(app.status, "base");
    }

    #[test]
    fn flash_never_clobbers_a_newer_status() {
        let mut app = App::new(Store::default());
        app.set_status("base");
        app.flash_status("⟳ synced", std::time::Duration::ZERO);
        app.set_status("something newer");
        app.expire_flash();
        assert_eq!(app.status, "something newer");
    }

    #[test]
    fn config_data_path_points_elsewhere() {
        let config_path = temp_path("config");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, r#"{"data_path":"notes/here.json"}"#).unwrap();

        let args = CliArgs {
            data_path: None,
            config_path: Some(config_path.clone()),
            add: None,
            workspace: None,
            prompt_add: false,
            silent: false,
            sync: false,
        };
        // JOT_CLI_DATA_PATH may leak in from the environment; only assert the
        // config is honored when the env override isn't set.
        if env::var("JOT_CLI_DATA_PATH").is_err() {
            let resolved = args.resolve_data_path().expect("resolve");
            // Relative config paths resolve against the config file's folder.
            assert_eq!(resolved, config_path.parent().unwrap().join("notes/here.json"));
        }

        // An explicit --data-path always wins.
        let with_flag = CliArgs {
            data_path: Some(PathBuf::from("/explicit/state.json")),
            ..args
        };
        assert_eq!(
            with_flag.resolve_data_path().expect("resolve"),
            PathBuf::from("/explicit/state.json")
        );
    }

    #[test]
    fn app_adds_and_flattens_child_items() {
        let mut app = App::new(Store::default());

        assert!(app.add_sibling(String::from("Parent")));
        assert!(app.add_child(String::from("Child")));

        let flat = app.flattened_items();
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].title, "Parent");
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[1].title, "Child");
        assert_eq!(flat[1].depth, 1);
    }

    #[test]
    fn completing_parent_cascades_to_subtree() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));
        app.selected_path = Some(vec![0, 0]);
        app.add_child(String::from("Grandchild")); // under Child
        app.selected_path = Some(vec![0]); // back to Parent

        // Completing the parent marks the whole subtree done.
        assert!(app.toggle_selected());
        let parent = &app.store.workspaces[0].items[0];
        assert!(parent.done);
        assert!(parent.children[0].done); // Child
        assert!(parent.children[0].children[0].done); // Grandchild

        // Reopening the parent clears the whole subtree again.
        assert!(app.toggle_selected());
        let parent = &app.store.workspaces[0].items[0];
        assert!(!parent.done);
        assert!(!parent.children[0].done);
        assert!(!parent.children[0].children[0].done);
    }

    #[test]
    fn undo_restores_state_before_last_edit() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));
        app.selected_path = Some(vec![0]);

        // Cascade-complete the subtree via the key path (so it records undo).
        press(&mut app, KeyCode::Char('x'));
        assert!(app.store.workspaces[0].items[0].done);
        assert!(app.store.workspaces[0].items[0].children[0].done);

        // Undo reverts the whole cascade in one step.
        assert!(app.undo());
        assert!(!app.store.workspaces[0].items[0].done);
        assert!(!app.store.workspaces[0].items[0].children[0].done);

        // Nothing left to undo (the test set up items directly, not via keys).
        assert!(!app.undo());
    }

    #[test]
    fn undo_history_is_bounded_to_twenty() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Task")); // gives us something to toggle
        // 25 recorded edits; only the last 20 should be undoable.
        for _ in 0..25 {
            press(&mut app, KeyCode::Char('x'));
        }
        let mut undone = 0;
        while app.undo() {
            undone += 1;
        }
        assert_eq!(undone, UNDO_DEPTH);
    }

    #[test]
    fn shift_z_confirms_then_unfolds_all() {
        let mut app = App::new(Store::default());
        // Two separate folded parents at the top level.
        app.add_sibling(String::from("A"));
        app.add_child(String::from("A-child"));
        app.selected_path = Some(vec![0]);
        app.toggle_fold(); // fold A
        app.add_sibling(String::from("B"));
        app.add_child(String::from("B-child"));
        app.selected_path = Some(vec![1]);
        app.toggle_fold(); // fold B
        assert!(app.store.workspaces[0].items[0].folded);
        assert!(app.store.workspaces[0].items[1].folded);

        // Shift+Z opens the confirm dialog without changing anything yet.
        press(&mut app, KeyCode::Char('Z'));
        assert_eq!(app.mode, Mode::ConfirmUnfoldAll);
        assert!(app.store.workspaces[0].items[0].folded);

        // n cancels, leaving folds intact.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.store.workspaces[0].items[0].folded);

        // Shift+Z again, then Enter (the default = yes) unfolds everything.
        press(&mut app, KeyCode::Char('Z'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.store.workspaces[0].items[0].folded);
        assert!(!app.store.workspaces[0].items[1].folded);
    }

    #[test]
    fn h_hides_and_unhides_completed_items() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Open"));
        app.add_sibling(String::from("Done"));
        app.selected_path = Some(vec![1]);
        press(&mut app, KeyCode::Char('x')); // complete "Done"
        assert_eq!(app.flattened_items().len(), 2);

        // h hides completed → only "Open" remains visible.
        press(&mut app, KeyCode::Char('h'));
        assert!(app.current_workspace().hide_completed);
        let visible: Vec<_> = app
            .flattened_items()
            .into_iter()
            .map(|f| f.title)
            .collect();
        assert_eq!(visible, vec!["Open"]);
        assert_eq!(app.hidden_count(), 1);

        // h again shows them.
        press(&mut app, KeyCode::Char('h'));
        assert!(!app.current_workspace().hide_completed);
        assert_eq!(app.flattened_items().len(), 2);
    }

    #[test]
    fn hiding_completed_hides_whole_subtree() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));
        app.selected_path = Some(vec![0]);
        press(&mut app, KeyCode::Char('x')); // cascade-completes Parent + Child

        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.flattened_items().len(), 0); // both hidden
        assert_eq!(app.hidden_count(), 2);
    }

    #[test]
    fn completing_under_hide_keeps_highlight_in_place() {
        let mut app = App::new(Store::default());
        for title in ["A", "B", "C", "D"] {
            app.add_sibling(String::from(title));
        }
        press(&mut app, KeyCode::Char('h')); // hide completed (none yet)

        // Select "B" and complete it; it vanishes. Selection should land on
        // "C" (the row that slides into B's spot), not jump to "A" at the top.
        app.selected_path = Some(vec![1]);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(
            app.selected_item().map(|i| i.title.as_str()),
            Some("C"),
            "highlight should follow the deletion rule, not jump to top"
        );

        // Completing the last visible row settles on the previous one.
        app.selected_path = Some(vec![3]); // "D"
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.selected_item().map(|i| i.title.as_str()), Some("C"));
    }

    #[test]
    fn shift_h_confirms_then_deletes_hidden() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Open"));
        app.add_sibling(String::from("Done"));
        app.selected_path = Some(vec![1]);
        press(&mut app, KeyCode::Char('x')); // complete "Done"
        press(&mut app, KeyCode::Char('h')); // hide completed

        // Shift+H opens the confirm dialog.
        press(&mut app, KeyCode::Char('H'));
        assert_eq!(app.mode, Mode::ConfirmDeleteHidden);

        // Enter (default yes) deletes the hidden item; "Open" survives.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        let remaining: Vec<_> = app.store.workspaces[0]
            .items
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert_eq!(remaining, vec!["Open"]);
    }

    #[test]
    fn shift_h_without_hidden_items_does_nothing() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Open"));
        press(&mut app, KeyCode::Char('H')); // hide is off, nothing hidden
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.status, "No hidden items to delete");
    }

    fn workspace_names(app: &App) -> Vec<String> {
        app.store
            .workspaces
            .iter()
            .map(|ws| ws.name.clone())
            .collect()
    }

    fn three_workspace_app() -> App {
        let mut store = Store::default();
        store.workspaces = vec![
            Workspace::new("A"),
            Workspace::new("B"),
            Workspace::new("C"),
        ];
        App::new(store)
    }

    #[test]
    fn m_reorders_workspace_down_on_confirm() {
        let mut app = three_workspace_app();
        app.focus = Focus::Workspaces;
        app.store.selected_workspace = 0; // moving "A"

        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.mode, Mode::MovingWorkspace { .. }));
        press(&mut app, KeyCode::Down); // target 1
        press(&mut app, KeyCode::Down); // target 2
        // Cancel-safe: nothing changed until Enter.
        assert_eq!(workspace_names(&app), vec!["A", "B", "C"]);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(workspace_names(&app), vec!["B", "C", "A"]);
        assert_eq!(app.store.selected_workspace, 2); // selection follows
    }

    #[test]
    fn m_reorder_esc_cancels_without_change() {
        let mut app = three_workspace_app();
        app.focus = Focus::Workspaces;
        app.store.selected_workspace = 2; // moving "C"

        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Esc);

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(workspace_names(&app), vec!["A", "B", "C"]);
    }

    #[test]
    fn workspace_reorder_is_undoable() {
        let mut app = three_workspace_app();
        app.focus = Focus::Workspaces;
        app.store.selected_workspace = 0;

        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(workspace_names(&app), vec!["B", "A", "C"]);

        // Undo restores the original order (one step, not the preview state).
        assert!(app.undo());
        assert_eq!(workspace_names(&app), vec!["A", "B", "C"]);
    }

    #[test]
    fn m_on_tasks_pane_still_moves_item() {
        let mut app = three_workspace_app();
        app.add_sibling(String::from("Task"));
        app.focus = Focus::Tasks;
        app.selected_path = Some(vec![0]);
        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(
            app.mode,
            Mode::Moving {
                dest: MoveDest::Item { .. },
                ..
            }
        ));
    }

    fn press(app: &mut App, code: KeyCode) -> Update {
        app.handle_key(KeyEvent::new(code, crossterm::event::KeyModifiers::NONE))
    }

    #[test]
    fn typing_in_edit_mode_accumulates_input() {
        let mut app = App::new(Store::default());

        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('H'));
        press(&mut app, KeyCode::Char('i'));

        match &app.mode {
            Mode::Editing { input, .. } => assert_eq!(input.text, "Hi"),
            other => panic!("expected editing mode, got {other:?}"),
        }

        press(&mut app, KeyCode::Backspace);
        match &app.mode {
            Mode::Editing { input, .. } => assert_eq!(input.text, "H"),
            other => panic!("expected editing mode, got {other:?}"),
        }

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("H")
        );
    }

    #[test]
    fn folding_hides_children_and_marks_parent() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));
        // Select the parent before folding.
        app.selected_path = Some(vec![0]);

        assert!(app.toggle_fold());
        let flat = app.flattened_items();
        assert_eq!(flat.len(), 1, "child should be hidden when folded");
        assert!(flat[0].has_children);
        assert!(flat[0].folded);

        // Down should not descend into the hidden child.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected_path, Some(vec![0]));

        assert!(app.toggle_fold());
        assert_eq!(app.flattened_items().len(), 2, "child visible again");
    }

    #[test]
    fn folding_leaf_item_is_noop() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Lonely"));
        app.selected_path = Some(vec![0]);

        assert!(!app.toggle_fold());
    }

    #[test]
    fn delete_requires_confirmation() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Doomed"));
        app.selected_path = Some(vec![0]);

        // First d only arms the confirmation; item still present.
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert_eq!(app.flattened_items().len(), 1);

        // n cancels.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.flattened_items().len(), 1);

        // d then d confirms.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.flattened_items().len(), 0);
    }

    #[test]
    fn delete_confirms_with_y() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Doomed"));
        app.selected_path = Some(vec![0]);

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.flattened_items().len(), 0);
    }

    #[test]
    fn move_as_sibling_reorders_items() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("A"));
        app.add_sibling(String::from("B"));
        app.add_sibling(String::from("C"));

        // Move A (index 0) to after C.
        app.selected_path = Some(vec![0]);
        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.mode, Mode::Moving { .. }));
        press(&mut app, KeyCode::Down); // target B
        press(&mut app, KeyCode::Down); // target C
        press(&mut app, KeyCode::Enter);

        let titles: Vec<_> = app
            .flattened_items()
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert_eq!(titles, vec!["B", "C", "A"]);
        // Selection follows the moved item.
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("A")
        );
    }

    #[test]
    fn move_as_child_nests_item() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_sibling(String::from("Loose"));

        // Move "Loose" (index 1) to be a child of "Parent" (index 0).
        app.selected_path = Some(vec![1]);
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Up); // target Parent
        press(&mut app, KeyCode::Right); // nest as child
        press(&mut app, KeyCode::Enter);

        let flat = app.flattened_items();
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].title, "Parent");
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[1].title, "Loose");
        assert_eq!(flat[1].depth, 1);
    }

    #[test]
    fn move_into_own_subtree_is_rejected() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));

        app.selected_path = Some(vec![0]); // Parent
        // Target the child (its own descendant) and try to nest under it.
        assert!(!app.confirm_move(&[0], &[0, 0], true));
        // Tree is unchanged.
        let flat = app.flattened_items();
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].title, "Parent");
        assert_eq!(flat[1].title, "Child");
    }

    #[test]
    fn left_arrow_climbs_then_jumps_to_workspace_choice() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent"));
        app.add_child(String::from("Child"));
        app.selected_path = Some(vec![0, 0]);

        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Left); // climb from Child to Parent level
        match &app.mode {
            Mode::Moving {
                dest: MoveDest::Item { anchor, as_child },
                ..
            } => {
                assert_eq!(anchor, &vec![0]);
                assert!(!as_child);
            }
            other => panic!("expected item move, got {other:?}"),
        }

        press(&mut app, KeyCode::Left); // top level → jump out to workspace choice
        assert!(matches!(
            app.mode,
            Mode::Moving {
                dest: MoveDest::Workspace,
                ..
            }
        ));
        assert_eq!(app.focus, Focus::Workspaces);
    }

    #[test]
    fn workspace_pane_picks_with_up_down_and_steps_in_with_right() {
        let mut store = Store::default();
        store.workspaces.push(Workspace::new("Other"));
        store.workspaces[0].items.push(TodoItem::new("A"));
        store.workspaces[1].items.push(TodoItem::new("X"));
        let mut app = App::new(store);

        app.selected_path = Some(vec![0]); // A in workspace 0
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Left); // top level → workspace choice
        assert_eq!(app.store.selected_workspace, 0);

        // Down picks the next workspace (does not move the item yet).
        press(&mut app, KeyCode::Down);
        assert_eq!(app.store.selected_workspace, 1);
        assert!(matches!(
            app.mode,
            Mode::Moving {
                dest: MoveDest::Workspace,
                ..
            }
        ));

        // Right steps into the highlighted (non-source) workspace's tree.
        press(&mut app, KeyCode::Right);
        assert!(matches!(
            app.mode,
            Mode::Moving {
                dest: MoveDest::Item { .. },
                ..
            }
        ));
        assert_eq!(app.focus, Focus::Tasks);
    }

    #[test]
    fn cross_workspace_move_places_precisely() {
        let mut store = Store::default();
        store.workspaces.push(Workspace::new("Other"));
        store.workspaces[0].items.push(TodoItem::new("A"));
        store.workspaces[1].items.push(TodoItem::new("X"));
        store.workspaces[1].items.push(TodoItem::new("Y"));
        let mut app = App::new(store);

        // Move "A" from workspace 0 to sit after "X" in workspace 1.
        app.selected_path = Some(vec![0]);
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Left); // out to workspace choice
        press(&mut app, KeyCode::Down); // pick "Other"
        press(&mut app, KeyCode::Right); // step in, anchor at X (index 0)
        press(&mut app, KeyCode::Enter); // drop after X

        // Source workspace no longer holds A.
        assert!(app.store.workspaces[0].items.is_empty());
        // Destination order is X, A, Y and we're now viewing it with A selected.
        let titles: Vec<_> = app.store.workspaces[1]
            .items
            .iter()
            .map(|item| item.title.clone())
            .collect();
        assert_eq!(titles, vec!["X", "A", "Y"]);
        assert_eq!(app.store.selected_workspace, 1);
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("A")
        );
    }

    #[test]
    fn move_item_to_another_workspace() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Task"));
        app.add_workspace(String::from("Other"));
        app.store.selected_workspace = 0;
        app.focus = Focus::Tasks;
        app.selected_path = Some(vec![0]);

        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('w')); // jump to workspace choice
        assert!(matches!(
            app.mode,
            Mode::Moving {
                dest: MoveDest::Workspace,
                ..
            }
        ));
        press(&mut app, KeyCode::Down); // select "Other"
        press(&mut app, KeyCode::Enter);

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.store.workspaces[0].items.len(), 0);
        assert_eq!(app.store.workspaces[1].items.len(), 1);
        assert_eq!(app.store.workspaces[1].items[0].title, "Task");
        assert_eq!(app.store.selected_workspace, 1);
    }

    #[test]
    fn w_focuses_workspace_then_opens_dialog() {
        let mut app = App::new(Store::default());
        assert_eq!(app.focus, Focus::Tasks);

        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.focus, Focus::Workspaces);
        assert_eq!(app.mode, Mode::Normal);

        press(&mut app, KeyCode::Char('w'));
        assert!(matches!(
            app.mode,
            Mode::Editing {
                target: EditTarget::NewWorkspace,
                ..
            }
        ));
    }

    #[test]
    fn e_on_workspaces_pane_renames_workspace() {
        let mut app = App::new(Store::default());
        let original = app.current_workspace().name.clone();

        app.focus = Focus::Workspaces;
        press(&mut app, KeyCode::Char('e'));
        match &app.mode {
            Mode::Editing {
                target: EditTarget::RenameWorkspace,
                input,
            } => assert_eq!(input.text, original),
            other => panic!("expected workspace rename dialog, got {other:?}"),
        }

        // Wipe the prefill and type a new name.
        for _ in 0..original.chars().count() {
            press(&mut app, KeyCode::Backspace);
        }
        for ch in "Errands".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        press(&mut app, KeyCode::Enter);

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.current_workspace().name, "Errands");
    }

    #[test]
    fn e_on_tasks_pane_still_renames_item() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Task"));
        app.focus = Focus::Tasks;

        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(
            app.mode,
            Mode::Editing {
                target: EditTarget::RenameSelected,
                ..
            }
        ));
    }

    #[test]
    fn workspace_name_typing_stops_at_cap() {
        let mut app = App::new(Store::default());
        app.focus = Focus::Workspaces;
        press(&mut app, KeyCode::Char('w'));

        for _ in 0..(WORKSPACE_NAME_MAX + 10) {
            press(&mut app, KeyCode::Char('x'));
        }
        match &app.mode {
            Mode::Editing { input, .. } => {
                assert_eq!(input.char_count(), WORKSPACE_NAME_MAX);
            }
            other => panic!("expected editing mode, got {other:?}"),
        }

        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.current_workspace().name.chars().count(),
            WORKSPACE_NAME_MAX
        );
    }

    #[test]
    fn workspace_name_pasted_past_cap_is_truncated_on_enter() {
        let mut app = App::new(Store::default());
        app.focus = Focus::Workspaces;
        press(&mut app, KeyCode::Char('w'));
        app.paste("a".repeat(WORKSPACE_NAME_MAX + 5));
        press(&mut app, KeyCode::Enter);

        assert_eq!(
            app.current_workspace().name,
            "a".repeat(WORKSPACE_NAME_MAX)
        );
    }

    #[test]
    fn item_titles_are_not_length_capped() {
        let mut app = App::new(Store::default());
        press(&mut app, KeyCode::Char('a'));
        for _ in 0..(WORKSPACE_NAME_MAX + 10) {
            press(&mut app, KeyCode::Char('y'));
        }
        press(&mut app, KeyCode::Enter);

        assert_eq!(
            app.selected_item().map(|item| item.title.chars().count()),
            Some(WORKSPACE_NAME_MAX + 10)
        );
    }

    #[test]
    fn editing_cursor_moves_and_inserts_mid_text() {
        let mut app = App::new(Store::default());
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));

        // Left moves the cursor between 'a' and 'b'; typing inserts there.
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('c'));
        match &app.mode {
            Mode::Editing { input, .. } => {
                assert_eq!(input.text, "acb");
                assert_eq!(input.cursor, 2);
            }
            other => panic!("expected editing mode, got {other:?}"),
        }

        // Backspace removes the char before the cursor ('c'), Delete the one
        // under it ('b'), Home/End jump to the extremes.
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Char('z'));
        match &app.mode {
            Mode::Editing { input, .. } => {
                assert_eq!(input.text, "za");
                assert_eq!(input.cursor, 1);
            }
            other => panic!("expected editing mode, got {other:?}"),
        }
    }

    #[test]
    fn wrap_words_breaks_at_word_boundaries() {
        assert_eq!(
            wrap_words("buy milk and also eggs", 10),
            vec!["buy milk", "and also", "eggs"]
        );
    }

    #[test]
    fn wrap_words_hard_splits_long_words() {
        assert_eq!(
            wrap_words("see https://example.com/really-long", 12),
            vec!["see", "https://exam", "ple.com/real", "ly-long"]
        );
    }

    #[test]
    fn wrap_words_short_text_is_one_line() {
        assert_eq!(wrap_words("hi", 10), vec!["hi"]);
        assert_eq!(wrap_words("", 10), vec![""]);
    }

    #[test]
    fn editing_up_down_move_by_wrap_width() {
        let mut app = App::new(Store::default());
        app.set_edit_wrap_width(5);

        press(&mut app, KeyCode::Char('a'));
        for ch in "abcdefghijkl".chars() {
            press(&mut app, KeyCode::Char(ch));
        }

        // Cursor starts at the end (12); Up climbs a row at a time, pinning
        // to the start on the first row; Down descends and pins to the end.
        let cursor = |app: &App| match &app.mode {
            Mode::Editing { input, .. } => input.cursor,
            other => panic!("expected editing mode, got {other:?}"),
        };
        assert_eq!(cursor(&app), 12);
        press(&mut app, KeyCode::Up);
        assert_eq!(cursor(&app), 7);
        press(&mut app, KeyCode::Up);
        assert_eq!(cursor(&app), 2);
        press(&mut app, KeyCode::Up);
        assert_eq!(cursor(&app), 0);
        press(&mut app, KeyCode::Down);
        assert_eq!(cursor(&app), 5);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(cursor(&app), 12);
    }

    #[test]
    fn edit_field_handles_multibyte_chars() {
        let mut field = EditField::new("héllo");
        assert_eq!(field.cursor, 5);
        field.move_left();
        field.move_left();
        field.move_left();
        field.move_left();
        field.delete(); // removes 'é'
        assert_eq!(field.text, "hllo");
        field.insert('ü');
        assert_eq!(field.text, "hüllo");
        assert_eq!(field.cursor, 2);
    }

    #[test]
    fn copy_selected_returns_title() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Buy milk"));
        app.selected_path = Some(vec![0]);

        assert_eq!(app.copy_selected(), Some(String::from("Buy milk")));
    }

    #[test]
    fn copy_with_no_selection_returns_none() {
        let mut app = App::new(Store::default());
        assert_eq!(app.copy_selected(), None);
    }

    #[test]
    fn paste_opens_add_dialog_then_accepts() {
        let mut app = App::new(Store::default());
        app.paste(String::from("Pasted task\n"));

        match &app.mode {
            Mode::Editing {
                target: EditTarget::NewSibling,
                input,
            } => assert_eq!(input.text, "Pasted task"),
            other => panic!("expected add dialog, got {other:?}"),
        }

        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("Pasted task")
        );
    }

    #[test]
    fn paste_while_editing_appends_to_input() {
        let mut app = App::new(Store::default());
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('H'));
        app.paste(String::from("ello\nworld"));

        match &app.mode {
            Mode::Editing { input, .. } => assert_eq!(input.text, "Hello world"),
            other => panic!("expected editing mode, got {other:?}"),
        }
    }

    #[test]
    fn help_key_restores_controls() {
        let mut app = App::new(Store::default());
        app.status = String::from("something else");
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.status, CONTROLS);
    }

    #[test]
    fn focus_toggles_navigation_target() {
        let mut app = App::new(Store::default());
        app.add_workspace(String::from("Work"));
        app.add_workspace(String::from("Home"));
        app.store.selected_workspace = 0;

        // Focus the workspace pane; up/down moves between workspaces.
        press(&mut app, KeyCode::Left);
        assert_eq!(app.focus, Focus::Workspaces);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.store.selected_workspace, 1);

        // Focus the tasks pane; up/down moves between items, not workspaces.
        press(&mut app, KeyCode::Right);
        assert_eq!(app.focus, Focus::Tasks);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.store.selected_workspace, 1);
    }

    fn cli(args: &[&str]) -> Result<CliArgs, String> {
        let mut full = vec![String::from("jot-cli")];
        full.extend(args.iter().map(|arg| arg.to_string()));
        parse_args(full)
    }

    #[test]
    fn parse_add_flags() {
        let parsed = cli(&["-a", "Buy milk", "-w", "Home", "--silent"]).expect("parse");
        assert_eq!(parsed.add.as_deref(), Some("Buy milk"));
        assert_eq!(parsed.workspace.as_deref(), Some("Home"));
        assert!(parsed.prompt_add);
        assert!(parsed.silent);

        let long = cli(&["--add", "Task", "--workspace", "Work"]).expect("parse");
        assert_eq!(long.add.as_deref(), Some("Task"));
        assert_eq!(long.workspace.as_deref(), Some("Work"));
        assert!(!long.silent);
    }

    #[test]
    fn parse_workspace_triggers_prompt_with_optional_name() {
        // Bare -w: prompt mode, no name (top workspace).
        let bare = cli(&["-w"]).expect("parse");
        assert!(bare.prompt_add);
        assert_eq!(bare.workspace, None);
        assert!(bare.add.is_none());

        // -w followed by a flag must not swallow the flag as the name.
        let with_flag = cli(&["-w", "--silent"]).expect("parse");
        assert!(with_flag.prompt_add);
        assert_eq!(with_flag.workspace, None);
        assert!(with_flag.silent);

        // -w with a name targets that workspace.
        let named = cli(&["--workspace", "Home"]).expect("parse");
        assert!(named.prompt_add);
        assert_eq!(named.workspace.as_deref(), Some("Home"));
    }

    #[test]
    fn parse_add_without_value_errors() {
        assert!(cli(&["-a"]).is_err());
    }

    #[test]
    fn add_item_defaults_to_top_workspace() {
        let mut store = Store {
            workspaces: vec![Workspace::new("Inbox"), Workspace::new("Other")],
            selected_workspace: 1,
            auto_sync: false,
        };

        let name = store.add_item("Top task", None).expect("add");
        assert_eq!(name, "Inbox");
        assert_eq!(store.workspaces[0].items.len(), 1);
        assert_eq!(store.workspaces[0].items[0].title, "Top task");
        assert_eq!(store.workspaces[1].items.len(), 0);
    }

    #[test]
    fn add_item_targets_named_workspace() {
        let mut store = Store {
            workspaces: vec![Workspace::new("Inbox"), Workspace::new("Home")],
            selected_workspace: 0,
            auto_sync: false,
        };

        let name = store.add_item("Chore", Some("Home")).expect("add");
        assert_eq!(name, "Home");
        assert_eq!(store.workspaces[1].items[0].title, "Chore");
    }

    #[test]
    fn add_item_unknown_workspace_errors() {
        let mut store = Store::default();
        let result = store.add_item("Nope", Some("Missing"));
        assert_eq!(result, Err(String::from("workspace not found: Missing")));
    }

    #[test]
    fn app_keeps_selection_when_removing_items() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("First"));
        app.add_sibling(String::from("Second"));
        app.selected_path = Some(vec![0]);

        assert!(app.remove_selected());

        assert_eq!(app.selected_path, Some(vec![0]));
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("Second")
        );
    }

    #[test]
    fn removing_middle_item_selects_the_one_that_slides_up() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("A"));
        app.add_sibling(String::from("B"));
        app.add_sibling(String::from("C"));

        // Delete the middle item; selection should land on C (which now fills
        // B's old slot), not jump elsewhere.
        app.selected_path = Some(vec![1]);
        assert!(app.remove_selected());
        assert_eq!(app.selected_path, Some(vec![1]));
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("C")
        );
    }

    #[test]
    fn removing_last_child_climbs_up_instead_of_jumping_to_a_higher_level() {
        let mut app = App::new(Store::default());
        // Parent A with two children, then a top-level Parent B.
        app.add_sibling(String::from("Parent A")); // [0]
        app.add_child(String::from("Child A1")); // [0, 0]
        app.add_sibling(String::from("Child A2")); // [0, 1]
        app.selected_path = Some(vec![0]);
        app.add_sibling(String::from("Parent B")); // [1]

        // Delete the last child (A2). The row that would slide in is Parent B
        // (a shallower level), so the cursor should climb up to Child A1.
        app.selected_path = Some(vec![0, 1]);
        assert!(app.remove_selected());
        assert_eq!(app.selected_path, Some(vec![0, 0]));
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("Child A1")
        );
    }

    #[test]
    fn removing_only_child_climbs_to_parent() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("Parent A")); // [0]
        app.add_child(String::from("Only child")); // [0, 0]
        app.selected_path = Some(vec![0]);
        app.add_sibling(String::from("Parent B")); // [1]

        // Deleting the only child leaves Parent B sliding up; climb to Parent A.
        app.selected_path = Some(vec![0, 0]);
        assert!(app.remove_selected());
        assert_eq!(app.selected_path, Some(vec![0]));
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("Parent A")
        );
    }

    #[test]
    fn removing_last_item_selects_the_previous_one() {
        let mut app = App::new(Store::default());
        app.add_sibling(String::from("A"));
        app.add_sibling(String::from("B"));

        app.selected_path = Some(vec![1]);
        assert!(app.remove_selected());
        assert_eq!(app.selected_path, Some(vec![0]));
        assert_eq!(
            app.selected_item().map(|item| item.title.as_str()),
            Some("A")
        );
    }
}
