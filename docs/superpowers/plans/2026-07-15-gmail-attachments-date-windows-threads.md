# Gmail Attachments Date Windows And Threads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the Gmail connector so users can configure a mailbox date window, fetch message attachments on demand when a specific email is hydrated, and opt into a Gmail thread-oriented filesystem view.

**Architecture:** Add a generic durable `MountConfig.settings_json` field, then keep Gmail-specific settings and parsing inside `locality-gmail`. Gmail message view remains the default for compatibility; date-window enumeration and thread view are opt-in Gmail mount settings. Attachments are not fetched during mailbox enumeration; full message hydration discovers attachment parts, downloads only that message's attachment bodies, and writes them under `.loc/gmail/attachments/`.

**Tech Stack:** Rust workspace crates (`locality-store`, `locality-gmail`, `localityd`, `loc-cli`), SQLite state migrations with `PRAGMA user_version`, reqwest blocking Gmail REST calls, serde JSON/YAML, existing Locality hydration and media asset plumbing.

---

## Product Decisions

- Default Gmail mounts keep the existing `inbox/*.md`, `sent/*.md`, and `draft/*.md` shape and still list the recent 100 messages per read-only mailbox.
- `loc mount gmail ... --after YYYY-MM-DD --before YYYY-MM-DD` stores a Gmail date window in mount settings. Both flags are required together. The connector converts them to Gmail search query dates as `after:YYYY/MM/DD before:YYYY/MM/DD`.
- When a date window is configured, Gmail enumeration pages through all matching messages for each mailbox using `maxResults=100` pages instead of stopping at the first recent 100 results.
- Attachment support is inbound read support only. Gmail draft-send attachments stay rejected until a later write-focused feature explicitly designs draft attachment upload semantics.
- A hydrated message renders attachment metadata and local attachment paths in frontmatter. Attachment bytes are written under `.loc/gmail/attachments/<message-id>/...` only when that message is hydrated by opening the online-only file or running `loc pull <message.md>`.
- Thread support is opt-in with `loc mount gmail ... --view threads`. In thread view, `inbox/` and `sent/` contain Gmail thread pages as `inbox/<date>-<subject>-<thread-id>/page.md`; each thread page can list child message files inside the same thread directory.
- Existing mounted message-view paths do not migrate automatically. Users who want thread paths create or update a Gmail mount with `--view threads`.

## External Gmail API Facts Used

- `users.messages.list` and `users.threads.list` accept `q`, `labelIds`, `maxResults`, and `pageToken` query parameters.
- Gmail search filtering through `q` supports Gmail's advanced search syntax, which includes date search operators.
- `users.messages.attachments.get` fetches an attachment body by message id and attachment id.
- `users.threads.get` can retrieve a thread with message data for a full conversation.

## File Structure

- Modify `crates/locality-store/src/records.rs`: add generic mount `settings_json` storage and builder helpers.
- Modify `crates/locality-store/src/sqlite.rs`: bump schema to v18, add `mounts.settings_json`, migrate v17 and older state, and persist/load settings.
- Modify `crates/locality-store/src/memory.rs`: preserve `settings_json` in the in-memory repository.
- Modify `crates/locality-store/tests/sqlite.rs` and `crates/locality-store/tests/repository.rs`: cover v17 migration and generic mount settings persistence.
- Create `crates/locality-gmail/src/settings.rs`: parse, validate, serialize, and convert Gmail mount settings to Gmail API query/view behavior.
- Modify `crates/locality-gmail/src/lib.rs`: export Gmail settings types.
- Modify `crates/loc-cli/src/mount.rs`: accept and report generic mount settings JSON.
- Modify `crates/loc-cli/src/commands.rs`: add Gmail `--after`, `--before`, and `--view` flags and serialize them into mount settings.
- Modify `crates/loc-cli/tests/mount.rs`: verify Gmail settings persist through CLI mount.
- Modify `crates/locality-gmail/src/dto.rs`: add attachment body DTOs and thread DTOs.
- Modify `crates/locality-gmail/src/client.rs`: add Gmail attachment and thread REST methods.
- Create `crates/locality-gmail/src/attachments.rs`: collect attachment specs from Gmail MIME parts, sanitize attachment paths, and decode attachment bodies.
- Modify `crates/locality-gmail/src/render.rs`: render attachment metadata and thread documents.
- Modify `crates/locality-gmail/src/connector.rs`: use settings for date windows, attachment metadata, and thread projection.
- Modify `crates/localityd/src/gmail.rs`: resolve Gmail mount settings and download attachment assets during message hydration.
- Modify `docs/gmail-connector.md`, `docs/cli.md`, and `docs/locality-store.md`: document the new user contract, CLI flags, and schema migration.

## Filesystem Contracts

Default message view stays:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-msg-1.md
  sent/
    1720900100000-reply-msg-2.md
  draft/
    reply.md
  .loc/
    gmail/
      attachments/
        msg-1/
          invoice-attach-1.pdf
```

Opt-in thread view is:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-thread-a/
      page.md
      1720900000000-quarterly-update-msg-1.md
      1720900500000-re-quarterly-update-msg-3.md
  sent/
    1720900100000-re-quarterly-update-thread-b/
      page.md
  draft/
```

Hydrated message frontmatter includes attachment metadata:

```yaml
gmail:
  mailbox: "inbox"
  message_id: "msg-1"
  thread_id: "thread-a"
  labels: ["INBOX"]
  attachments:
    - filename: "invoice.pdf"
      attachment_id: "attach-1"
      mime_type: "application/pdf"
      size: 12345
      path: ".loc/gmail/attachments/msg-1/invoice-attach-1.pdf"
```

## Task 1: Add Durable Mount Settings JSON

**Files:**
- Modify: `crates/locality-store/src/records.rs`
- Modify: `crates/locality-store/src/sqlite.rs`
- Modify: `crates/locality-store/src/memory.rs`
- Test: `crates/locality-store/tests/repository.rs`
- Test: `crates/locality-store/tests/sqlite.rs`
- Modify: `docs/locality-store.md`

- [ ] **Step 1: Write the failing repository persistence test**

Add this test to `crates/locality-store/tests/repository.rs` near the existing mount repository tests:

```rust
#[test]
fn repository_persists_mount_settings_json() {
    fn exercise<S>(store: &mut S)
    where
        S: MountRepository,
    {
        let mount_id = MountId::new("gmail-main");
        let mount = MountConfig::new(mount_id.clone(), "gmail", "/tmp/Locality/gmail-main")
            .with_settings_json(
                r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#,
            );

        store.save_mount(mount).expect("save mount");

        let loaded = store
            .get_mount(&mount_id)
            .expect("load mount")
            .expect("mount exists");
        assert_eq!(
            loaded.settings_json,
            r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#
        );
    }

    let mut memory = InMemoryStateStore::new();
    exercise(&mut memory);

    let fixture = SqliteFixture::new();
    let mut sqlite = fixture.open();
    exercise(&mut sqlite);
}
```

- [ ] **Step 2: Run the repository test and verify it fails**

Run:

```bash
cargo test -p locality-store repository_persists_mount_settings_json
```

Expected: FAIL with a compiler error that `MountConfig` has no field or method named `settings_json` / `with_settings_json`.

- [ ] **Step 3: Add settings JSON to the mount record**

In `crates/locality-store/src/records.rs`, replace `MountConfig` with:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountConfig {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub connection_id: Option<ConnectionId>,
    pub read_only: bool,
    pub projection: ProjectionMode,
    pub settings_json: String,
}

impl MountConfig {
    pub fn new(mount_id: MountId, connector: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            mount_id,
            connector: connector.into(),
            root: root.into(),
            remote_root_id: None,
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        }
    }

    pub fn with_remote_root_id(mut self, remote_root_id: RemoteId) -> Self {
        self.remote_root_id = Some(remote_root_id);
        self
    }

    pub fn with_connection_id(mut self, connection_id: ConnectionId) -> Self {
        self.connection_id = Some(connection_id);
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn projection(mut self, projection: ProjectionMode) -> Self {
        self.projection = projection;
        self
    }

    pub fn with_settings_json(mut self, settings_json: impl Into<String>) -> Self {
        self.settings_json = settings_json.into();
        self
    }
}
```

- [ ] **Step 4: Persist settings JSON in SQLite**

In `crates/locality-store/src/sqlite.rs`:

Change:

```rust
const SCHEMA_VERSION: i64 = 17;
```

to:

```rust
const SCHEMA_VERSION: i64 = 18;
```

Update every `CREATE TABLE IF NOT EXISTS mounts` definition to include:

```sql
settings_json TEXT NOT NULL DEFAULT '{}'
```

Use this column order everywhere mounts are selected or inserted:

```sql
mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
```

Update `save_mount` to insert and update `settings_json`:

```rust
"INSERT INTO mounts (mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
 ON CONFLICT(mount_id) DO UPDATE SET
    connector = excluded.connector,
    root = excluded.root,
    remote_root_id = excluded.remote_root_id,
    read_only = excluded.read_only,
    projection_json = excluded.projection_json,
    connection_id = excluded.connection_id,
    settings_json = excluded.settings_json",
```

and bind:

```rust
mount.settings_json.as_str(),
```

Add this migration block after the v17 migration block and before the final `PRAGMA user_version` update:

```rust
if user_version < 18 && !column_exists(connection, "mounts", "settings_json")? {
    connection.execute_batch(
        "ALTER TABLE mounts
         ADD COLUMN settings_json TEXT NOT NULL DEFAULT '{}';",
    )?;
    if user_version >= 13 {
        record_schema_migration(connection, user_version, SCHEMA_VERSION)?;
    }
}
```

Update the mount row alias and mapper to read the eighth column:

```rust
type MountRow = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    Option<String>,
    String,
);

