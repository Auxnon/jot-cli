//! Two-way reconciliation between local workspaces and Google task lists.
//!
//! Only compiled with the `google` feature. The reconciliation logic works
//! against the [`TaskBackend`] trait so it can be unit-tested with a fake
//! backend; [`GoogleClient`] is the real implementation.
//!
//! Scope (first cut): a flat list with one level of nesting — top-level items
//! and their direct children sync to Google parent/child tasks. Deeper
//! descendants are left local-only and untouched.

use std::collections::{HashMap, HashSet};

use crate::google::{GoogleClient, Task};
use crate::{Store, SyncMeta, TodoItem};

/// What a backend must provide for reconciliation. Implemented by
/// [`GoogleClient`]; fakes implement it in tests.
pub trait TaskBackend {
    fn list(&self, tasklist: &str) -> Result<Vec<Task>, String>;
    fn insert(
        &self,
        tasklist: &str,
        title: &str,
        done: bool,
        parent: Option<&str>,
    ) -> Result<Task, String>;
    fn patch(
        &self,
        tasklist: &str,
        id: &str,
        title: Option<&str>,
        done: Option<bool>,
    ) -> Result<(), String>;
}

impl TaskBackend for GoogleClient {
    fn list(&self, tasklist: &str) -> Result<Vec<Task>, String> {
        self.list_tasks(tasklist)
    }
    fn insert(
        &self,
        tasklist: &str,
        title: &str,
        done: bool,
        parent: Option<&str>,
    ) -> Result<Task, String> {
        self.insert_task(tasklist, title, done, parent)
    }
    fn patch(
        &self,
        tasklist: &str,
        id: &str,
        title: Option<&str>,
        done: Option<bool>,
    ) -> Result<(), String> {
        self.patch_task(tasklist, id, title, done)
    }
}

/// Counts of what a sync did, for the status line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncSummary {
    pub pulled: usize,
    pub pushed: usize,
    pub removed: usize,
}

impl SyncSummary {
    fn merge(&mut self, other: SyncSummary) {
        self.pulled += other.pulled;
        self.pushed += other.pushed;
        self.removed += other.removed;
    }

    pub fn describe(&self) -> String {
        format!(
            "Synced • {} pulled, {} pushed, {} removed",
            self.pulled, self.pushed, self.removed
        )
    }
}

/// Sync every Google-linked workspace in the store. Authenticates once (which
/// may open a browser on first use), then reconciles each linked list.
pub fn sync_store(store: &mut Store) -> Result<SyncSummary, String> {
    let targets: Vec<(usize, String)> = store
        .workspaces
        .iter()
        .enumerate()
        .filter_map(|(i, ws)| ws.google_tasklist.clone().map(|list| (i, list)))
        .collect();

    if targets.is_empty() {
        return Err(String::from("No Google-linked workspace to sync"));
    }

    let client = GoogleClient::connect()?;
    let mut total = SyncSummary::default();
    for (index, tasklist) in targets {
        let summary = sync_items(&client, &tasklist, &mut store.workspaces[index].items)?;
        total.merge(summary);
    }
    Ok(total)
}

/// Reconcile one workspace's items against one Google task list.
pub fn sync_items<B: TaskBackend>(
    backend: &B,
    tasklist: &str,
    items: &mut Vec<TodoItem>,
) -> Result<SyncSummary, String> {
    let remote = backend.list(tasklist)?;
    let remote_by_id: HashMap<String, Task> = remote
        .iter()
        .filter_map(|task| task.id.clone().map(|id| (id, task.clone())))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut summary = SyncSummary::default();

    // --- Phase A: reconcile items we already have locally (top level, then
    // one level of children). Remote-deleted items are dropped. ---
    let mut remove_top: Vec<usize> = Vec::new();
    // Index loop: we borrow items[i] and then items[i].children separately, and
    // defer removals — an iterator can't express that.
    #[allow(clippy::needless_range_loop)]
    for i in 0..items.len() {
        let present = reconcile_one(
            backend,
            tasklist,
            &mut items[i],
            None,
            &remote_by_id,
            &mut seen,
            &mut summary,
        )?;
        if !present {
            remove_top.push(i);
            continue;
        }

        // The parent now has a google id; reconcile its direct children.
        let parent_gid = items[i].sync.as_ref().map(|m| m.google_id.clone());
        if let Some(parent_gid) = parent_gid {
            let mut remove_child: Vec<usize> = Vec::new();
            for j in 0..items[i].children.len() {
                let present = reconcile_one(
                    backend,
                    tasklist,
                    &mut items[i].children[j],
                    Some(&parent_gid),
                    &remote_by_id,
                    &mut seen,
                    &mut summary,
                )?;
                if !present {
                    remove_child.push(j);
                }
            }
            for &j in remove_child.iter().rev() {
                items[i].children.remove(j);
                summary.removed += 1;
            }
        }
    }
    for &i in remove_top.iter().rev() {
        items.remove(i);
        summary.removed += 1;
    }

    // --- Phase B: pull remote tasks we haven't seen. Parents (no parent id)
    // first, so a child can attach to a freshly-pulled parent. ---
    let mut unseen: Vec<&Task> = remote
        .iter()
        .filter(|task| {
            task.id
                .as_ref()
                .is_some_and(|id| !seen.contains(id))
                && task.deleted != Some(true)
        })
        .collect();
    unseen.sort_by_key(|task| task.parent.is_some());

    for task in unseen {
        let Some(id) = task.id.clone() else { continue };
        let title = task.title.clone().unwrap_or_default();
        let done = task.is_done();
        let mut item = TodoItem::new(title.clone());
        item.done = done;
        item.sync = Some(SyncMeta {
            google_id: id,
            synced_title: title,
            synced_done: done,
        });

        if let Some(parent_id) = &task.parent
            && let Some(parent) = find_top_by_gid(items, parent_id)
        {
            parent.children.push(item);
            summary.pulled += 1;
            continue;
        }
        items.push(item);
        summary.pulled += 1;
    }

    Ok(summary)
}

