//! The conversation that outlives the process.
//!
//! A TUI agent that forgets which tier it had settled on, what it had been told
//! to do when a tier stalled, and what it had already said is a toy: every
//! restart is a cold start, and the work of the previous session has to be
//! explained again. This module is the file that prevents that.
//!
//! Three things are stored, and each one is here for a reason:
//!
//! - **The transcript**, inline rather than behind a pointer. A pointer would be
//!   smaller, but an `openai` tier has no provider-side conversation to point at
//!   — chat completions is stateless, so the whole history is re-sent every turn
//!   — which means a pointer-only design would remember *nothing* for the local
//!   model this program is built around. The transcript is the only record that
//!   exists for those tiers, so it is the record we keep.
//! - **The chain state**: which tier is answering, whether one was pinned, the
//!   sticky choice, and any session-level stuck policy. This is the part a user
//!   notices first, because it is what the rail and the session panel draw.
//! - **The CLI session ids**, keyed by tier id. A resumed id means the CLI
//!   continues its own conversation and is sent only the new turn, instead of
//!   the whole transcript being flattened into its prompt again.
//!
//! It is written by the agent, which is the only thing that can see all of it,
//! and read by `main` before the terminal is taken over, so a damaged file is
//! reported as ordinary output instead of corrupting the interface.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::Mode;
use crate::config::OnStuck;
use crate::fallback::ChainState;
use crate::session::ChatMessage;

/// The format this build writes. A file from another version is ignored rather
/// than guessed at: resuming a conversation under rules it was not recorded
/// under is worse than starting over.
pub const SESSION_VERSION: u32 = 1;

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
        }
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

/// The one session belonging to one workspace directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    workspace: PathBuf,
}

impl SessionStore {
    /// The store for a workspace, under the platform's state directory.
    pub fn for_workspace(workspace: &Path) -> Result<Self, StoreError> {
        let dirs = directories::ProjectDirs::from("", "", "spill").ok_or(StoreError::NoStateDir)?;
        let dir = dirs
            .state_dir()
            .ok_or(StoreError::NoStateDir)?
            .join("sessions");
        Ok(Self::at(
            dir.join(format!("{}.json", key_for(workspace))),
            workspace.to_path_buf(),
        ))
    }

    /// A store at an explicit path, for tests and for a caller that has already
    /// worked out where the file belongs.
    pub fn at(path: PathBuf, workspace: PathBuf) -> Self {
        Self { path, workspace }
    }

    /// Where this session is written, for saying so when it cannot be.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the session, or `None` when there is not a usable one.
    pub fn load(&self) -> Option<SessionFile> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let file: SessionFile = serde_json::from_str(&text).ok()?;
        if file.version != SESSION_VERSION {
            return None;
        }
        // The file name is a hash, so confirm the contents agree with it. A
        // collision, or a workspace that has since been renamed onto this key,
        // must not resume somebody else's conversation.
        if normalise(&file.workspace) != normalise(&self.workspace) {
            return None;
        }
        Some(file)
    }

    /// Write the session, atomically.
    pub fn save(&self, file: &SessionFile) -> io::Result<()> {
        if file.is_empty() {
            return self.clear();
        }

        let Some(parent) = self.path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the session file has no parent directory",
            ));
        };
        std::fs::create_dir_all(parent)?;

        let text = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        set_private(&temporary)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    /// Forget this workspace's session. A missing file is not an error.
    pub fn clear(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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
        SessionStore::at(dir.join("session.json"), workspace.to_path_buf())
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
        std::fs::write(store.path(), "{ this is not json").expect("write");

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
        let mut value = serde_json::to_value(&file).expect("to value");
        value["version"] = serde_json::json!(SESSION_VERSION + 1);
        std::fs::write(store.path(), value.to_string()).expect("write");

        assert!(store.load().is_none());
    }

    #[test]
    fn a_file_that_names_another_workspace_is_refused() {
        // Guards a hash collision, or a workspace renamed onto a key that
        // already belongs to something else.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/example"));

        let mut file = sample();
        file.workspace = Path::new("/tmp/somewhere-else").to_path_buf();
        store.save(&file).expect("save");

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
    fn an_empty_session_removes_the_file_instead_of_writing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");
        assert!(store.path().exists());

        let mut empty = SessionFile::new(workspace);
        empty.workspace = workspace.to_path_buf();
        store.save(&empty).expect("save empty");
        assert!(
            !store.path().exists(),
            "a cleared session should leave nothing to resume"
        );
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Path::new("/tmp/example");
        let store = store_in(dir.path(), workspace);

        let mut file = sample();
        file.workspace = workspace.to_path_buf();
        store.save(&file).expect("save");

        assert!(!store.path().with_extension("json.tmp").exists());
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            1,
            "only the session file should be there"
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
    fn clearing_a_session_that_is_not_there_is_fine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(dir.path(), Path::new("/tmp/example"));
        store.clear().expect("clearing nothing should succeed");
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
        ] {
            assert!(text.contains(key), "{key} missing from {text}");
        }
    }
}