fn mount_from_row(row: MountRow) -> StoreResult<MountConfig> {
    Ok(MountConfig {
        mount_id: MountId(row.0),
        connector: row.1,
        root: PathBuf::from(row.2),
        remote_root_id: row.3.map(RemoteId),
        read_only: row.4 != 0,
        projection: from_json(&row.5)?,
        connection_id: row.6.map(ConnectionId),
        settings_json: row.7,
    })
}
```

- [ ] **Step 5: Update in-memory mount storage call sites**

In `crates/locality-store/src/memory.rs`, run:

```bash
rg -n "MountConfig \\{" crates/locality-store/src/memory.rs
```

For each `MountConfig` struct literal in that output, add:

```rust
settings_json: "{}".to_string(),
```

Leave `save_mount` as whole-record storage. It must continue storing the complete `MountConfig` value in the in-memory mount map.

- [ ] **Step 6: Add a v17 migration test**

Add this test to `crates/locality-store/tests/sqlite.rs` near the schema migration tests:

```rust
#[test]
fn sqlite_store_migrates_v17_mounts_with_default_settings_json() {
    let fixture = SqliteFixture::new();
    fs::create_dir_all(&fixture.state_root).expect("create state root");
    let db_path = fixture.state_root.join("state.sqlite3");
    let connection = Connection::open(&db_path).expect("raw connection");
    connection
        .execute_batch(
            r#"
            PRAGMA user_version = 17;
            CREATE TABLE mounts (
                mount_id TEXT PRIMARY KEY,
                connector TEXT NOT NULL,
                root TEXT NOT NULL,
                remote_root_id TEXT,
                read_only INTEGER NOT NULL CHECK (read_only IN (0, 1)),
                projection_json TEXT NOT NULL DEFAULT '"plain_files"',
                connection_id TEXT
            );
            CREATE TABLE state_components (
                component_id TEXT PRIMARY KEY,
                component_kind TEXT NOT NULL,
                version INTEGER NOT NULL,
                min_reader_version INTEGER NOT NULL DEFAULT 1,
                required INTEGER NOT NULL CHECK (required IN (0, 1)),
                rebuildable INTEGER NOT NULL CHECK (rebuildable IN (0, 1)),
                data_json TEXT NOT NULL DEFAULT '{}',
                updated_at TEXT NOT NULL
            );
            INSERT INTO state_components (
                component_id, component_kind, version, min_reader_version,
                required, rebuildable, data_json, updated_at
            )
            VALUES ('core:schema', 'schema', 17, 1, 1, 0, '{}', '2026-07-15T00:00:00Z');
            INSERT INTO mounts (
                mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id
            )
            VALUES (
                'gmail-main', 'gmail', '/tmp/Locality/gmail-main', NULL, 0, '"plain_files"', NULL
            );
            "#,
        )
        .expect("seed v17 state");
    drop(connection);

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("migrate v17");
    let connection = Connection::open(&store.db_path).expect("raw migrated connection");
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user version");
    let settings_json: String = connection
        .query_row(
            "SELECT settings_json FROM mounts WHERE mount_id = 'gmail-main'",
            [],
            |row| row.get(0),
        )
        .expect("settings json");

    assert_eq!(user_version, 18);
    assert_eq!(settings_json, "{}");
}
```

- [ ] **Step 7: Update schema expectations and docs**

In `crates/locality-store/tests/sqlite.rs`, update expected schema version values from `17` to `18` where they assert the current schema after a successful migration.

In the expected schema column list test, change the mounts line to:

```text
mounts: mount_id, connector, root, remote_root_id, read_only, projection_json, connection_id, settings_json
```

In `docs/locality-store.md`, add:

```markdown
- SQLite migrates v17 rows to v18 by adding `mounts.settings_json`, a generic
  mount-scoped JSON settings field used by connector-specific mount options.
```

and update the current migration summary to mention schema version 18 where the document names the current version.

- [ ] **Step 8: Run focused store tests**

Run:

```bash
cargo test -p locality-store repository_persists_mount_settings_json
cargo test -p locality-store sqlite_store_migrates_v17_mounts_with_default_settings_json
cargo test -p locality-store sqlite_store_initializes_idempotently
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/locality-store/src/records.rs crates/locality-store/src/sqlite.rs crates/locality-store/src/memory.rs crates/locality-store/tests/repository.rs crates/locality-store/tests/sqlite.rs docs/locality-store.md
git commit -m "feat: persist mount settings json"
```

## Task 2: Add Gmail Mount Settings Types

**Files:**
- Create: `crates/locality-gmail/src/settings.rs`
- Modify: `crates/locality-gmail/src/lib.rs`
- Test: `crates/locality-gmail/src/settings.rs`

- [ ] **Step 1: Write the Gmail settings module with tests**

Create `crates/locality-gmail/src/settings.rs`:

```rust
use std::fmt;

use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GmailMountSettings {
    pub gmail: GmailSettings,
}

impl Default for GmailMountSettings {
    fn default() -> Self {
        Self {
            gmail: GmailSettings::default(),
        }
    }
}

