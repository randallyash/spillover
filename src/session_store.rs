//! Named sessions that outlive the process, one history per workspace.
//!
//! A workspace used to have a single file. That made restart a resume, and made
//! starting over a wipe. The store is now a directory: each conversation is a
//! file, an index names the current one, and `/sessions` can pick among them.
//! A leftover `{hash}.json` from the old layout is migrated on first open.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::Mode;
use crate::config::OnStuck;
use crate::fallback::ChainState;
use crate::session::{ChatMessage, Role};

/// The format this build writes. A file from another version is ignored rather
/// than guessed at: resuming a conversation under rules it was not recorded
/// under is worse than starting over.
pub const SESSION_VERSION: u32 = 1;

/// How long an auto-title may run, in characters.
const TITLE_MAX: usize = 48;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("no state directory is available on this system, so a session cannot be saved")]
    NoStateDir,
}

/// What a saved session holds. Field names are camelCase on disk to match the
/// rest of spill's JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    pub version: u32,
    /// The directory this session belongs to. Stored, not just hashed into the
    /// file name, so a load can confirm it found the right conversation.
    pub workspace: PathBuf,
    /// When this was written, as seconds since the Unix epoch.
    pub saved_at: u64,
    /// The tier answering, by its configured id rather than its position, so
    /// reordering `config.toml` does not silently move the user elsewhere.
    pub active_tier: Option<String>,
    /// The tier the user pinned by hand, if any.
    pub pinned_tier: Option<String>,
    pub sticky: bool,
    /// A stuck policy chosen for the session. `None` means each tier's own.
    pub on_stuck: Option<OnStuck>,
    pub mode: Mode,
    /// Tier id -> the conversation that tier's CLI is following.
    #[serde(default)]
    pub cli_sessions: BTreeMap<String, String>,
    /// The conversation, without the system prompt: that is rebuilt from the
    /// mode on load, so a stale "you are in PLAN MODE" prompt can never be
    /// resumed into a build-mode session.
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Stable id of this conversation within the workspace.
    #[serde(default)]
    pub id: String,
    /// What the picker shows. Inferred from the first prompt unless `named`.
    #[serde(default)]
    pub title: String,
    /// The user set the title; do not overwrite it from the transcript.
    #[serde(default)]
    pub named: bool,
}

impl SessionFile {
    /// An empty session for a workspace, ready to have state written into it.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            version: SESSION_VERSION,
            workspace: workspace.into(),
            saved_at: now_epoch(),
            active_tier: None,
            pinned_tier: None,
            sticky: true,
            on_stuck: None,
            mode: Mode::default(),
            cli_sessions: BTreeMap::new(),
            messages: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            title: "untitled".to_string(),
            named: false,
        }
    }

    /// Fill in a title from the first user prompt, unless the user named it.
    pub fn refresh_title(&mut self) {
        if self.named {
            return;
        }
        self.title = infer_title(&self.messages);
    }

    /// Whether there is anything worth remembering.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.cli_sessions.is_empty()
    }

    /// The chain state this session was saved with.
    pub fn chain_state(&self) -> ChainState {
        ChainState {
            active: self.active_tier.clone(),
            pinned: self.pinned_tier.clone(),
            sticky: self.sticky,
            on_stuck: self.on_stuck,
        }
    }
}

/// A row in the picker: enough to choose without opening the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    pub saved_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Index {
    #[serde(default)]
    current: String,
    #[serde(default)]
    sessions: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    id: String,
    title: String,
    saved_at: u64,
}

/// Named sessions for one workspace directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
    workspace: PathBuf,
}

impl SessionStore {
    /// The store for a workspace, under the platform's state directory.
    pub fn for_workspace(workspace: &Path) -> Result<Self, StoreError> {
        let dirs = directories::ProjectDirs::from("", "", "spill").ok_or(StoreError::NoStateDir)?;
        let root = dirs
            .state_dir()
            .ok_or(StoreError::NoStateDir)?
            .join("sessions");
        let key = key_for(workspace);
        let dir = root.join(&key);
        let legacy = root.join(format!("{key}.json"));
        if legacy.is_file() {
            let _ = migrate_legacy(&legacy, &dir, workspace);
        }
        Ok(Self::at(dir, workspace.to_path_buf()))
    }

    /// A store in an explicit directory, for tests.
    pub fn at(dir: PathBuf, workspace: PathBuf) -> Self {
        Self { dir, workspace }
    }

    /// Where the current session is written, for saying so when it cannot be.
    pub fn path(&self) -> PathBuf {
        match self.current_id() {
            Some(id) => self.file_path(&id),
            None => self.dir.join("index.json"),
        }
    }

