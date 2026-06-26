# Google Tasks Sync — Implementation Plan

Plan for syncing `jot-cli` with Google Tasks: pulling new tasks down from the
cloud and pushing local "cleared" (done) state back up.

Branch: `feature/try-google`

## Goal

Two-way sync between the local JSON store and Google Tasks:

- **Pull**: tasks created in Google (web/mobile) appear locally.
- **Push**: items toggled done locally (`x`, `lib.rs:868`) and items added
  locally are reflected in Google.
- Workspaces (`Workspace`, `lib.rs:33`) map to Google **task lists**.

## Where we are today

The local model has **no stable identity and no sync metadata**:

- `TodoItem` (`lib.rs:11`) = `{ title, done, children, folded }`. No id, no
  timestamp.
- Items are addressed by *position path* in the tree (`selected_path`,
  `remove_at_path` at `lib.rs:982`). Paths shift on any reorder/delete, so they
  cannot correlate a local item to a cloud task across two sync runs.
- Persistence is a single pretty-printed JSON file via serde
  (`Store::save`/`Store::load`, `lib.rs:69`). The app is fully synchronous with
  **zero network dependencies**.

These are the things the plan has to change.

## The two real frictions

1. **No stable identity.** Must add a Google task id and local sync state to
   `TodoItem` so reconciliation can match items across syncs.
2. **Nesting depth mismatch.** `children: Vec<TodoItem>` recurses to any depth;
   Google Tasks supports **exactly one level** of parent→child. We need an
   explicit collapse policy (see Phase 3).

Secondary: no local timestamps (needed for conflict resolution); local delete
hard-removes (`lib.rs:880`) while Google soft-marks `deleted`; OAuth + async
runtime are new infrastructure.

---

## Phase 0 — Decisions to lock first

- [ ] **Auth flow**: OAuth2 *loopback / localhost redirect* (recommended for a
      desktop CLI) vs. device-code flow. Loopback is the standard for installed
      apps and gives the smoothest UX.
- [ ] **Credential storage**: where the refresh token lives (e.g.
      `~/.config/jot-cli/google-token.json`, `0600`). Reuse `default_data_path`
      conventions (`lib.rs:133`).
- [ ] **Nesting policy** (Phase 3): flatten-deep vs. top-2-levels-only vs.
      encode-depth-in-notes.
- [ ] **Delete policy**: does a local delete delete upstream, or just unlink?
- [ ] **Conflict policy**: last-write-wins using Google's `updated` stamp vs.
      always-prefer-local for `done`.
- [ ] **Sync trigger**: on launch + on quit, an explicit `--sync` flag, and/or a
      key in the TUI. Recommend `--sync` first (no TUI churn).

## Phase 1 — Model changes (serde, backwards-compatible)

All new fields use `#[serde(default)]` so existing `state.json` files keep
loading (same pattern as the current `children`/`folded` fields).

- [ ] Add to `TodoItem` (`lib.rs:11`):
  - `google_id: Option<String>` — remote task id; `None` until first pushed.
  - `local_id: String` (default = fresh UUID) — stable local identity,
    independent of tree position.
  - `last_synced: Option<SyncSnapshot>` — `{ title, done }` captured at last
    sync, to detect local-vs-remote changes (three-way merge).
- [ ] Add to `Workspace` (`lib.rs:33`):
  - `google_tasklist_id: Option<String>`.
- [ ] Add a top-level `SyncState` (token path, last full-sync time) — either on
      `Store` (`lib.rs:49`) or a sibling file.
- [ ] New dep: `uuid` (with `v4`, `serde`).

## Phase 2 — Google API client

- [ ] New deps: `tokio` (rt-multi-thread), `reqwest` (json, rustls-tls),
      `serde` (already present). Keep the TUI synchronous; run sync in a small
      blocking `tokio` runtime invoked from `main` (`main.rs:26`).