impl GmailMountSettings {
    pub fn from_json(value: &str) -> LocalityResult<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str::<Self>(value).map_err(|error| {
            LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
                "gmail_mount_settings_invalid",
                std::path::PathBuf::new(),
                Some(1),
                format!("Gmail mount settings JSON is invalid: {error}"),
                Some("remount Gmail with valid --after/--before/--view options".to_string()),
            )])
        })
    }

    pub fn to_json(&self) -> LocalityResult<String> {
        serde_json::to_string(self)
            .map_err(|error| LocalityError::Io(format!("gmail settings encode failed: {error}")))
    }

    pub fn with_date_window(after: &str, before: &str) -> LocalityResult<Self> {
        Ok(Self {
            gmail: GmailSettings {
                date_window: Some(GmailDateWindow::new(after, before)?),
                view: GmailProjectionView::Messages,
            },
        })
    }

    pub fn with_view(mut self, view: GmailProjectionView) -> Self {
        self.gmail.view = view;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GmailSettings {
    pub date_window: Option<GmailDateWindow>,
    pub view: GmailProjectionView,
}

impl Default for GmailSettings {
    fn default() -> Self {
        Self {
            date_window: None,
            view: GmailProjectionView::Messages,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GmailProjectionView {
    Messages,
    Threads,
}

impl Default for GmailProjectionView {
    fn default() -> Self {
        Self::Messages
    }
}

impl GmailProjectionView {
    pub fn parse(value: &str) -> LocalityResult<Self> {
        match value {
            "messages" => Ok(Self::Messages),
            "threads" => Ok(Self::Threads),
            other => Err(settings_validation(
                "gmail_mount_view_invalid",
                format!("unsupported Gmail view `{other}`"),
                "use `--view messages` or `--view threads`",
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Threads => "threads",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDateWindow {
    pub after: GmailSearchDate,
    pub before: GmailSearchDate,
}

impl GmailDateWindow {
    pub fn new(after: &str, before: &str) -> LocalityResult<Self> {
        let after = GmailSearchDate::parse(after)?;
        let before = GmailSearchDate::parse(before)?;
        if before <= after {
            return Err(settings_validation(
                "gmail_mount_date_window_order",
                "`--before` must be later than `--after`",
                "choose a before date after the after date",
            ));
        }
        Ok(Self { after, before })
    }

    pub fn query(&self) -> String {
        format!(
            "after:{} before:{}",
            self.after.gmail_query_date(),
            self.before.gmail_query_date()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GmailSearchDate(String);

impl GmailSearchDate {
    pub fn parse(value: &str) -> LocalityResult<Self> {
        let bytes = value.as_bytes();
        let valid = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
        if !valid {
            return Err(settings_validation(
                "gmail_mount_date_invalid",
                format!("Gmail date `{value}` must use YYYY-MM-DD"),
                "use a date such as 2026-07-15",
            ));
        }
        let year = value[0..4].parse::<u32>().unwrap_or(0);
        let month = value[5..7].parse::<u32>().unwrap_or(0);
        let day = value[8..10].parse::<u32>().unwrap_or(0);
        if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
            return Err(settings_validation(
                "gmail_mount_date_invalid",
                format!("Gmail date `{value}` is not a calendar date"),
                "use a valid calendar date",
            ));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn gmail_query_date(&self) -> String {
        self.0.replace('-', "/")
    }
}

impl fmt::Display for GmailSearchDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn leap_year(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn settings_validation(code: &'static str, message: impl Into<String>, suggestion: &'static str) -> LocalityError {
    LocalityError::Validation(vec![locality_core::validation::ValidationIssue::new(
        code,
        std::path::PathBuf::new(),
        Some(1),
        message,
        Some(suggestion.to_string()),
    )])
}

#[cfg(test)]
mod tests {
    use super::{GmailMountSettings, GmailProjectionView, GmailSearchDate};

    #[test]
    fn default_settings_keep_message_view_without_date_window() {
        let settings = GmailMountSettings::from_json("{}").expect("settings");

        assert_eq!(settings.gmail.date_window, None);
        assert_eq!(settings.gmail.view, GmailProjectionView::Messages);
    }

    #[test]
    fn settings_serialize_date_window_and_thread_view() {
        let settings = GmailMountSettings::with_date_window("2026-07-01", "2026-07-15")
            .expect("date window")
            .with_view(GmailProjectionView::Threads);

        let json = settings.to_json().expect("json");
        let parsed = GmailMountSettings::from_json(&json).expect("parsed json");

        assert_eq!(parsed.gmail.view, GmailProjectionView::Threads);
        assert_eq!(
            parsed
                .gmail
                .date_window
                .as_ref()
                .expect("window")
                .query(),
            "after:2026/07/01 before:2026/07/15"
        );
    }

    #[test]
    fn date_window_rejects_invalid_or_reversed_dates() {
        assert!(GmailSearchDate::parse("2026-02-29").is_err());
        assert!(GmailSearchDate::parse("2024-02-29").is_ok());
        assert!(GmailMountSettings::with_date_window("2026-07-15", "2026-07-01").is_err());
    }

    #[test]
    fn view_parser_accepts_only_known_views() {
        assert_eq!(
            GmailProjectionView::parse("messages").expect("messages"),
            GmailProjectionView::Messages
        );
        assert_eq!(
            GmailProjectionView::parse("threads").expect("threads"),
            GmailProjectionView::Threads
        );
        assert!(GmailProjectionView::parse("conversation").is_err());
    }
}
```

- [ ] **Step 2: Export settings**

In `crates/locality-gmail/src/lib.rs`, add:

```rust
pub mod settings;
```

and export:

```rust
pub use settings::{
    GmailDateWindow, GmailMountSettings, GmailProjectionView, GmailSearchDate, GmailSettings,
};
```

- [ ] **Step 3: Run Gmail settings tests**

Run:

```bash
cargo test -p locality-gmail settings
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/locality-gmail/src/settings.rs crates/locality-gmail/src/lib.rs
git commit -m "feat: add gmail mount settings"
```

## Task 3: Wire Gmail Settings Through CLI Mount

**Files:**
- Modify: `crates/loc-cli/src/mount.rs`
- Modify: `crates/loc-cli/src/commands.rs`
- Test: `crates/loc-cli/tests/mount.rs`

- [ ] **Step 1: Write failing CLI tests**

Add this test to `crates/loc-cli/tests/mount.rs` after `cli_mount_gmail_persists_requested_registration`:

```rust
#[test]
fn cli_mount_gmail_persists_date_window_and_thread_view() {
    let fixture = MountFixture::new("loc-cli-gmail-mount-settings");
    fs::create_dir_all(&fixture.root).expect("create fixture root");
    let state_root = fixture.root.join("state");
    seed_cli_gmail_connection(&state_root, "gmail-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let mount_root = fixture.root.join("gmail");
    let mount_root_arg = mount_root.display().to_string();

    let report = loc_json_ok(loc_command(loc, &state_root).args([
        "mount",
        "gmail",
        mount_root_arg.as_str(),
        "--connection",
        "gmail-work",
        "--mount-id",
        "gmail-main",
        "--projection",
        "plain-files",
        "--after",
        "2026-07-01",
        "--before",
        "2026-07-15",
        "--view",
        "threads",
        "--json",
    ]));

    assert_eq!(report["connector"], "gmail", "{report:#?}");
    assert_eq!(
        report["settings_json"],
        r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#,
        "{report:#?}"
    );

    let store = SqliteStateStore::open(state_root).expect("open state");
    let mount = store
        .get_mount(&MountId::new("gmail-main"))
        .expect("load mount")
        .expect("mount exists");
    assert_eq!(
        mount.settings_json,
        r#"{"gmail":{"date_window":{"after":"2026-07-01","before":"2026-07-15"},"view":"threads"}}"#
    );
}
```

Add this validation test:

```rust
#[test]
fn cli_mount_gmail_rejects_partial_date_window() {
    for args in [
        vec!["--after", "2026-07-01"],
        vec!["--before", "2026-07-15"],
    ] {
        let fixture = MountFixture::new("loc-cli-gmail-partial-date-window");
        fs::create_dir_all(&fixture.root).expect("create fixture root");
        let state_root = fixture.root.join("state");
        seed_cli_gmail_connection(&state_root, "gmail-work");
        let loc = env!("CARGO_BIN_EXE_loc");
        let mount_root = fixture.root.join("gmail");
        let mount_root_arg = mount_root.display().to_string();
        let mut command = loc_command(loc, &state_root);
        command.args([
            "mount",
            "gmail",
            mount_root_arg.as_str(),
            "--connection",
            "gmail-work",
            "--projection",
            "plain-files",
            "--json",
        ]);
        command.args(args);

        let output = command.output().expect("run loc mount gmail");
        assert!(!output.status.success());
        let body: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("json error response");
        assert_eq!(body["code"], "gmail_date_window_requires_after_and_before");
    }
}
```

- [ ] **Step 2: Run the CLI tests and verify they fail**

Run:

```bash
cargo test -p loc-cli cli_mount_gmail_persists_date_window_and_thread_view
cargo test -p loc-cli cli_mount_gmail_rejects_partial_date_window
```

Expected: FAIL because `settings_json` is not reported and the Gmail flags are not parsed.

- [ ] **Step 3: Add settings to mount options and reports**

In `crates/loc-cli/src/mount.rs`, add `settings_json` to `MountOptions`:

```rust
pub struct MountOptions {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub connection_id: Option<ConnectionId>,
    pub read_only: bool,
    pub projection: ProjectionMode,
    pub settings_json: String,
}
```

Add `settings_json` to `MountReport`:

```rust
pub struct MountReport {
    pub ok: bool,
    pub command: &'static str,
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub remote_root_id: Option<String>,
    pub connection_id: Option<String>,
    pub read_only: bool,
    pub projection: String,
    pub settings_json: String,
    pub guidance: MountGuidanceReport,
}
```

When building the mount in `run_mount`, set:

```rust
let mut mount = MountConfig::new(options.mount_id.clone(), options.connector.clone(), &root)
    .read_only(options.read_only)
    .projection(options.projection.clone())
    .with_settings_json(options.settings_json.clone());
```

When returning `MountReport`, set:

```rust
settings_json: options.settings_json,
```

Update existing `MountOptions` literals in tests to include:

```rust
settings_json: "{}".to_string(),
```

- [ ] **Step 4: Parse Gmail-specific mount flags**

In `crates/loc-cli/src/commands.rs`, add these imports near the existing Gmail imports:

```rust
use locality_gmail::{GmailMountSettings, GmailProjectionView};
```

In `MountGmailArgs`, add:

```rust
#[arg(long, value_name = "YYYY-MM-DD", help = "Fetch Gmail messages on or after this date. Must be paired with --before.")]
after: Option<String>,
#[arg(long, value_name = "YYYY-MM-DD", help = "Fetch Gmail messages before this date. Must be paired with --after.")]
before: Option<String>,
#[arg(long, value_name = "messages|threads", help = "Gmail projection view. Defaults to messages.")]
view: Option<String>,
```

In the CLI argument reconstruction block for `MountCommand::Gmail`, add:

```rust
push_optional_flag_value(&mut args, "--after", options.after.as_deref());
push_optional_flag_value(&mut args, "--before", options.before.as_deref());
push_optional_flag_value(&mut args, "--view", options.view.as_deref());
```

Add this helper near the other mount helpers:

```rust
fn gmail_mount_settings_json(args: &[String]) -> Result<String, CommandError> {
    let after = flag_value(args, "--after");
    let before = flag_value(args, "--before");
    let view = flag_value(args, "--view")
        .map(GmailProjectionView::parse)
        .transpose()
        .map_err(|error| {
            CommandError::new("mount", "gmail_view_invalid", error.to_string())
        })?
        .unwrap_or(GmailProjectionView::Messages);

    let settings = match (after, before) {
        (None, None) => GmailMountSettings::default().with_view(view),
        (Some(after), Some(before)) => GmailMountSettings::with_date_window(after, before)
            .map_err(|error| {
                CommandError::new("mount", "gmail_date_window_invalid", error.to_string())
            })?
            .with_view(view),
        _ => {
            return Err(CommandError::new(
                "mount",
                "gmail_date_window_requires_after_and_before",
                "Gmail date windows require both --after and --before",
            ));
        }
    };

    settings.to_json().map_err(|error| {
        CommandError::new("mount", "gmail_settings_encode_failed", error.to_string())
    })
}
```

In `mount(args, json)`, before constructing `MountOptions`, add:

```rust
let settings_json = if descriptor.id() == GMAIL_CONNECTOR_ID {
    match gmail_mount_settings_json(args) {
        Ok(settings_json) => settings_json,
        Err(error) => return command_error(json, error, EXIT_USAGE),
    }
} else {
    "{}".to_string()
};
```

Set `settings_json` in `MountOptions`:

```rust
settings_json,
```

- [ ] **Step 5: Update human mount output**

In `print_mount_report`, add:

```rust
if report.settings_json != "{}" {
    println!("settings: {}", report.settings_json);
}
```

- [ ] **Step 6: Run focused CLI tests**

Run:

```bash
cargo test -p loc-cli cli_mount_gmail_persists_date_window_and_thread_view
cargo test -p loc-cli cli_mount_gmail_rejects_partial_date_window
cargo test -p loc-cli cli_mount_gmail_persists_requested_registration
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/loc-cli/src/mount.rs crates/loc-cli/src/commands.rs crates/loc-cli/tests/mount.rs
git commit -m "feat: wire gmail mount settings through cli"
```

## Task 4: Use Gmail Date Windows During Message Enumeration

**Files:**
- Modify: `crates/locality-gmail/src/connector.rs`
- Modify: `crates/localityd/src/gmail.rs`
- Test: `crates/locality-gmail/src/connector.rs`

- [ ] **Step 1: Write failing connector date-window tests**

Add these tests to `crates/locality-gmail/src/connector.rs` `mod tests`:

```rust
#[test]
fn enumerate_with_date_window_pages_all_matching_messages_with_gmail_query() {
    let api = Arc::new(FakeGmailApi::default());
    {
        let mut calls = api.calls.lock().expect("calls");
        calls.paged_message_ids.insert(
            ("INBOX".to_string(), None),
            GmailMessageList {
                messages: vec![GmailMessageRef {
                    id: "inbox-msg-1".to_string(),
                    thread_id: Some("thread-1".to_string()),
                }],
                next_page_token: Some("next-inbox".to_string()),
                result_size_estimate: Some(2),
            },
        );
        calls.paged_message_ids.insert(
            ("INBOX".to_string(), Some("next-inbox".to_string())),
            GmailMessageList {
                messages: vec![GmailMessageRef {
                    id: "inbox-msg-2".to_string(),
                    thread_id: Some("thread-2".to_string()),
                }],
                next_page_token: None,
                result_size_estimate: Some(2),
            },
        );
    }
    let settings = crate::settings::GmailMountSettings::with_date_window(
        "2026-07-01",
        "2026-07-15",
    )
    .expect("date window");
    let connector = GmailConnector::with_api(
        GmailConfig::new("token").with_settings(settings),
        api.clone(),
    );

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("gmail-main"),
            cursor: None,
        })
        .expect("enumerate");

    assert!(entries.iter().any(|entry| entry.remote_id == RemoteId::new("inbox-msg-1")));
    assert!(entries.iter().any(|entry| entry.remote_id == RemoteId::new("inbox-msg-2")));
    let calls = api.calls.lock().expect("calls");
    assert_eq!(
        calls.list_queries,
        vec![
            "after:2026/07/01 before:2026/07/15".to_string(),
            "after:2026/07/01 before:2026/07/15".to_string(),
            "after:2026/07/01 before:2026/07/15".to_string(),
        ]
    );
    assert_eq!(
        calls.list_page_tokens,
        vec![None, Some("next-inbox".to_string()), None]
    );
}

#[test]
fn enumerate_without_date_window_keeps_recent_100_single_page_behavior() {
    let api = Arc::new(FakeGmailApi::default());
    let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

    connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("gmail-main"),
            cursor: None,
        })
        .expect("enumerate");

    let calls = api.calls.lock().expect("calls");
    assert_eq!(calls.list_max_results, vec![100, 100]);
    assert_eq!(calls.list_page_tokens, vec![None, None]);
    assert!(calls.list_queries.is_empty());
}
```

Extend `FakeCalls` in the same test module:

```rust
paged_message_ids: std::collections::BTreeMap<(String, Option<String>), GmailMessageList>,
list_page_tokens: Vec<Option<String>>,
```

In `FakeGmailApi::list_messages`, record the page token:

```rust
calls.list_page_tokens.push(_page_token.map(str::to_string));
```

and return a configured page before the default response:

```rust
if let Some(page) = calls
    .paged_message_ids
    .get(&(label_id.to_string(), _page_token.map(str::to_string)))
    .cloned()
{
    return Ok(page);
}
```

- [ ] **Step 2: Run date-window tests and verify they fail**

Run:

```bash
cargo test -p locality-gmail enumerate_with_date_window_pages_all_matching_messages_with_gmail_query
cargo test -p locality-gmail enumerate_without_date_window_keeps_recent_100_single_page_behavior
```

Expected: FAIL because `GmailConfig::with_settings` does not exist and enumeration ignores settings.

- [ ] **Step 3: Store settings in GmailConfig**

In `crates/locality-gmail/src/connector.rs`, add:

```rust
use crate::settings::{GmailMountSettings, GmailProjectionView};
```

Change `GmailConfig` to:

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
    pub settings: GmailMountSettings,
}

impl GmailConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            settings: GmailMountSettings::default(),
        }
    }

    pub fn with_settings(mut self, settings: GmailMountSettings) -> Self {
        self.settings = settings;
        self
    }
}
```

Keep the existing redacted `Debug` implementation and add this field:

```rust
.field("settings", &self.settings)
```

- [ ] **Step 4: Resolve Gmail settings from mounts**

In `crates/localityd/src/gmail.rs`, add `GmailMountSettings` to the `locality_gmail` imports.

In `connector_from_connection`, replace:

```rust
Ok(GmailConnector::new(GmailConfig::new(token)))
```

with a new helper call:

```rust
Ok(GmailConnector::new(gmail_config_from_mount(token, mount)?))
```

Pass `mount` into `connector_from_connection` by changing calls from:

```rust
return connector_from_connection(credentials, &connection);
```

to:

```rust
return connector_from_connection(credentials, &connection, mount);
```

and changing the function signature:

```rust
fn connector_from_connection(
    credentials: &dyn CredentialStore,
    connection: &ConnectionRecord,
    mount: &MountConfig,
) -> Result<GmailConnector, ConnectorResolveError> {
```

Add:

```rust
fn gmail_config_from_mount(
    token: String,
    mount: &MountConfig,
) -> Result<GmailConfig, ConnectorResolveError> {
    let settings = GmailMountSettings::from_json(&mount.settings_json).map_err(|error| {
        ConnectorResolveError::CredentialStoreUnavailable(format!(
            "Gmail mount `{}` settings are invalid: {error}",
            mount.mount_id.0
        ))
    })?;
    Ok(GmailConfig::new(token).with_settings(settings))
}
```

- [ ] **Step 5: Page date-window message enumeration**

In `crates/locality-gmail/src/connector.rs`, replace:

```rust
const RECENT_LIMIT: u32 = 100;
```

with:

```rust
const GMAIL_PAGE_SIZE: u32 = 100;
```

Update `enumerate` and `list_children` so `list_label_entries` receives `&self.config.settings`:

```rust
entries.extend(list_label_entries(
    self.api.as_ref(),
    &self.config.settings,
    &request.mount_id,
    "INBOX",
    "inbox",
    Path::new("inbox"),
)?);
```

Replace `list_label_entries` with:

```rust
fn list_label_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    label_id: &str,
    mailbox: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let messages = list_message_refs(api, settings, label_id)?;
    messages
        .into_iter()
        .map(|message_ref| {
            let message = api.get_message_metadata(&message_ref.id)?;
            Ok(message_entry(mount_id, parent_path, mailbox, message))
        })
        .collect()
}

fn list_message_refs(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    label_id: &str,
) -> LocalityResult<Vec<crate::dto::GmailMessageRef>> {
    let Some(query) = settings.gmail.date_window.as_ref().map(|window| window.query()) else {
        return Ok(api
            .list_messages(label_id, GMAIL_PAGE_SIZE, None, None)?
            .messages);
    };

    let mut page_token = None;
    let mut messages = Vec::new();
    loop {
        let page = api.list_messages(
            label_id,
            GMAIL_PAGE_SIZE,
            page_token.as_deref(),
            Some(&query),
        )?;
        messages.extend(page.messages);
        let Some(next) = page.next_page_token else {
            break;
        };
        page_token = Some(next);
    }
    Ok(messages)
}
```

- [ ] **Step 6: Guard thread view until Task 8**

At the top of `enumerate`, before message listing, add:

```rust
if self.config.settings.gmail.view == GmailProjectionView::Threads {
    return enumerate_threads_not_enabled_yet();
}
```

Add this guard function in the same file:

```rust
fn enumerate_threads_not_enabled_yet() -> LocalityResult<Vec<TreeEntry>> {
    Err(LocalityError::Unsupported(
        "gmail thread view requires the thread projection implementation in Task 8".to_string(),
    ))
}
```

Task 8 removes this guard after thread projection is implemented.

- [ ] **Step 7: Run focused Gmail date-window tests**

Run:

```bash
cargo test -p locality-gmail enumerate_with_date_window_pages_all_matching_messages_with_gmail_query
cargo test -p locality-gmail enumerate_without_date_window_keeps_recent_100_single_page_behavior
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/locality-gmail/src/connector.rs crates/localityd/src/gmail.rs
git commit -m "feat: enumerate gmail date windows"
```

## Task 5: Add Gmail Attachment API And Metadata Helpers

**Files:**
- Modify: `crates/locality-gmail/src/dto.rs`
- Modify: `crates/locality-gmail/src/client.rs`
- Create: `crates/locality-gmail/src/attachments.rs`
- Modify: `crates/locality-gmail/src/lib.rs`
- Test: `crates/locality-gmail/src/client.rs`
- Test: `crates/locality-gmail/src/attachments.rs`

- [ ] **Step 1: Add failing attachment helper tests**

Create `crates/locality-gmail/src/attachments.rs` with the tests first:

```rust
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::dto::{GmailMessage, GmailMessagePart, GmailMessagePartBody};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailAttachmentSpec {
    pub message_id: String,
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<u64>,
    pub local_path: PathBuf,
}

pub fn collect_attachment_specs(message: &GmailMessage) -> Vec<GmailAttachmentSpec> {
    let mut specs = Vec::new();
    if let Some(payload) = &message.payload {
        collect_part_specs(&message.id, payload, &mut specs);
    }
    specs
}

fn collect_part_specs(message_id: &str, part: &GmailMessagePart, specs: &mut Vec<GmailAttachmentSpec>) {
    if let Some(filename) = part.filename.as_deref().filter(|value| !value.trim().is_empty()) {
        if let Some(body) = &part.body {
            if let Some(attachment_id) = body.attachment_id.as_deref() {
                specs.push(GmailAttachmentSpec {
                    message_id: message_id.to_string(),
                    attachment_id: attachment_id.to_string(),
                    filename: filename.to_string(),
                    mime_type: part.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
                    size: body.size,
                    local_path: attachment_local_path(message_id, attachment_id, filename),
                });
            }
        }
    }
    for child in &part.parts {
        collect_part_specs(message_id, child, specs);
    }
}

pub fn attachment_local_path(message_id: &str, attachment_id: &str, filename: &str) -> PathBuf {
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", safe_component(value)))
        .unwrap_or_default();
    PathBuf::from(".loc")
        .join("gmail")
        .join("attachments")
        .join(safe_component(message_id))
        .join(format!(
            "{}-{}{}",
            safe_stem(filename),
            safe_component(attachment_id),
            extension
        ))
}

pub fn decode_attachment_body(body: &GmailMessagePartBody) -> LocalityResult<Vec<u8>> {
    let data = body.data.as_deref().ok_or_else(|| {
        LocalityError::Io("gmail attachment response did not include body data".to_string())
    })?;
    URL_SAFE_NO_PAD
        .decode(data.as_bytes())
        .or_else(|_| URL_SAFE.decode(data.as_bytes()))
        .map_err(|error| LocalityError::Io(format!("gmail attachment decode failed: {error}")))
}

fn safe_stem(filename: &str) -> String {
    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("attachment");
    let safe = safe_component(stem);
    if safe.is_empty() {
        "attachment".to_string()
    } else {
        safe
    }
}

fn safe_component(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::{attachment_local_path, collect_attachment_specs, decode_attachment_body};
    use crate::dto::GmailMessage;

    #[test]
    fn collects_nested_attachment_specs_with_safe_local_paths() {
        let message: GmailMessage = serde_json::from_value(serde_json::json!({
            "id": "msg/1",
            "payload": {
                "mimeType": "multipart/mixed",
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "filename": "Invoice July.pdf",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "attach/1", "size": 12345 }
                    }
                ]
            }
        }))
        .expect("message");

        let specs = collect_attachment_specs(&message);

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].message_id, "msg/1");
        assert_eq!(specs[0].attachment_id, "attach/1");
        assert_eq!(specs[0].filename, "Invoice July.pdf");
        assert_eq!(specs[0].mime_type, "application/pdf");
        assert_eq!(specs[0].size, Some(12345));
        assert_eq!(
            specs[0].local_path,
            std::path::PathBuf::from(".loc/gmail/attachments/msg-1/invoice-july-attach-1.pdf")
        );
    }

    #[test]
    fn attachment_local_path_keeps_distinct_attachment_ids() {
        assert_eq!(
            attachment_local_path("msg-1", "a-1", "report.final.pdf"),
            std::path::PathBuf::from(".loc/gmail/attachments/msg-1/report-final-a-1.pdf")
        );
    }

    #[test]
    fn decodes_padded_and_unpadded_gmail_attachment_data() {
        let padded = crate::dto::GmailMessagePartBody {
            data: Some("SGVsbG8=".to_string()),
            ..crate::dto::GmailMessagePartBody::default()
        };
        let unpadded = crate::dto::GmailMessagePartBody {
            data: Some("SGVsbG8".to_string()),
            ..crate::dto::GmailMessagePartBody::default()
        };

        assert_eq!(decode_attachment_body(&padded).expect("padded"), b"Hello");
        assert_eq!(decode_attachment_body(&unpadded).expect("unpadded"), b"Hello");
    }
}
```

- [ ] **Step 2: Add a failing HTTP client attachment test**

In `crates/locality-gmail/src/client.rs` tests, add:

```rust
#[test]
fn get_attachment_calls_gmail_attachment_endpoint() {
    let (base_url, request_rx, server) = spawn_response_server(
        "HTTP/1.1 200 OK",
        r#"{"attachmentId":"attach-1","size":5,"data":"SGVsbG8"}"#,
    );
    let client = HttpGmailApiClient::with_base_url("access-token", base_url);

    let attachment = client
        .get_attachment("msg-1", "attach-1")
        .expect("attachment response");

    assert_eq!(attachment.attachment_id.as_deref(), Some("attach-1"));
    assert_eq!(attachment.size, Some(5));
    assert_eq!(attachment.data.as_deref(), Some("SGVsbG8"));
    let request = request_rx.recv().expect("request line");
    server.join().expect("server exits");
    assert!(
        request.starts_with("GET /users/me/messages/msg-1/attachments/attach-1 "),
        "{request}"
    );
}
```

- [ ] **Step 3: Run attachment tests and verify they fail**

Run:

```bash
cargo test -p locality-gmail attachment
```

Expected: FAIL because `get_attachment` and `attachment_id` response support are not wired.

- [ ] **Step 4: Extend DTOs**

In `crates/locality-gmail/src/dto.rs`, change `GmailMessagePartBody` to:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessagePartBody {
    pub attachment_id: Option<String>,
    pub size: Option<u64>,
    pub data: Option<String>,
}
```

This keeps existing body decoding fields and allows the attachment endpoint response to deserialize into the same DTO.

- [ ] **Step 5: Add GmailApi attachment method**

In `crates/locality-gmail/src/client.rs`, import `GmailMessagePartBody`.

Add to `GmailApi`:

```rust
fn get_attachment(
    &self,
    message_id: &str,
    attachment_id: &str,
) -> LocalityResult<GmailMessagePartBody>;
```

Implement in `HttpGmailApiClient`:

```rust
fn get_attachment(
    &self,
    message_id: &str,
    attachment_id: &str,
) -> LocalityResult<GmailMessagePartBody> {
    self.get_json(
        &format!("/users/me/messages/{message_id}/attachments/{attachment_id}"),
        Vec::new(),
    )
}
```

Update every `FakeGmailApi` implementation in tests to provide:

```rust
fn get_attachment(
    &self,
    _message_id: &str,
    _attachment_id: &str,
) -> locality_core::LocalityResult<GmailMessagePartBody> {
    Ok(GmailMessagePartBody::default())
}
```

- [ ] **Step 6: Export attachments module**

In `crates/locality-gmail/src/lib.rs`, add:

```rust
pub mod attachments;
```

- [ ] **Step 7: Run focused attachment tests**

Run:

```bash
cargo test -p locality-gmail attachment
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/locality-gmail/src/dto.rs crates/locality-gmail/src/client.rs crates/locality-gmail/src/attachments.rs crates/locality-gmail/src/lib.rs
git commit -m "feat: add gmail attachment metadata helpers"
```

## Task 6: Download Attachments During Message Hydration

**Files:**
- Modify: `crates/locality-gmail/src/render.rs`
- Modify: `crates/localityd/src/gmail.rs`
- Test: `crates/locality-gmail/src/render.rs`
- Test: `crates/localityd/src/gmail.rs`

- [ ] **Step 1: Write failing render test for attachment frontmatter**

In `crates/locality-gmail/src/render.rs` tests, add:

```rust
#[test]
fn renders_attachment_metadata_without_downloading_bytes() {
    let message: GmailMessage = serde_json::from_value(serde_json::json!({
        "id": "msg-attach",
        "threadId": "thread-attach",
        "labelIds": ["INBOX"],
        "internalDate": "1720900000000",
        "payload": {
            "mimeType": "multipart/mixed",
            "headers": [
                { "name": "Subject", "value": "Attachments" }
            ],
            "parts": [
                {
                    "mimeType": "text/plain",
                    "body": { "data": "Qm9keQo" }
                },
                {
                    "filename": "Invoice.pdf",
                    "mimeType": "application/pdf",
                    "body": { "attachmentId": "attach-1", "size": 12 }
                }
            ]
        }
    }))
    .expect("message");

    let rendered = render_gmail_message(&GmailNativeBundle {
        mailbox: "inbox".to_string(),
        message,
    })
    .expect("render");

    assert_eq!(rendered.document.body, "Body\n");
    assert_eq!(rendered.attachment_specs.len(), 1);
    assert!(rendered.document.frontmatter.contains("attachments:"));
    assert!(rendered.document.frontmatter.contains("filename: \"Invoice.pdf\""));
    assert!(rendered.document.frontmatter.contains("attachment_id: \"attach-1\""));
    assert!(
        rendered
            .document
            .frontmatter
            .contains("path: \".loc/gmail/attachments/msg-attach/invoice-attach-1.pdf\"")
    );
}
```

- [ ] **Step 2: Run the render test and verify it fails**

Run:

```bash
cargo test -p locality-gmail renders_attachment_metadata_without_downloading_bytes
```

Expected: FAIL because `GmailRenderedEntity` has no `attachment_specs` and frontmatter omits attachments.

- [ ] **Step 3: Add attachment specs to rendered Gmail messages**

In `crates/locality-gmail/src/render.rs`, import:

```rust
use crate::attachments::{GmailAttachmentSpec, collect_attachment_specs};
```

Change `GmailRenderedEntity` to:

```rust
pub struct GmailRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub attachment_specs: Vec<GmailAttachmentSpec>,
}
```

In `render_gmail_message`, compute specs:

```rust
let attachment_specs = collect_attachment_specs(&bundle.message);
let frontmatter = message_frontmatter_with_attachments(bundle, &attachment_specs);
```

Return:

```rust
Ok(GmailRenderedEntity {
    document,
    shadow,
    attachment_specs,
})
```

Replace `message_frontmatter` internals with a wrapper:

```rust
pub fn message_frontmatter(bundle: &GmailNativeBundle) -> String {
    let specs = collect_attachment_specs(&bundle.message);
    message_frontmatter_with_attachments(bundle, &specs)
}

fn message_frontmatter_with_attachments(
    bundle: &GmailNativeBundle,
    attachment_specs: &[GmailAttachmentSpec],
) -> String {
    let message = &bundle.message;
    let version = remote_version(message);
    let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
    let subject = headers
        .get("subject")
        .cloned()
        .unwrap_or_else(|| "(no subject)".to_string());
    let attachment_yaml = if attachment_specs.is_empty() {
        " []\n".to_string()
    } else {
        format!(
            "\n{}",
            attachment_specs
                .iter()
                .map(|spec| {
                    format!(
                        "    - filename: {}\n      attachment_id: {}\n      mime_type: {}\n      size: {}\n      path: {}\n",
                        yaml_scalar(&spec.filename),
                        yaml_scalar(&spec.attachment_id),
                        yaml_scalar(&spec.mime_type),
                        spec.size
                            .map(|size| size.to_string())
                            .unwrap_or_else(|| "null".to_string()),
                        yaml_scalar(&spec.local_path.display().to_string())
                    )
                })
                .collect::<String>()
        )
    };

    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  message_id: {}\n  thread_id: {}\n  labels: [{}]\n  attachments:{}from: {}\nto: [{}]\ncc: [{}]\nbcc: []\nsubject: {}\ndate: {}\n",
        yaml_scalar(&message.id),
        GMAIL_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&subject),
        yaml_scalar(&bundle.mailbox),
        yaml_scalar(&message.id),
        yaml_scalar(message.thread_id.as_deref().unwrap_or("")),
        message
            .label_ids
            .iter()
            .map(|label| yaml_scalar(label))
            .collect::<Vec<_>>()
            .join(", "),
        attachment_yaml,
        yaml_scalar(headers.get("from").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("to").map(String::as_str).unwrap_or("")),
        yaml_list_items(headers.get("cc").map(String::as_str).unwrap_or("")),
        yaml_scalar(&subject),
        yaml_scalar(headers.get("date").map(String::as_str).unwrap_or("")),
    )
}
```

- [ ] **Step 4: Write failing hydration test for downloading attachments**

In `crates/localityd/src/gmail.rs` tests, add a test module if none exists:

```rust
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use locality_core::hydration::{HydrationReason, HydrationRequest};
    use locality_core::model::{HydrationState, MountId, RemoteId};
    use locality_gmail::client::GmailApi;
    use locality_gmail::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage,
        GmailMessageList, GmailMessagePartBody,
    };
    use locality_gmail::{GmailConfig, GmailConnector};

    use super::*;

    #[test]
    fn gmail_hydration_downloads_message_attachments_as_assets() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let request = HydrationRequest {
            mount_id: MountId::new("gmail-main"),
            remote_id: RemoteId::new("msg-attach"),
            path: "inbox/msg-attach.md".into(),
            target_state: HydrationState::Hydrated,
            reason: HydrationReason::Explicit,
        };

        let hydrated = connector.fetch_render(&request).expect("hydrate");

        assert_eq!(hydrated.assets.len(), 1);
        assert_eq!(
            hydrated.assets[0].path,
            std::path::PathBuf::from(".loc/gmail/attachments/msg-attach/invoice-attach-1.pdf")
        );
        assert_eq!(hydrated.assets[0].bytes, b"attachment bytes");
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.attachments,
            vec![("msg-attach".to_string(), "attach-1".to_string())]
        );
    }

    #[derive(Default, Debug)]
    struct FakeGmailApi {
        calls: Mutex<FakeCalls>,
    }

    #[derive(Default, Debug)]
    struct FakeCalls {
        attachments: Vec<(String, String)>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(
            &self,
            _label_id: &str,
            _max_results: u32,
            _page_token: Option<&str>,
            _query: Option<&str>,
        ) -> locality_core::LocalityResult<GmailMessageList> {
            Ok(GmailMessageList::default())
        }

        fn get_message_metadata(&self, message_id: &str) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_message_full(&self, message_id: &str) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_attachment(
            &self,
            message_id: &str,
            attachment_id: &str,
        ) -> locality_core::LocalityResult<GmailMessagePartBody> {
            self.calls
                .lock()
                .expect("calls")
                .attachments
                .push((message_id.to_string(), attachment_id.to_string()));
            Ok(GmailMessagePartBody {
                attachment_id: Some(attachment_id.to_string()),
                size: Some(16),
                data: Some(URL_SAFE_NO_PAD.encode(b"attachment bytes")),
            })
        }

        fn create_draft(&self, _request: GmailDraftCreateRequest) -> locality_core::LocalityResult<GmailDraft> {
            panic!("not used")
        }

        fn send_draft(&self, _request: GmailDraftSendRequest) -> locality_core::LocalityResult<GmailMessage> {
            panic!("not used")
        }
    }

    fn message_fixture(id: &str) -> GmailMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": "thread-attach",
            "labelIds": ["INBOX"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    { "name": "Subject", "value": "Attachments" }
                ],
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "filename": "Invoice.pdf",
                        "mimeType": "application/pdf",
                        "body": { "attachmentId": "attach-1", "size": 16 }
                    }
                ]
            }
        }))
        .expect("message")
    }
}
```

- [ ] **Step 5: Run hydration test and verify it fails**

Run:

```bash
cargo test -p localityd gmail_hydration_downloads_message_attachments_as_assets
```

Expected: FAIL because Gmail hydration still returns `assets: Vec::new()`.

- [ ] **Step 6: Download attachment assets during Gmail hydration**

In `crates/localityd/src/gmail.rs`, import:

```rust
use locality_gmail::attachments::decode_attachment_body;
use crate::hydration::HydratedAsset;
```

Replace the `assets: Vec::new()` part in `HydrationSource for GmailConnector` with:

```rust
let mut assets = Vec::new();
for spec in &rendered.attachment_specs {
    let body = self
        .api()
        .get_attachment(&spec.message_id, &spec.attachment_id)?;
    assets.push(HydratedAsset {
        path: spec.local_path.clone(),
        bytes: decode_attachment_body(&body)?,
        media: None,
    });
}
```

Expose the connector API reference from `crates/locality-gmail/src/connector.rs`:

```rust
pub fn api(&self) -> &dyn GmailApi {
    self.api.as_ref()
}
```

The completed hydration return should be:

```rust
Ok(HydratedEntity {
    document: rendered.document,
    shadow: rendered.shadow,
    remote_edited_at: Some(remote_version(&bundle.message)),
    assets,
})
```

- [ ] **Step 7: Run attachment hydration tests**

Run:

```bash
cargo test -p locality-gmail renders_attachment_metadata_without_downloading_bytes
cargo test -p localityd gmail_hydration_downloads_message_attachments_as_assets
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/locality-gmail/src/render.rs crates/locality-gmail/src/connector.rs crates/localityd/src/gmail.rs
git commit -m "feat: hydrate gmail attachments on demand"
```

## Task 7: Add Gmail Thread API DTOs And Client Methods

**Files:**
- Modify: `crates/locality-gmail/src/dto.rs`
- Modify: `crates/locality-gmail/src/client.rs`
- Test: `crates/locality-gmail/src/client.rs`

- [ ] **Step 1: Add failing thread HTTP tests**

In `crates/locality-gmail/src/client.rs` tests, add:

```rust
#[test]
fn list_threads_calls_gmail_threads_endpoint_with_query() {
    let (base_url, request_rx, server) = spawn_response_server(
        "HTTP/1.1 200 OK",
        r#"{"threads":[{"id":"thread-1","snippet":"hello"}],"nextPageToken":"next"}"#,
    );
    let client = HttpGmailApiClient::with_base_url("access-token", base_url);

    let threads = client
        .list_threads(
            "INBOX",
            100,
            Some("page-2"),
            Some("after:2026/07/01 before:2026/07/15"),
        )
        .expect("threads");

    assert_eq!(threads.threads[0].id, "thread-1");
    assert_eq!(threads.next_page_token.as_deref(), Some("next"));
    let request = request_rx.recv().expect("request line");
    server.join().expect("server exits");
    assert!(request.starts_with("GET /users/me/threads?"), "{request}");
    assert!(request.contains("labelIds=INBOX"), "{request}");
    assert!(request.contains("maxResults=100"), "{request}");
    assert!(request.contains("pageToken=page-2"), "{request}");
    assert!(request.contains("q=after%3A2026%2F07%2F01+before%3A2026%2F07%2F15"), "{request}");
}