    /// Read the current session, or `None` when there is not a usable one.
    pub fn load(&self) -> Option<SessionFile> {
        let id = self.current_id()?;
        self.load_id(&id)
    }

    /// Every saved conversation, newest first, with the current one marked by
    /// matching `id` against `current_id`.
    pub fn list(&self) -> Vec<SessionEntry> {
        let mut entries = self
            .read_index()
            .map(|index| {
                index
                    .sessions
                    .into_iter()
                    .map(|entry| SessionEntry {
                        id: entry.id,
                        title: entry.title,
                        saved_at: entry.saved_at,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if entries.is_empty() {
            entries = self.scan();
        }
        entries.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then(b.id.cmp(&a.id)));
        entries
    }

    pub fn current_id(&self) -> Option<String> {
        let index = self.read_index()?;
        if index.current.is_empty() {
            None
        } else {
            Some(index.current)
        }
    }

    /// Write the current session, atomically, and keep the index in step.
    pub fn save(&self, file: &SessionFile) -> io::Result<()> {
        let mut file = file.clone();
        if file.id.is_empty() {
            file.id = self
                .current_id()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        }
        file.workspace = self.workspace.clone();
        file.saved_at = now_epoch();
        file.refresh_title();
        self.write_session(&file)?;
        self.touch_index(&file)?;
        Ok(())
    }

    /// Persist the current conversation if it has anything in it, then start a
    /// blank one. An empty untitled session is reused rather than stacked.
    pub fn start_new(&self) -> SessionFile {
        if let Some(current) = self.load() {
            if current.is_empty() && !current.named {
                return current;
            }
            let _ = self.save(&current);
        }
        let file = SessionFile::new(self.workspace.clone());
        let _ = self.save(&file);
        file
    }

    /// Make this id the current session and return it.
    pub fn open(&self, id: &str) -> Option<SessionFile> {
        let file = self.load_id(id)?;
        let mut index = self.read_index().unwrap_or_default();
        if !index.sessions.iter().any(|entry| entry.id == id) {
            index.sessions.push(IndexEntry {
                id: file.id.clone(),
                title: file.title.clone(),
                saved_at: file.saved_at,
            });
        }
        index.current = id.to_string();
        let _ = self.write_index(&index);
        Some(file)
    }

    /// Name the current session. Returns the title that stuck.
    pub fn rename(&self, title: &str) -> Option<String> {
        let mut file = self.load()?;
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        file.title = title.to_string();
        file.named = true;
        let _ = self.save(&file);
        Some(file.title)
    }

    /// Forget a session. If it was current, the next most recent becomes current.
    pub fn delete(&self, id: &str) -> io::Result<Option<SessionFile>> {
        let path = self.file_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut index = self.read_index().unwrap_or_default();
        index.sessions.retain(|entry| entry.id != id);
        let switching = index.current == id;
        if switching {
            index.current = index
                .sessions
                .iter()
                .max_by_key(|entry| entry.saved_at)
                .map(|entry| entry.id.clone())
                .unwrap_or_default();
        }
        self.write_index(&index)?;
        if switching {
            if index.current.is_empty() {
                return Ok(Some(self.start_new()));
            }
            return Ok(self.load_id(&index.current));
        }
        Ok(self.load())
    }

    fn load_id(&self, id: &str) -> Option<SessionFile> {
        let text = std::fs::read_to_string(self.file_path(id)).ok()?;
        let mut file: SessionFile = serde_json::from_str(&text).ok()?;
        if file.version != SESSION_VERSION {
            return None;
        }
        if normalise(&file.workspace) != normalise(&self.workspace) {
            return None;
        }
        if file.id.is_empty() {
            file.id = id.to_string();
        }
        if file.title.is_empty() {
            file.refresh_title();
        }
        Some(file)
    }

    fn file_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn read_index(&self) -> Option<Index> {
        let text = std::fs::read_to_string(self.index_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_index(&self, index: &Index) -> io::Result<()> {
        write_private(&self.index_path(), index)
    }

    fn write_session(&self, file: &SessionFile) -> io::Result<()> {
        write_private(&self.file_path(&file.id), file)
    }

    fn touch_index(&self, file: &SessionFile) -> io::Result<()> {
        let mut index = self.read_index().unwrap_or_default();
        index.current = file.id.clone();
        if let Some(entry) = index.sessions.iter_mut().find(|entry| entry.id == file.id) {
            entry.title = file.title.clone();
            entry.saved_at = file.saved_at;
        } else {
            index.sessions.push(IndexEntry {
                id: file.id.clone(),
                title: file.title.clone(),
                saved_at: file.saved_at,
            });
        }
        self.write_index(&index)
    }

    fn scan(&self) -> Vec<SessionEntry> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                if name == "index.json" || !name.ends_with(".json") {
                    return None;
                }
                let id = name.trim_end_matches(".json");
                let file = self.load_id(id)?;
                Some(SessionEntry {
                    id: file.id,
                    title: file.title,
                    saved_at: file.saved_at,
                })
            })
            .collect()
    }
}

fn write_private<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text)?;
    set_private(&temporary)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn migrate_legacy(legacy: &Path, dir: &Path, workspace: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let text = std::fs::read_to_string(legacy)?;
    let mut file: SessionFile = serde_json::from_str(&text).map_err(io::Error::other)?;
    if file.id.is_empty() {
        file.id = uuid::Uuid::new_v4().to_string();
    }
    file.workspace = workspace.to_path_buf();
    file.refresh_title();
    let store = SessionStore::at(dir.to_path_buf(), workspace.to_path_buf());
    store.write_session(&file)?;
    store.touch_index(&file)?;
    let _ = std::fs::remove_file(legacy);
    Ok(())
}

/// The picker label for a transcript: first user line, shortened.
pub fn infer_title(messages: &[ChatMessage]) -> String {
    let Some(user) = messages.iter().find(|message| message.role == Role::User) else {
        return "untitled".to_string();
    };
    let line = user.content.lines().next().unwrap_or("").trim();
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "untitled".to_string();
    }
    let mut chars = collapsed.chars();
    let taken: String = chars.by_ref().take(TITLE_MAX).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// The file name for a workspace: a stable hash of its canonical path.
fn key_for(workspace: &Path) -> String {
    // FNV-1a, written out rather than taken from `DefaultHasher`, whose output
    // is explicitly not stable across Rust releases — a key that changes when
    // the toolchain changes would silently orphan every saved session.
    let canonical = normalise(workspace);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Seconds since the Unix epoch, or zero if the clock is before it.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// The path to compare and hash on: canonical when the filesystem can resolve
/// it, as given otherwise.
fn normalise(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    // On Windows the file inherits the user profile's ACLs, which is the right
    // scope already.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCall;

    fn store_in(dir: &Path, workspace: &Path) -> SessionStore {
        SessionStore::at(dir.to_path_buf(), workspace.to_path_buf())
    }

    fn sample() -> SessionFile {
        let mut file = SessionFile::new("/tmp/example");
        file.active_tier = Some("local".to_string());
        file.pinned_tier = Some("grok".to_string());
        file.sticky = false;
        file.on_stuck = Some(OnStuck::Consult);
        file.mode = Mode::Plan;
        file.cli_sessions
            .insert("grok".to_string(), "9f3c-ab".to_string());
        file.messages = vec![
            ChatMessage::user("make the tests pass"),
            ChatMessage::assistant(
                "",
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.rs"}"#.to_string(),
                }],
            ),
            ChatMessage::tool_result("call_1", "error[E0308]: mismatched types"),
        ];
        file
    }