/// Reconcile a single item against the remote set. Returns `false` when the
/// item was linked to a remote task that no longer exists (caller removes it).
fn reconcile_one<B: TaskBackend>(
    backend: &B,
    tasklist: &str,
    item: &mut TodoItem,
    parent_gid: Option<&str>,
    remote_by_id: &HashMap<String, Task>,
    seen: &mut HashSet<String>,
    summary: &mut SyncSummary,
) -> Result<bool, String> {
    match item.sync.clone() {
        Some(meta) => {
            let Some(task) = remote_by_id.get(&meta.google_id) else {
                // Linked, but the remote task is gone — remote deletion wins.
                return Ok(false);
            };
            seen.insert(meta.google_id.clone());

            let remote_done = task.is_done();
            let remote_title = task.title.clone().unwrap_or_default();

            // Three-way merge against the last-synced baseline: the side that
            // changed wins; if both changed, local wins.
            let final_done = merge_field(item.done, remote_done, meta.synced_done);
            let final_title =
                merge_field(item.title.clone(), remote_title.clone(), meta.synced_title.clone());

            // Push whatever differs from the remote's current value.
            let push_title = (final_title != remote_title).then(|| final_title.clone());
            let push_done = (final_done != remote_done).then_some(final_done);
            if push_title.is_some() || push_done.is_some() {
                backend.patch(tasklist, &meta.google_id, push_title.as_deref(), push_done)?;
                summary.pushed += 1;
            }
            if final_title != item.title || final_done != item.done {
                summary.pulled += 1;
            }

            item.title = final_title.clone();
            item.done = final_done;
            item.sync = Some(SyncMeta {
                google_id: meta.google_id,
                synced_title: final_title,
                synced_done: final_done,
            });
            Ok(true)
        }
        None => {
            // New local item — create it on Google and record the link.
            let task = backend.insert(tasklist, &item.title, item.done, parent_gid)?;
            let id = task
                .id
                .ok_or_else(|| String::from("inserted Google task had no id"))?;
            seen.insert(id.clone());
            item.sync = Some(SyncMeta {
                google_id: id,
                synced_title: item.title.clone(),
                synced_done: item.done,
            });
            summary.pushed += 1;
            Ok(true)
        }
    }
}

/// Resolve one field three ways: whichever side diverged from the last-synced
/// baseline wins; if both diverged, prefer the local value.
fn merge_field<T: PartialEq>(local: T, remote: T, baseline: T) -> T {
    let local_changed = local != baseline;
    let remote_changed = remote != baseline;
    if local_changed {
        local
    } else if remote_changed {
        remote
    } else {
        baseline
    }
}