#[test]
fn get_thread_metadata_requests_metadata_format_headers() {
    let (base_url, request_rx, server) = spawn_response_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"thread-1","messages":[{"id":"msg-1","threadId":"thread-1"}]}"#,
    );
    let client = HttpGmailApiClient::with_base_url("access-token", base_url);

    let thread = client.get_thread_metadata("thread-1").expect("thread");

    assert_eq!(thread.id, "thread-1");
    let request = request_rx.recv().expect("request line");
    server.join().expect("server exits");
    assert!(request.starts_with("GET /users/me/threads/thread-1?"), "{request}");
    assert!(request.contains("format=metadata"), "{request}");
    assert!(request.contains("metadataHeaders=Subject"), "{request}");
}
```

- [ ] **Step 2: Run thread client tests and verify they fail**

Run:

```bash
cargo test -p locality-gmail list_threads_calls_gmail_threads_endpoint_with_query
cargo test -p locality-gmail get_thread_metadata_requests_metadata_format_headers
```

Expected: FAIL because thread DTOs and methods do not exist.

- [ ] **Step 3: Add thread DTOs**

In `crates/locality-gmail/src/dto.rs`, add:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailThreadList {
    #[serde(default)]
    pub threads: Vec<GmailThreadRef>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailThreadRef {
    pub id: String,
    pub snippet: Option<String>,
    pub history_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailThread {
    pub id: String,
    pub history_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<GmailMessage>,
}
```

