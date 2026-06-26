# Google Tasks sync — setup

Google Tasks integration is **opt-in at build time** and off by default.

```sh
cargo build --release --features google
```

A build without `--features google` has zero network dependencies and no sync
UI; `state.json` files stay compatible between the two builds.

## One-time Google Cloud setup

You need your own OAuth client (the app can't ship one):

1. Go to <https://console.cloud.google.com/> and create (or pick) a project.
2. **APIs & Services → Library →** enable the **Google Tasks API**.
3. **APIs & Services → OAuth consent screen:** configure it (External is fine),
   and add your Google account under **Test users**.
4. **APIs & Services → Credentials → Create credentials → OAuth client ID →
   Application type: Desktop app.** Copy the **client ID** and **client secret**.

## Credentials file

Save the credentials at `~/.config/jot-cli/google-credentials.json` (same dir as
`state.json`; honors `XDG_CONFIG_HOME`). Two formats are accepted:

- The **JSON you download** from the Credentials page as-is — it nests the
  secrets under an `installed` (Desktop app) or `web` key; extra fields are
  ignored:

  ```json
  { "installed": { "client_id": "…", "client_secret": "…", "...": "…" } }
  ```

- Or a **flat** object:

  ```json
  { "client_id": "xxxxxxxx.apps.googleusercontent.com", "client_secret": "yyyyyyyy" }
  ```

On first sync the app opens your browser for consent, catches the redirect on a
local loopback port, and caches the token in `google-token.json` next to the
credentials. Both files live outside the repo — keep them private.

## Using it

- A **"Google" workspace** is created automatically (bound to your **default**
  Google task list, `@default`).
- In the TUI: **`s`** syncs now. **`Shift+S`** toggles auto-sync (runs on launch
  and quit); enabling it asks for confirmation and is remembered across runs.
- Headless: `jot-cli --sync` reconciles and exits.

## Scope / limitations (first cut)

- **Two-way** sync: pulls new Google tasks down, pushes locally-added tasks up,
  and merges completion/title both directions (last-synced baseline; on a
  genuine conflict the local value wins).
- **One nesting level** — top-level items and their direct children map to
  Google parent/child tasks. Deeper descendants stay local-only and are left
  untouched (Google Tasks itself only supports one level).
- A task deleted on Google is removed locally on the next sync.
