use std::{
    env, fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
    #[serde(default)]
    pub children: Vec<TodoItem>,
    /// When true, this item's children are hidden in the list.
    #[serde(default)]
    pub folded: bool,
}

impl TodoItem {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            done: false,
            children: Vec::new(),
            folded: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub name: String,
    #[serde(default)]
    pub items: Vec<TodoItem>,
}

impl Workspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Store {
    #[serde(default = "default_workspaces")]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub selected_workspace: usize,
}

fn default_workspaces() -> Vec<Workspace> {
    vec![Workspace::new("Inbox")]
}

impl Default for Store {
    fn default() -> Self {
        Self {
            workspaces: default_workspaces(),
            selected_workspace: 0,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    NewWorkspace,
    NewSibling,
    NewChild,
    RenameSelected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Editing { target: EditTarget, input: String },
    ConfirmDelete,
    /// Relocating `origin`; the current selection marks the drop position,
    /// nesting the item as a child when `as_child` is set.
    Moving { origin: Vec<usize>, as_child: bool },
}

pub const CONTROLS: &str = "q quit • ←/→ focus • ↑/↓ move • a add • o child • e rename • x toggle • z fold • m move • d delete • w workspace • ? help";

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
}

#[derive(Debug, Clone)]
pub struct App {
    pub store: Store,
    pub selected_path: Option<Vec<usize>>,
    pub mode: Mode,
    pub focus: Focus,
    pub status: String,
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
        flatten_items(&self.current_workspace().items, 0, &mut path, &mut flat);
        flat
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Update {
        match self.mode.clone() {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Editing { target, input } => self.handle_editing_key(key, target, input),
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key),
            Mode::Moving { origin, as_child } => self.handle_moving_key(key, origin, as_child),
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
            KeyCode::Char('h') | KeyCode::Left => {
                self.set_focus(Focus::Workspaces);
                Update::None
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.set_focus(Focus::Tasks);
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
                self.mode = Mode::Editing {
                    target: EditTarget::NewWorkspace,
                    input: String::new(),
                };
                self.status = String::from("New workspace name");
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
            KeyCode::Char('m') => {
                match self.selected_path.clone() {
                    Some(origin) => {
                        self.focus = Focus::Tasks;
                        self.mode = Mode::Moving {
                            origin,
                            as_child: false,
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

    fn handle_moving_key(&mut self, key: KeyEvent, origin: Vec<usize>, as_child: bool) -> Update {
        let mut as_child = as_child;
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = String::from("Move canceled");
                return Update::None;
            }
            KeyCode::Enter => {
                let target = self.selected_path.clone();
                self.mode = Mode::Normal;
                return match target {
                    Some(target) if self.confirm_move(&origin, &target, as_child) => Update::Save,
                    _ => Update::None,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Right | KeyCode::Char('l') => as_child = true,
            KeyCode::Left | KeyCode::Char('h') => as_child = false,
            _ => {}
        }

        self.status = self.move_status(as_child);
        self.mode = Mode::Moving { origin, as_child };
        Update::None
    }

    fn move_status(&self, as_child: bool) -> String {
        let target = self
            .selected_item()
            .map(|item| item.title.clone())
            .unwrap_or_default();
        if as_child {
            format!("Move: nest under \"{target}\" • ← back • Enter confirm • Esc cancel")
        } else {
            format!("Move: after \"{target}\" • → nest as child • Enter confirm • Esc cancel")
        }
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

    fn toggle_selected(&mut self) -> bool {
        if let Some(item) = self.selected_item_mut() {
            item.done = !item.done;
            self.status = if item.done {
                format!("Completed: {}", item.title)
            } else {
                format!("Reopened: {}", item.title)
            };
            true
        } else {
            false
        }
    }

    fn remove_selected(&mut self) -> bool {
        let Some(path) = self.selected_path.clone() else {
            return false;
        };

        let next_selection = next_path_after_removal(&self.flattened_items(), &path);
        let items = &mut self.current_workspace_mut().items;
        let removed = remove_at_path(items, &path);
        if removed {
            self.selected_path = next_selection;
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
    depth: usize,
    path: &mut Vec<usize>,
    flat: &mut Vec<FlatItem>,
) {
    for (index, item) in items.iter().enumerate() {
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
            flatten_items(&item.children, depth + 1, path, flat);
        }
        path.pop();
    }
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
    let title = moved.title.clone();

    if as_child {
        let parent = item_mut(items, target_adj)?;
        parent.folded = false;
        parent.children.push(moved);
        let mut new_path = target_adj.to_vec();
        new_path.push(parent.children.len() - 1);
        Some((new_path, title))
    } else {
        let (&target_index, parent_path) = target_adj.split_last()?;
        let insert_index = target_index + 1;
        let list = list_mut(items, parent_path)?;
        let insert_index = insert_index.min(list.len());
        list.insert(insert_index, moved);
        let mut new_path = parent_path.to_vec();
        new_path.push(insert_index);
        Some((new_path, title))
    }
}

fn next_path_after_removal(flat: &[FlatItem], removed: &[usize]) -> Option<Vec<usize>> {
    let position = flat.iter().position(|item| item.path == removed)?;
    flat.get(position + 1)
        .or_else(|| position.checked_sub(1).and_then(|index| flat.get(index)))
        .map(|item| item.path.clone())
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<PathBuf, String> {
    let mut args = args.into_iter();
    let _ = args.next();
    let mut data_path = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-path" => {
                let value = args
                    .next()
                    .ok_or_else(|| String::from("expected a path after --data-path"))?;
                data_path = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                return Err(String::from(
                    "Usage: jot-cli [--data-path <path>]\n\nControls:\n  ←/→ or h/l  focus workspaces / tasks pane\n  Tab         toggle focused pane\n  ↑/↓ or k/j  move within focused pane\n  a add item\n  o add child item\n  e rename item\n  x toggle done\n  z fold/unfold nested items\n  m move item (→ nest as child)\n  d delete item\n  w new workspace\n  ? show controls\n  q quit",
                ));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(data_path.unwrap_or_else(default_data_path))
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
                }],
            }],
            selected_workspace: 0,
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
}