    #[test]
    fn a_session_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        // The store keys on the real workspace, so make the contents agree.
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");

        let loaded = store.load().expect("the session should load");
        assert_eq!(loaded.active_tier.as_deref(), Some("local"));
        assert_eq!(loaded.pinned_tier.as_deref(), Some("grok"));
        assert!(!loaded.sticky);
        assert_eq!(loaded.on_stuck, Some(OnStuck::Consult));
        assert_eq!(loaded.mode, Mode::Plan);
        assert_eq!(
            loaded.cli_sessions.get("grok").map(String::as_str),
            Some("9f3c-ab")
        );
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2].content, "error[E0308]: mismatched types");
        assert_eq!(loaded.messages[1].tool_calls[0].name, "read_file");
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn a_missing_session_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/nowhere"));
        assert!(store.load().is_none());
    }

    #[test]
    fn a_corrupt_file_loads_as_no_session_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/example"));
        let file = store.start_new();
        std::fs::write(store.file_path(&file.id), "{ this is not json").expect("write");

        assert!(
            store.load().is_none(),
            "a damaged file must not stop the app from starting"
        );
    }

    #[test]
    fn a_session_from_another_format_version_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/example"));

        let mut file = sample();
        file.workspace = Path::new("/tmp/example").to_path_buf();
        store.save(&file).expect("save");
        let mut value = serde_json::to_value(&file).expect("to value");
        value["version"] = serde_json::json!(SESSION_VERSION + 1);
        std::fs::write(store.path(), value.to_string()).expect("write");

        assert!(store.load().is_none());
    }

    #[test]
    fn a_file_that_names_another_workspace_is_refused() {
        // Guards a hash collision, or a workspace renamed onto a key that
        // already belongs to something else. Written by hand so save() cannot
        // stamp the store's workspace over the lie.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/example"));
        let mut file = sample();
        file.workspace = Path::new("/tmp/somewhere-else").to_path_buf();
        file.id = "foreign".to_string();
        std::fs::create_dir_all(dir.path()).expect("mkdir");
        std::fs::write(
            store.file_path("foreign"),
            serde_json::to_string(&file).unwrap(),
        )
        .expect("write");
        let index = Index {
            current: "foreign".to_string(),
            sessions: vec![IndexEntry {
                id: "foreign".to_string(),
                title: "x".to_string(),
                saved_at: 1,
            }],
        };
        std::fs::write(store.index_path(), serde_json::to_string(&index).unwrap()).expect("index");

        assert!(store.load().is_none());
    }

    #[test]
    fn the_same_directory_always_gets_the_same_key_and_others_differ() {
        let one = key_for(Path::new("/home/someone/projects/foo"));
        let again = key_for(Path::new("/home/someone/projects/foo"));
        let other = key_for(Path::new("/home/someone/projects/bar"));

        assert_eq!(one, again);
        assert_ne!(one, other);
        assert_eq!(one.len(), 16, "a hex-encoded u64");
    }

    #[test]
    fn an_empty_named_session_is_kept_so_history_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut empty = SessionFile::new(workspace);
        empty.workspace = workspace.to_path_buf();
        empty.title = "notes".to_string();
        empty.named = true;
        store.save(&empty).expect("save empty");
        assert!(store.path().exists(), "the slot stays in the history");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");

        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            2,
            "the session and the index"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_session_file_is_private() {
        // A transcript holds whatever was discussed, so it is not readable by
        // anyone else on the machine.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");

        let mode = std::fs::metadata(store.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_second_session_joins_the_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut first = sample();
        first.workspace = workspace.to_path_buf();
        store.save(&first).expect("save");

        let second = store.start_new();
        assert_ne!(second.id, first.id);
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.current_id().as_deref(), Some(second.id.as_str()));
    }

    #[test]
    fn opening_a_session_makes_it_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut first = sample();
        first.workspace = workspace.to_path_buf();
        store.save(&first).expect("save");
        let first_id = store.current_id().expect("id");

        let second = store.start_new();
        store.open(&first_id).expect("open");
        assert_eq!(store.current_id().as_deref(), Some(first_id.as_str()));
        assert_ne!(store.current_id().as_deref(), Some(second.id.as_str()));
    }

    #[test]
    fn renaming_sticks_and_is_not_overwritten_by_the_first_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");
        store.rename("the parser").expect("rename");

        let mut loaded = store.load().expect("load");
        loaded
            .messages
            .insert(0, ChatMessage::user("something else"));
        store.save(&loaded).expect("save");
        assert_eq!(store.load().expect("load").title, "the parser");
    }

    #[test]
    fn infer_title_takes_the_first_user_line() {
        let messages = vec![
            ChatMessage::assistant("hi", Vec::new()),
            ChatMessage::user("make the tests pass\nand then ship"),
        ];
        assert_eq!(infer_title(&messages), "make the tests pass");
    }

    #[test]
    fn a_legacy_flat_file_is_migrated_into_the_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let key = key_for(workspace);
        let legacy = root.path().join(format!("{key}.json"));
        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        std::fs::write(&legacy, serde_json::to_string(&file).unwrap()).expect("write");

        let dir = root.path().join(&key);
        migrate_legacy(&legacy, &dir, workspace).expect("migrate");
        assert!(!legacy.exists());
        let store = SessionStore::at(dir, workspace.to_path_buf());
        let loaded = store.load().expect("migrated session");
        assert_eq!(loaded.messages.len(), 3);
        assert!(!store.list().is_empty());
    }

    #[test]
    fn the_on_disk_shape_is_the_documented_one() {
        // The file is a user-visible artifact, so its field names are part of
        // the contract rather than an implementation detail.
        let mut file = SessionFile::new("/tmp/example");
        file.active_tier = Some("local".to_string());
        let text = serde_json::to_string(&file).expect("serialize");

        for key in [
            "\"version\"",
            "\"workspace\"",
            "\"savedAt\"",
            "\"activeTier\"",
            "\"pinnedTier\"",
            "\"sticky\"",
            "\"onStuck\"",
            "\"mode\"",
            "\"cliSessions\"",
            "\"messages\"",
            "\"id\"",
            "\"title\"",
            "\"named\"",
        ] {
            assert!(text.contains(key), "{key} missing from {text}");
        }
    }
}