- [ ] **Step 4: Add GmailApi thread methods**

In `crates/locality-gmail/src/client.rs`, import `GmailThread` and `GmailThreadList`.

Add to `GmailApi`:

```rust
fn list_threads(
    &self,
    label_id: &str,
    max_results: u32,
    page_token: Option<&str>,
    query: Option<&str>,
) -> LocalityResult<GmailThreadList>;
fn get_thread_metadata(&self, thread_id: &str) -> LocalityResult<GmailThread>;
fn get_thread_full(&self, thread_id: &str) -> LocalityResult<GmailThread>;
```

Implement for `HttpGmailApiClient`:

```rust
fn list_threads(
    &self,
    label_id: &str,
    max_results: u32,
    page_token: Option<&str>,
    search_query: Option<&str>,
) -> LocalityResult<GmailThreadList> {
    let mut params = vec![
        ("labelIds".to_string(), label_id.to_string()),
        ("maxResults".to_string(), max_results.to_string()),
    ];
    if let Some(page_token) = page_token {
        params.push(("pageToken".to_string(), page_token.to_string()));
    }
    if let Some(search_query) = search_query {
        params.push(("q".to_string(), search_query.to_string()));
    }
    self.get_json("/users/me/threads", params)
}

fn get_thread_metadata(&self, thread_id: &str) -> LocalityResult<GmailThread> {
    let mut query = vec![("format".to_string(), "metadata".to_string())];
    for header in ["From", "To", "Cc", "Bcc", "Subject", "Date", "Message-ID"] {
        query.push(("metadataHeaders".to_string(), header.to_string()));
    }
    self.get_json(&format!("/users/me/threads/{thread_id}"), query)
}

fn get_thread_full(&self, thread_id: &str) -> LocalityResult<GmailThread> {
    self.get_json(
        &format!("/users/me/threads/{thread_id}"),
        vec![("format".to_string(), "full".to_string())],
    )
}
```

