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
        }
    }
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
}

impl Workspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            items: Vec::new(),
            google_tasklist: None,
            hide_completed: false,
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
        fs::write(path, payload)
    }

    pub fn normalize(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces = default_workspaces();
        }

        if self.selected_workspace >= self.workspaces.len() {
            self.selected_workspace = self.workspaces.len().saturating_sub(1);
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    NewWorkspace,
    NewSibling,
    NewChild,
    RenameSelected,
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
    Editing { target: EditTarget, input: String },
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

#[derive(Debug, Clone)]
pub struct App {
    pub store: Store,
    pub selected_path: Option<Vec<usize>>,
    pub mode: Mode,
    pub focus: Focus,
    pub status: String,
    /// Bounded history of pre-edit snapshots, newest last. Capped at
    /// [`UNDO_DEPTH`]; in memory only.
    undo_stack: Vec<Snapshot>,
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
            undo_stack: Vec::new(),
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
            input.push_str(&sanitized);
            return;
        }
        self.focus = Focus::Tasks;
        self.mode = Mode::Editing {
            target: EditTarget::NewSibling,
            input: sanitized.trim().to_string(),
        };
        self.status = String::from("Pasted — Enter to add item, Esc to cancel");
    }

    /// Replace the status line text (used by the event loop after a sync).
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
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
        };
        if matches!(update, Update::Save) {
            self.record_undo(before);
        }
        update
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
                self.store = snapshot.store;
                self.selected_path = snapshot.selected_path;
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
                    input: String::new(),
                };
                self.status = String::from("New item name");
                Update::None
            }
            KeyCode::Char('o') => {
                self.mode = Mode::Editing {
                    target: EditTarget::NewChild,
                    input: String::new(),
                };
                self.status = String::from("New child item name");
                Update::None
            }
            KeyCode::Char('w') => {
                if self.focus == Focus::Workspaces {
                    self.mode = Mode::Editing {
                        target: EditTarget::NewWorkspace,
                        input: String::new(),
                    };
                    self.status = String::from("New workspace name");
                } else {
                    self.focus = Focus::Workspaces;
                    self.status = format!("Workspace: {}", self.current_workspace().name);
                }
                Update::None
            }
            KeyCode::Char('e') => {
                let current_title = self
                    .selected_item()
                    .map(|item| item.title.clone())
                    .unwrap_or_default();
                self.mode = Mode::Editing {
                    target: EditTarget::RenameSelected,
                    input: current_title,
                };
                self.status = String::from("Rename selected item");
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
        mut input: String,
    ) -> Update {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Canceled");
                Update::None
            }
            KeyCode::Enter => {
                let value = input.trim().to_string();
                self.mode = Mode::Normal;
                if value.is_empty() {
                    self.status = String::from("Ignored empty input");
                    return Update::None;
                }

                let changed = match target {
                    EditTarget::NewWorkspace => self.add_workspace(value),
                    EditTarget::NewSibling => self.add_sibling(value),
                    EditTarget::NewChild => self.add_child(value),
                    EditTarget::RenameSelected => self.rename_selected(value),
                };

                if changed { Update::Save } else { Update::None }
            }
            KeyCode::Backspace => {
                input.pop();
                self.mode = Mode::Editing { target, input };
                Update::None
            }
            KeyCode::Char(ch) => {
                input.push(ch);
                self.mode = Mode::Editing { target, input };
                Update::None
            }
            _ => {
                // Preserve the in-progress input for any unhandled key.
                self.mode = Mode::Editing { target, input };
                Update::None
            }
        }
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
    pub data_path: PathBuf,
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

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliArgs, String> {
    let mut args = args.into_iter().peekable();
    let _ = args.next();
    let mut data_path = None;
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
                    "Usage: jot-cli [--data-path <path>] [-a|--add <task>] [-w|--workspace [name]] [--sync] [--silent]\n\nAdd a task without the full TUI:\n  -a, --add <task>          add a task and exit (defaults to the top workspace)\n  -w, --workspace [name]    open an inline input field to add a task, then exit\n                            (defaults to the top workspace; name is optional)\n  --sync                    sync Google-linked workspaces and exit\n                            (requires a build with --features google)\n  --silent                  print nothing on success (errors still shown)\n\nControls:\n  ←/→         focus workspaces / tasks pane\n  Tab         toggle focused pane\n  ↑/↓ or k/j  move within focused pane\n  a add item\n  o add child item\n  e rename item\n  x toggle done\n  z fold/unfold nested items\n  Z unfold all items\n  h hide/show completed items\n  H delete hidden (completed) items\n  m move item (→ nest as child)\n  d delete item\n  w new workspace\n  ? show controls\n  q quit",
                ));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(CliArgs {
        data_path: data_path.unwrap_or_else(default_data_path),
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
                }],
                google_tasklist: None,
                hide_completed: false,
            }],
            selected_workspace: 0,
            auto_sync: false,
        };

        store.save(&path).expect("save store");
        let loaded = Store::load(&path).expect("load store");

        assert_eq!(loaded, store);
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
            Mode::Editing { input, .. } => assert_eq!(input, "Hi"),
            other => panic!("expected editing mode, got {other:?}"),
        }

        press(&mut app, KeyCode::Backspace);
        match &app.mode {
            Mode::Editing { input, .. } => assert_eq!(input, "H"),
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
            } => assert_eq!(input, "Pasted task"),
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
            Mode::Editing { input, .. } => assert_eq!(input, "Hello world"),
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