fn find_top_by_gid<'a>(items: &'a mut [TodoItem], gid: &str) -> Option<&'a mut TodoItem> {
    items
        .iter_mut()
        .find(|item| item.sync.as_ref().is_some_and(|m| m.google_id == gid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory backend that records calls and serves canned tasks.
    #[derive(Default)]
    struct FakeBackend {
        remote: Vec<Task>,
        inserted: RefCell<Vec<(String, bool, Option<String>)>>, // title, done, parent
        patched: RefCell<Vec<(String, Option<String>, Option<bool>)>>, // id, title, done
        next_id: RefCell<usize>,
    }

    impl FakeBackend {
        fn with_remote(remote: Vec<Task>) -> Self {
            Self {
                remote,
                next_id: RefCell::new(1000),
                ..Self::default()
            }
        }
    }

    impl TaskBackend for FakeBackend {
        fn list(&self, _tasklist: &str) -> Result<Vec<Task>, String> {
            Ok(self.remote.clone())
        }
        fn insert(
            &self,
            _tasklist: &str,
            title: &str,
            done: bool,
            parent: Option<&str>,
        ) -> Result<Task, String> {
            self.inserted.borrow_mut().push((
                title.to_string(),
                done,
                parent.map(str::to_string),
            ));
            let mut n = self.next_id.borrow_mut();
            *n += 1;
            Ok(Task {
                id: Some(format!("g{n}")),
                title: Some(title.to_string()),
                status: Some(if done { "completed" } else { "needsAction" }.to_string()),
                parent: parent.map(str::to_string),
                ..Task::default()
            })
        }
        fn patch(
            &self,
            _tasklist: &str,
            id: &str,
            title: Option<&str>,
            done: Option<bool>,
        ) -> Result<(), String> {
            self.patched
                .borrow_mut()
                .push((id.to_string(), title.map(str::to_string), done));
            Ok(())
        }
    }

    fn remote_task(id: &str, title: &str, done: bool, parent: Option<&str>) -> Task {
        Task {
            id: Some(id.to_string()),
            title: Some(title.to_string()),
            status: Some(if done { "completed" } else { "needsAction" }.to_string()),
            parent: parent.map(str::to_string),
            ..Task::default()
        }
    }

    fn synced(item: &mut TodoItem, id: &str) {
        item.sync = Some(SyncMeta {
            google_id: id.to_string(),
            synced_title: item.title.clone(),
            synced_done: item.done,
        });
    }

    #[test]
    fn pulls_new_remote_tasks() {
        let backend = FakeBackend::with_remote(vec![remote_task("g1", "From phone", false, None)]);
        let mut items = Vec::new();
        let summary = sync_items(&backend, "@default", &mut items).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "From phone");
        assert_eq!(items[0].sync.as_ref().unwrap().google_id, "g1");
        assert_eq!(summary.pulled, 1);
    }

    #[test]
    fn pushes_new_local_tasks() {
        let backend = FakeBackend::with_remote(vec![]);
        let mut items = vec![TodoItem::new("Buy milk")];
        let summary = sync_items(&backend, "@default", &mut items).unwrap();

        assert_eq!(backend.inserted.borrow().len(), 1);
        assert_eq!(backend.inserted.borrow()[0].0, "Buy milk");
        // The local item is now linked.
        assert!(items[0].sync.is_some());
        assert_eq!(summary.pushed, 1);
    }

    #[test]
    fn pushes_locally_completed_state() {
        // Remote has the task open; locally we completed it since last sync.
        let backend = FakeBackend::with_remote(vec![remote_task("g1", "Task", false, None)]);
        let mut item = TodoItem::new("Task");
        synced(&mut item, "g1"); // baseline: not done
        item.done = true; // local change
        let mut items = vec![item];

        sync_items(&backend, "@default", &mut items).unwrap();

        let patched = backend.patched.borrow();
        assert_eq!(patched.len(), 1);
        assert_eq!(patched[0].0, "g1");
        assert_eq!(patched[0].2, Some(true)); // pushed done=true
    }

    #[test]
    fn pulls_remote_completion_when_local_unchanged() {
        // Remote completed the task; locally it's unchanged from baseline.
        let backend = FakeBackend::with_remote(vec![remote_task("g1", "Task", true, None)]);
        let mut item = TodoItem::new("Task");
        synced(&mut item, "g1"); // baseline: not done
        let mut items = vec![item];

        sync_items(&backend, "@default", &mut items).unwrap();

        assert!(items[0].done); // pulled remote completion
        assert!(backend.patched.borrow().is_empty()); // nothing pushed
    }

    #[test]
    fn removes_items_deleted_on_remote() {
        let backend = FakeBackend::with_remote(vec![]); // remote no longer has it
        let mut item = TodoItem::new("Gone");
        synced(&mut item, "g1");
        let mut items = vec![item];

        let summary = sync_items(&backend, "@default", &mut items).unwrap();

        assert!(items.is_empty());
        assert_eq!(summary.removed, 1);
    }

    #[test]
    fn pulls_child_under_its_parent() {
        let backend = FakeBackend::with_remote(vec![
            remote_task("g1", "Parent", false, None),
            remote_task("g2", "Child", false, Some("g1")),
        ]);
        let mut items = Vec::new();
        sync_items(&backend, "@default", &mut items).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Parent");
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].title, "Child");
    }

    #[test]
    fn pushes_local_child_with_parent_link() {
        let backend = FakeBackend::with_remote(vec![]);
        let mut parent = TodoItem::new("Parent");
        parent.children.push(TodoItem::new("Child"));
        let mut items = vec![parent];

        sync_items(&backend, "@default", &mut items).unwrap();

        let inserted = backend.inserted.borrow();
        assert_eq!(inserted.len(), 2);
        // Parent inserted first with no parent link.
        assert_eq!(inserted[0].0, "Parent");
        assert_eq!(inserted[0].2, None);
        // Child inserted with the parent's freshly-minted google id.
        assert_eq!(inserted[1].0, "Child");
        assert!(inserted[1].2.is_some());
    }
}