Update test fakes to return `GmailThreadList::default()` and `GmailThread::default()` for the new methods until Task 8 uses them.

- [ ] **Step 5: Run focused thread client tests**

Run:

```bash
cargo test -p locality-gmail list_threads_calls_gmail_threads_endpoint_with_query
cargo test -p locality-gmail get_thread_metadata_requests_metadata_format_headers
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/locality-gmail/src/dto.rs crates/locality-gmail/src/client.rs
git commit -m "feat: add gmail thread api methods"
```

## Task 8: Implement Opt-In Gmail Thread Projection

**Files:**
- Modify: `crates/locality-gmail/src/render.rs`
- Modify: `crates/locality-gmail/src/connector.rs`
- Modify: `crates/localityd/src/gmail.rs`
- Test: `crates/locality-gmail/src/render.rs`
- Test: `crates/locality-gmail/src/connector.rs`

- [ ] **Step 1: Write failing thread render test**

In `crates/locality-gmail/src/render.rs` tests, add:

```rust
#[test]
fn renders_thread_document_with_message_sections() {
    let thread: crate::dto::GmailThread = serde_json::from_value(serde_json::json!({
        "id": "thread-1",
        "historyId": "h1",
        "messages": [
            {
                "id": "msg-1",
                "threadId": "thread-1",
                "labelIds": ["INBOX"],
                "internalDate": "1720900000000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [
                        { "name": "From", "value": "Ann <ann@example.com>" },
                        { "name": "Subject", "value": "Quarterly update" },
                        { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                    ],
                    "body": { "data": "Rmlyc3QgbWVzc2FnZS4K" }
                }
            },
            {
                "id": "msg-2",
                "threadId": "thread-1",
                "labelIds": ["SENT"],
                "internalDate": "1720900500000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [
                        { "name": "From", "value": "Me <me@example.com>" },
                        { "name": "Subject", "value": "Re: Quarterly update" },
                        { "name": "Date", "value": "Tue, 14 Jul 2026 09:38:20 +0000" }
                    ],
                    "body": { "data": "UmVwbHkuCg" }
                }
            }
        ]
    }))
    .expect("thread");

    let rendered = render_gmail_thread(&GmailThreadNativeBundle {
        mailbox: "inbox".to_string(),
        thread,
    })
    .expect("render thread");

    assert!(rendered.document.frontmatter.contains("type: page"));
    assert!(rendered.document.frontmatter.contains("thread_id: \"thread-1\""));
    assert!(rendered.document.frontmatter.contains("message_count: 2"));
    assert!(rendered.document.body.contains("## Ann <ann@example.com>"));
    assert!(rendered.document.body.contains("First message."));
    assert!(rendered.document.body.contains("## Me <me@example.com>"));
    assert!(rendered.document.body.contains("Reply."));
    assert_eq!(rendered.shadow.entity_id.as_str(), "gmail-thread:inbox:thread-1");
}
```