- [ ] New module `src/google.rs`:
  - OAuth2: authorize, exchange code, refresh token, persist.
  - `tasklists.list` / `tasklists.insert`.
  - `tasks.list` (with `showCompleted`, `showHidden`), `tasks.insert`,
    `tasks.patch` (status), `tasks.delete`.
  - Thin typed structs for the Task resource: `id, title, status, parent,
    position, updated, deleted`.
- [ ] Register an OAuth client in Google Cloud console (Tasks API enabled);
      document the client-id/secret handling in README.

## Phase 3 — Nesting collapse (the design-heavy part)

Google has one level of nesting. Recommended default: **top-2-levels map
natively; deeper levels are flattened into the level-1 child's `notes`** as an
indented checklist, round-tripped on pull. Alternatives documented for the
decision in Phase 0:

- [ ] `flatten`: project the tree to depth 2, deeper items become siblings
      (lossy on structure, simplest).
- [ ] `top-2-only`: only sync depths 0–1; deeper items stay local-only.
- [ ] `notes-encoded`: encode depth ≥2 subtree in the parent task's `notes`
      (lossless, more code).

Implement as a pair of pure functions: `tree -> Vec<FlatTask>` and
`Vec<FlatTask> -> tree`, unit-tested independently of the network.

## Phase 4 — Reconciliation engine

A pure function over `(local Workspace, remote tasks, last_synced)` producing a
list of actions — no I/O, fully unit-testable (mirrors the existing test style,
`lib.rs:1238`+).

Matching: by `google_id` when present, else by `local_id` round-trip.

- [ ] **Pull-new**: remote task with id not present locally → insert into tree,
      stamp `google_id` + `last_synced`.
- [ ] **Push-new**: local item with `google_id == None` → `tasks.insert`, store
      returned id.
- [ ] **Push-cleared**: `done` differs from `last_synced.done` → `tasks.patch`
      status (`needsAction`/`completed`). *This is the headline feature and the
      simplest action.*
- [ ] **Title edits**: title differs from snapshot → patch / pull per conflict
      policy.
- [ ] **Deletes**: remote `deleted=true` → remove locally; local-missing →
      delete upstream per Phase 0 policy.
- [ ] Update each item's `last_synced` snapshot after a successful action.

## Phase 5 — Wiring & UX

- [ ] Add `--sync` to `parse_args` / `CliArgs` (`lib.rs:1058`, `lib.rs:1070`)
      and a usage line.
- [ ] In `main` (`main.rs:26`): `--sync` loads store → runs auth+reconcile →
      saves store, no TUI.
- [ ] Optional: a sync key in the TUI and a status-line indicator
      (`App.status`, `lib.rs:217`).
- [ ] `.gitignore` the token file; never commit secrets.

## Phase 6 — Testing

- [ ] Unit-test the tree↔flat conversions (Phase 3).
- [ ] Unit-test the reconciler with fixture inputs covering: pull-new,
      push-new, push-cleared, both-sides-changed conflict, delete each way.
- [ ] Manual end-to-end against a real Google account before merge.
- [ ] Verify old `state.json` (without new fields) still loads.

---

## Effort estimate

- One-way **push cleared state up** only: ~1 day once auth exists.
- Two-way, **flat lists**: a few days — bounded by OAuth plumbing + id/sync
  fields + the reconciler.
- Full two-way honoring **arbitrary depth**: the extra cost is entirely the
  Phase 3 nesting policy, not the API.

The API is not the blocker; the missing local identity/metadata is. Phases 1
and 4 are the backbone — land those and the rest is mechanical.

## Suggested commit slices

1. Model fields + UUID + serde-default migration (Phase 1).
2. `google.rs` client + OAuth, behind `--sync` doing nothing yet (Phase 2).
3. Tree↔flat conversion + tests (Phase 3).
4. Reconciler + tests (Phase 4).
5. Wire `--sync` end-to-end + docs (Phase 5–6).

> Per repo DevOps law: create/link a backlog ticket before cutting code, and put
> `tickets[<id>]` in each commit message.