- [ ] **Step 2: Run thread render test and verify it fails**

Run:

```bash
cargo test -p locality-gmail renders_thread_document_with_message_sections
```

Expected: FAIL because `GmailThreadNativeBundle` and `render_gmail_thread` do not exist.

- [ ] **Step 3: Add thread render types and functions**

In `crates/locality-gmail/src/render.rs`, import `GmailThread`.

Add:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailThreadNativeBundle {
    pub mailbox: String,
    pub thread: GmailThread,
}

pub fn thread_remote_id(mailbox: &str, thread_id: &str) -> RemoteId {
    RemoteId::new(format!("gmail-thread:{mailbox}:{thread_id}"))
}

pub fn parse_thread_remote_id(remote_id: &RemoteId) -> Option<(&str, &str)> {
    let rest = remote_id.as_str().strip_prefix("gmail-thread:")?;
    rest.split_once(':')
}

pub fn render_gmail_thread(bundle: &GmailThreadNativeBundle) -> LocalityResult<GmailRenderedEntity> {
    let remote_id = thread_remote_id(&bundle.mailbox, &bundle.thread.id);
    let first_message = bundle.thread.messages.first();
    let subject = first_message.map(message_subject_from_headers).unwrap_or_else(|| "(no subject)".to_string());
    let version = thread_remote_version(&bundle.thread);
    let body = thread_body(&bundle.thread);
    let frontmatter = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngmail:\n  mailbox: {}\n  thread_id: {}\n  message_count: {}\n",
        yaml_scalar(remote_id.as_str()),
        GMAIL_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&subject),
        yaml_scalar(&bundle.mailbox),
        yaml_scalar(&bundle.thread.id),
        bundle.thread.messages.len(),
    );
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let shadow = ShadowDocument::from_synced_body(
        remote_id,
        body,
        1,
        bundle
            .thread
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| RemoteId::new(format!("{}:thread-body:{index}", message.id)))
            .collect(),
    )
    .map_err(|error| LocalityError::InvalidState(error.to_string()))?
    .with_frontmatter(frontmatter);

    Ok(GmailRenderedEntity {
        document,
        shadow,
        attachment_specs: bundle
            .thread
            .messages
            .iter()
            .flat_map(crate::attachments::collect_attachment_specs)
            .collect(),
    })
}

pub fn thread_remote_version(thread: &GmailThread) -> String {
    let mut parts = thread
        .messages
        .iter()
        .map(remote_version)
        .collect::<Vec<_>>();
    parts.sort();
    format!("gmail-thread:{}:{}", thread.id, parts.join("|"))
}

fn thread_body(thread: &GmailThread) -> String {
    thread
        .messages
        .iter()
        .map(|message| {
            let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
            let from = headers.get("from").map(String::as_str).unwrap_or("");
            let date = headers.get("date").map(String::as_str).unwrap_or("");
            let body = message_body(message).unwrap_or_default();
            format!(
                "## {}\n\nDate: {}\nMessage-ID: {}\n\n{}",
                from,
                date,
                message.id,
                body
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_subject_from_headers(message: &GmailMessage) -> String {
    message
        .payload
        .as_ref()
        .map(header_map)
        .and_then(|headers| headers.get("subject").cloned())
        .filter(|subject| !subject.trim().is_empty())
        .unwrap_or_else(|| "(no subject)".to_string())
}
```

Make `message_body` visible inside this module by leaving it as a private function in the same file.

- [ ] **Step 4: Write failing connector thread projection tests**

In `crates/locality-gmail/src/connector.rs` tests, add:

```rust
#[test]
fn enumerate_projects_threads_when_thread_view_enabled() {
    let api = Arc::new(FakeGmailApi::default());
    let settings = crate::settings::GmailMountSettings::default()
        .with_view(crate::settings::GmailProjectionView::Threads);
    let connector = GmailConnector::with_api(
        GmailConfig::new("token").with_settings(settings),
        api.clone(),
    );

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("gmail-main"),
            cursor: None,
        })
        .expect("enumerate");

    assert!(entries.iter().any(|entry| entry.remote_id == RemoteId::new("gmail-thread:inbox:thread-inbox-1")));
    assert!(entries.iter().any(|entry| entry.path == std::path::PathBuf::from("inbox/1720900000000-hello-thread-inbox-1/page.md")));
    assert!(entries.iter().any(|entry| entry.remote_id == RemoteId::new("gmail-thread:sent:thread-sent-1")));
}

#[test]
fn list_children_for_thread_page_returns_message_files() {
    let api = Arc::new(FakeGmailApi::default());
    let settings = crate::settings::GmailMountSettings::default()
        .with_view(crate::settings::GmailProjectionView::Threads);
    let connector = GmailConnector::with_api(
        GmailConfig::new("token").with_settings(settings),
        api,
    );

    let result = connector
        .list_children(ListChildrenRequest {
            mount_id: MountId::new("gmail-main"),
            container: ChildContainer::PageChildren(RemoteId::new("gmail-thread:inbox:thread-inbox-1")),
            parent_path: "inbox/1720900000000-hello-thread-inbox-1".into(),
        })
        .expect("children");

    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].remote_id, RemoteId::new("inbox-msg-1"));
    assert_eq!(
        result.entries[0].path,
        std::path::PathBuf::from("inbox/1720900000000-hello-thread-inbox-1/1720900000000-hello-inbox-msg-1.md")
    );
}
```

Extend `FakeGmailApi` in the same module:

```rust
fn list_threads(
    &self,
    label_id: &str,
    max_results: u32,
    page_token: Option<&str>,
    query: Option<&str>,
) -> locality_core::LocalityResult<crate::dto::GmailThreadList> {
    let _ = (max_results, page_token, query);
    let id = match label_id {
        "INBOX" => "thread-inbox-1",
        "SENT" => "thread-sent-1",
        other => panic!("unexpected label {other}"),
    };
    Ok(crate::dto::GmailThreadList {
        threads: vec![crate::dto::GmailThreadRef {
            id: id.to_string(),
            snippet: Some("hello".to_string()),
            history_id: Some("h1".to_string()),
        }],
        next_page_token: None,
        result_size_estimate: Some(1),
    })
}

fn get_thread_metadata(&self, thread_id: &str) -> locality_core::LocalityResult<crate::dto::GmailThread> {
    Ok(thread_fixture(thread_id))
}

fn get_thread_full(&self, thread_id: &str) -> locality_core::LocalityResult<crate::dto::GmailThread> {
    Ok(thread_fixture(thread_id))
}
```

Add:

```rust
fn thread_fixture(thread_id: &str) -> crate::dto::GmailThread {
    let message_id = if thread_id.contains("sent") {
        "sent-msg-1"
    } else {
        "inbox-msg-1"
    };
    crate::dto::GmailThread {
        id: thread_id.to_string(),
        history_id: Some("h1".to_string()),
        messages: vec![message_fixture(message_id)],
    }
}
```

- [ ] **Step 5: Run thread projection tests and verify they fail**

Run:

```bash
cargo test -p locality-gmail enumerate_projects_threads_when_thread_view_enabled
cargo test -p locality-gmail list_children_for_thread_page_returns_message_files
```

Expected: FAIL because `enumerate_threads_not_enabled_yet` still returns unsupported.

- [ ] **Step 6: Implement thread listing and entries**

In `crates/locality-gmail/src/connector.rs`, update imports:

```rust
use crate::render::{
    GmailDraftDocument, GmailNativeBundle, GmailThreadNativeBundle, build_draft_mime_with_message_id,
    message_frontmatter, raw_message_base64url, remote_version, render_gmail_message,
    render_gmail_thread, thread_remote_id, thread_remote_version, parse_thread_remote_id,
};
```

Remove `enumerate_threads_not_enabled_yet`.

In `enumerate`, branch:

```rust
if self.config.settings.gmail.view == GmailProjectionView::Threads {
    let mut entries = gmail_folder_entries(&request.mount_id, Path::new(""));
    entries.extend(list_thread_entries(
        self.api.as_ref(),
        &self.config.settings,
        &request.mount_id,
        "INBOX",
        "inbox",
        Path::new("inbox"),
    )?);
    entries.extend(list_thread_entries(
        self.api.as_ref(),
        &self.config.settings,
        &request.mount_id,
        "SENT",
        "sent",
        Path::new("sent"),
    )?);
    return Ok(entries);
}
```

In `list_children`, add this match arm before the final `_`:

```rust
ChildContainer::PageChildren(remote_id)
    if parse_thread_remote_id(&remote_id).is_some() =>
{
    let Some((mailbox, thread_id)) = parse_thread_remote_id(&remote_id) else {
        return Ok(ListChildrenResult { entries: Vec::new() });
    };
    let thread = self.api.get_thread_metadata(thread_id)?;
    thread
        .messages
        .into_iter()
        .map(|message| Ok(message_entry(&request.mount_id, &request.parent_path, mailbox, message)))
        .collect::<LocalityResult<Vec<_>>>()?
}
```

Add helper functions:

```rust
fn list_thread_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    label_id: &str,
    mailbox: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let threads = list_thread_refs(api, settings, label_id)?;
    threads
        .into_iter()
        .map(|thread_ref| {
            let thread = api.get_thread_metadata(&thread_ref.id)?;
            Ok(thread_entry(mount_id, parent_path, mailbox, thread))
        })
        .collect()
}

fn list_thread_refs(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    label_id: &str,
) -> LocalityResult<Vec<crate::dto::GmailThreadRef>> {
    let query = settings.gmail.date_window.as_ref().map(|window| window.query());
    let mut page_token = None;
    let mut threads = Vec::new();
    loop {
        let page = api.list_threads(
            label_id,
            GMAIL_PAGE_SIZE,
            page_token.as_deref(),
            query.as_deref(),
        )?;
        threads.extend(page.threads);
        let Some(next) = page.next_page_token else {
            break;
        };
        page_token = Some(next);
        if query.is_none() {
            break;
        }
    }
    Ok(threads)
}

fn thread_entry(
    mount_id: &MountId,
    parent_path: &Path,
    mailbox: &str,
    thread: crate::dto::GmailThread,
) -> TreeEntry {
    let title = thread
        .messages
        .first()
        .map(message_subject)
        .unwrap_or_else(|| "(no subject)".to_string());
    let version = thread_remote_version(&thread);
    let path = parent_path
        .join(thread_directory_name(&thread, &title))
        .join("page.md");
    let bundle = GmailThreadNativeBundle {
        mailbox: mailbox.to_string(),
        thread: thread.clone(),
    };
    let stub_frontmatter = render_gmail_thread(&bundle)
        .ok()
        .map(|rendered| rendered.document.frontmatter);
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: thread_remote_id(mailbox, &thread.id),
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(version),
        stub_frontmatter,
    }
}

fn thread_directory_name(thread: &crate::dto::GmailThread, title: &str) -> String {
    let date = thread
        .messages
        .iter()
        .filter_map(|message| message.internal_date.as_deref())
        .min()
        .unwrap_or("unknown");
    format!(
        "{}-{}-{}",
        safe_slug(date),
        safe_slug(title),
        safe_slug(&thread.id)
    )
}
```

- [ ] **Step 7: Fetch and render thread entities**

In `fetch`, add this before message fetch:

```rust
if let Some((mailbox, thread_id)) = parse_thread_remote_id(&request.remote_id) {
    let thread = self.api.get_thread_full(thread_id)?;
    let bundle = GmailThreadNativeBundle {
        mailbox: mailbox.to_string(),
        thread,
    };
    let raw = serde_json::to_vec(&bundle)
        .map_err(|error| LocalityError::Io(format!("gmail thread native encode failed: {error}")))?;
    return Ok(NativeEntity {
        remote_id: request.remote_id,
        kind: "gmail_thread".to_string(),
        raw,
    });
}
```

In `render`, branch by native kind:

```rust
if entity.kind == "gmail_thread" {
    let bundle = serde_json::from_slice::<GmailThreadNativeBundle>(&entity.raw)
        .map_err(|error| LocalityError::Io(format!("gmail thread native decode failed: {error}")))?;
    return render_gmail_thread(&bundle).map(|rendered| rendered.document);
}
```

In `observe`, add a thread branch:

```rust
if let Some((mailbox, thread_id)) = parse_thread_remote_id(&request.remote_id) {
    let thread = self.api.get_thread_metadata(thread_id)?;
    let entry = thread_entry(&request.mount_id, Path::new(mailbox), mailbox, thread.clone());
    return Ok(RemoteObservation::new(
        request.mount_id,
        request.remote_id,
        EntityKind::Page,
        entry.title,
        entry.path,
    )
    .with_parent(RemoteId::new(mailbox_folder_id(mailbox)))
    .with_remote_version(RemoteVersion::new(thread_remote_version(&thread)))
    .with_raw_metadata_json(
        serde_json::to_string(&thread).unwrap_or_else(|_| "{}".to_string()),
    ));
}
```

- [ ] **Step 8: Hydrate thread entities in daemon Gmail source**

In `crates/localityd/src/gmail.rs` `fetch_render`, branch on native kind:

```rust
if native.kind == "gmail_thread" {
    let bundle = serde_json::from_slice::<locality_gmail::render::GmailThreadNativeBundle>(&native.raw)
        .map_err(|error| LocalityError::Io(format!("gmail thread native decode failed: {error}")))?;
    let rendered = locality_gmail::render::render_gmail_thread(&bundle)?;
    let mut assets = Vec::new();
    for spec in &rendered.attachment_specs {
        let body = self.api().get_attachment(&spec.message_id, &spec.attachment_id)?;
        assets.push(crate::hydration::HydratedAsset {
            path: spec.local_path.clone(),
            bytes: decode_attachment_body(&body)?,
            media: None,
        });
    }
    return Ok(HydratedEntity {
        document: rendered.document,
        shadow: rendered.shadow,
        remote_edited_at: Some(locality_gmail::render::thread_remote_version(&bundle.thread)),
        assets,
    });
}
```

Keep the existing message branch after this.

- [ ] **Step 9: Run thread tests**

Run:

```bash
cargo test -p locality-gmail renders_thread_document_with_message_sections
cargo test -p locality-gmail enumerate_projects_threads_when_thread_view_enabled
cargo test -p locality-gmail list_children_for_thread_page_returns_message_files
```

Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/locality-gmail/src/render.rs crates/locality-gmail/src/connector.rs crates/localityd/src/gmail.rs
git commit -m "feat: add gmail thread projection"
```

## Task 9: Documentation And Final Verification

**Files:**
- Modify: `docs/gmail-connector.md`
- Modify: `docs/cli.md`
- Modify: `docs/locality-store.md`

- [ ] **Step 1: Update Gmail connector docs**

In `docs/gmail-connector.md`, replace the `Projection And Pull` section with:

```markdown
## Projection And Pull

By default, Pull enumerates the recent 100 inbox messages and recent 100 sent
messages. The `draft/` folder is created locally, but the connector does not
enumerate remote Gmail drafts.

Gmail mounts can be registered with a date window:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main \
  --after 2026-07-01 \
  --before 2026-07-15
```

Date-window mounts use Gmail search query dates and page through all matching
messages for `inbox/` and `sent/` instead of stopping after the first recent 100
results.

Message view is the default projection:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-msg-1.md
  sent/
    1720900100000-reply-msg-2.md
  draft/
```

Thread view is opt-in:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main --view threads
```

Thread view projects thread pages and child messages:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-thread-a/
      page.md
      1720900000000-quarterly-update-msg-1.md
  sent/
  draft/
```

Inbox, sent, and thread content is read-only. Creating a Markdown file directly
under `draft/` remains the send surface.
```

Add an `Attachments` section:

```markdown
## Attachments

Gmail attachment bytes are fetched on demand. Enumeration and metadata refreshes
do not download attachment bodies. When a specific message or thread is
hydrated, Locality downloads the attachment bodies referenced by that message or
thread and writes them under:

```text
.loc/gmail/attachments/<message-id>/
```

Rendered message frontmatter includes attachment filename, MIME type, size,
Gmail attachment ID, and the local path. Draft sends still reject `attachment`
or `attachments` frontmatter; outbound attachments require a separate design.
```

- [ ] **Step 2: Update CLI docs**

In `docs/cli.md`, find the Gmail mount section and add:

```markdown
Gmail mount options:

- `--after YYYY-MM-DD --before YYYY-MM-DD`: persist a Gmail date window for
  inbox and sent enumeration. The flags must be used together.
- `--view messages`: keep the default flat message-file projection.
- `--view threads`: project Gmail threads as page directories with child message
  files.
```

- [ ] **Step 3: Update store docs for settings JSON**

In `docs/locality-store.md`, ensure the mounts table description includes:

```markdown
- `mounts`: mount id, connector, local root, optional remote root id, read-only
  flag, projection mode, optional connection id, and connector-specific
  `settings_json`;
```

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo test -p locality-store repository_persists_mount_settings_json
cargo test -p locality-store sqlite_store_migrates_v17_mounts_with_default_settings_json
cargo test -p loc-cli cli_mount_gmail_persists_date_window_and_thread_view
cargo test -p loc-cli cli_mount_gmail_rejects_partial_date_window
cargo test -p locality-gmail enumerate_with_date_window_pages_all_matching_messages_with_gmail_query
cargo test -p locality-gmail renders_attachment_metadata_without_downloading_bytes
cargo test -p localityd gmail_hydration_downloads_message_attachments_as_assets
cargo test -p locality-gmail renders_thread_document_with_message_sections
cargo test -p locality-gmail enumerate_projects_threads_when_thread_view_enabled
cargo test -p locality-gmail list_children_for_thread_page_returns_message_files
```

Expected: PASS.

- [ ] **Step 5: Run broader crate checks**

Run:

```bash
cargo test -p locality-store -p locality-gmail -p localityd -p loc-cli
```

Expected: PASS.

- [ ] **Step 6: Commit docs**

```bash
git add docs/gmail-connector.md docs/cli.md docs/locality-store.md
git commit -m "docs: document gmail attachments dates and threads"
```

## Self-Review

- Spec coverage: date windows are covered by Tasks 1 through 4 and documented in Task 9. Attachment-on-demand read support is covered by Tasks 5 and 6 and documented in Task 9. Gmail thread support is covered by Tasks 7 and 8 and documented in Task 9.
- Scope: outbound draft attachments remain intentionally rejected because the requested attachment behavior was "fetch the actual attachment for a specific email"; changing send MIME construction and local file upload policy is a separate write-path feature.
- Compatibility: default message view and recent-100 behavior are preserved when `settings_json` is `{}`. SQLite v17 state migrates to v18 with default settings and newer schema versions still fail through the existing schema-version guard.
- Type consistency: `GmailMountSettings`, `GmailProjectionView`, `GmailDateWindow`, `GmailThreadNativeBundle`, `GmailAttachmentSpec`, and `settings_json` use the same names across tasks.
