//! Putting back what an approved write replaced.
//!
//! This is the capability that belongs to spill rather than to an agent harness:
//! every write already stops and shows a diff before it happens, so keeping the
//! bytes it was about to replace is a short step from there. It pairs with
//! spilling over — the cheap model wrote junk, you spilled, and now you want the
//! junk gone.
//!
//! Three things bound it, and each is deliberate:
//!
//! - **A stack, and a bounded one.** `/undo` reaches the last ten approved writes,
//!   newest first. A snapshot is a copy of a file, so what bounds it is the bytes
//!   as much as the count: whichever comes first drops the oldest write, and the
//!   newest is never dropped, because that is the one the command is for.
//! - **It refuses when the file has moved on.** The bytes are only put back if
//!   what is there is still what the write left, so `/undo` can never destroy
//!   work done since — including the user's own edit.
//! - **Not everything can be undone.** A write too large to keep a copy of says
//!   so rather than silently offering to restore an older write, and a shell
//!   command has no reversal at all.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// How many writes an undo can reach.
///
/// Ten, though the byte budget below is usually what decides.
const UNDO_DEPTH: usize = 10;

/// The most file contents the history may hold at once.
///
/// The reason this was a single entry to begin with. One snapshot is capped at
/// `MAX_SNAPSHOT`, but ten of them are not, so the budget is what actually bounds
/// the memory a session can spend on being able to undo.
const UNDO_BUDGET: usize = 8 * 1024 * 1024;

/// How large a file may be and still have a copy kept for an undo.
///
/// A snapshot is *retained* for the session, unlike the transient reads the file
/// tools already do, so it is capped. Past this the write is remembered as
/// unreversible rather than not remembered at all — otherwise `/undo` would
/// quietly offer to put back the write *before* it.
const MAX_SNAPSHOT: u64 = 2 * 1024 * 1024;

/// What was at the path before the write.
#[derive(Debug, Clone)]
pub enum Before {
    /// These were the bytes.
    Bytes(Vec<u8>),
    /// There was no file, so undoing means removing the one that was created.
    Nothing,
    /// There was something, but no copy of it was kept — with the reason, so a
    /// refusal can say why rather than pretending there was nothing there.
    Unavailable(String),
}

/// A write that can be put back.
#[derive(Debug, Clone)]
pub struct Undo {
    /// The tool that made the change, for saying what is being reversed.
    tool: String,
    path: PathBuf,
    before: Before,
    /// Directories the write created, deepest first, removed on the way back out
    /// but only while empty.
    created_dirs: Vec<PathBuf>,
    /// A fingerprint of what the write left behind, so an undo can tell whether
    /// the file has moved on since.
    after: u64,
}

impl Undo {
    /// How many bytes of file contents this entry is holding on to.
    ///
    /// What the budget counts. An entry that kept no copy — a file that was
    /// created, or one too large to snapshot — is holding nothing.
    pub fn held_bytes(&self) -> usize {
        match &self.before {
            Before::Bytes(bytes) => bytes.len(),
            Before::Nothing | Before::Unavailable(_) => 0,
        }
    }

    /// Record what is at `path` now, for a write that is about to replace it.
    ///
    /// Read before the write rather than after: afterwards the old bytes are gone,
    /// and this is the only moment they exist.
    pub async fn capture(tool: impl Into<String>, path: PathBuf, updated: &[u8]) -> Self {
        let before = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.len() > MAX_SNAPSHOT => Before::Unavailable(format!(
                "it is {} bytes and a copy is only kept up to {MAX_SNAPSHOT}",
                metadata.len()
            )),
            Ok(_) => match tokio::fs::read(&path).await {
                Ok(bytes) => Before::Bytes(bytes),
                Err(error) => Before::Unavailable(format!("it could not be read: {error}")),
            },
            // Nothing there: the write is creating it, and undoing removes it.
            Err(_) => Before::Nothing,
        };

        let created_dirs = missing_ancestors(&path).await;
        Self {
            tool: tool.into(),
            path,
            before,
            created_dirs,
            after: fingerprint(updated),
        }
    }

    /// Record a change whose before and after bytes are both already known.
    ///
    /// For a tool that had to read the file to do its job — `edit_file` reads it
    /// to find the text it replaces — so capturing costs no extra IO at all.
    pub fn from_previous(
        tool: impl Into<String>,
        path: PathBuf,
        previous: &[u8],
        updated: &[u8],
    ) -> Self {
        Self {
            tool: tool.into(),
            path,
            before: Before::Bytes(previous.to_vec()),
            // An edit cannot create the file it edited, so there is nothing to
            // tidy up on the way back out.
            created_dirs: Vec::new(),
            after: fingerprint(updated),
        }
    }

    /// Put it back, or say why it cannot be.
    ///
    /// The `Err` is a sentence for the transcript, not a bug report: every way
    /// this can refuse is a normal thing that happened, and each says what and
    /// what to do instead.
    pub async fn restore(&self) -> Result<String, String> {
        let path = self.path.display();

        if let Before::Unavailable(reason) = &self.before {
            return Err(format!(
                "could not undo the {} on {path}: {reason}. Nothing was changed.",
                self.tool
            ));
        }

        // The safety rule. Whatever is there now has to still be what the write
        // left, or putting the old bytes back would silently discard anything
        // done since — an edit the user made by hand, or another tool's work.
        match tokio::fs::read(&self.path).await {
            Ok(current) if fingerprint(&current) == self.after => {}
            Ok(_) => return Err(changed(&path)),
            // Gone entirely is also moved on: re-creating a file somebody
            // deliberately deleted is not what "undo that write" means.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(changed(&path));
            }
            Err(error) => {
                return Err(format!(
                    "could not read {path} to undo the {}: {error}. Nothing was changed.",
                    self.tool
                ));
            }
        }

        match &self.before {
            Before::Bytes(previous) => {
                if let Err(error) = tokio::fs::write(&self.path, previous).await {
                    return Err(format!("could not restore {path}: {error}"));
                }
                Ok(format!(
                    "restored {path} ({} bytes, as it was before the {})",
                    previous.len(),
                    self.tool
                ))
            }
            Before::Nothing => {
                if let Err(error) = tokio::fs::remove_file(&self.path).await {
                    return Err(format!("could not remove {path}: {error}"));
                }
                let tidied = remove_created_dirs(&self.created_dirs).await;
                let mut said = format!("removed {path} (created by the {})", self.tool);
                if tidied > 0 {
                    said.push_str(&format!(
                        ", and the {} director{} it created",
                        tidied,
                        if tidied == 1 { "y" } else { "ies" }
                    ));
                }
                Ok(said)
            }
            Before::Unavailable(_) => unreachable!("checked above"),
        }
    }
}

fn changed(path: impl std::fmt::Display) -> String {
    format!("{path} has changed since that write — leaving it alone. Nothing was changed.")
}

/// The directories on the way to `path` that do not exist yet, deepest first.
///
/// Deepest first is the order they were created, and so the order they can be
/// removed. Stops at the first one that is already there: everything above it
/// existed before this write and is not ours to touch.
async fn missing_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || tokio::fs::metadata(dir).await.is_ok() {
            break;
        }
        missing.push(dir.to_path_buf());
        current = dir.parent();
    }
    missing
}

/// Remove the directories a write created, while they are still empty.
///
/// Only while empty: a directory that acquired something else in the meantime is
/// somebody's, and deleting it would be the opposite of tidying up.
async fn remove_created_dirs(dirs: &[PathBuf]) -> usize {
    let mut removed = 0;
    for dir in dirs {
        // `remove_dir` refuses a non-empty directory on every platform, which is
        // exactly the guard wanted here — no separate emptiness check to race.
        if tokio::fs::remove_dir(dir).await.is_ok() {
            removed += 1;
        } else {
            // A directory that could not be removed means everything above it is
            // still needed too.
            break;
        }
    }
    removed
}

/// A cheap fingerprint of some bytes, for telling one version of a file from
/// another.
///
/// FNV-1a. Deliberately not shared with the identical loop in `session_store`:
/// that one names a file on disk and so must be stable across releases, where
/// this one only has to be self-consistent for the life of a process. Coupling
/// them would buy nothing and tie two unrelated things together.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The writes an undo can still reach, newest first.
///
/// A stack rather than a single slot, so `/undo` reaches back past the mistake you
/// just made to the one before it. Bounded twice — by a count and by the bytes it
/// holds — and whichever bound is reached first drops the *oldest* entry, because
/// the newest writes are the ones anybody wants to reach for.
///
/// Every entry carries its own path and its own fingerprint of what the write left
/// behind, so the rule that an entry may only be put back while its file is
/// untouched holds per entry, exactly as it did when there was one of them.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: VecDeque<Box<Undo>>,
    held: usize,
}

impl UndoStack {
    /// Record a write. The newest is the one `/undo` reaches first.
    pub fn push(&mut self, undo: Undo) {
        self.held += undo.held_bytes();
        self.entries.push_front(Box::new(undo));
        self.trim();
    }

    /// Take the newest entry, if there is one.
    pub fn pop(&mut self) -> Option<Box<Undo>> {
        let undo = self.entries.pop_front()?;
        self.held = self.held.saturating_sub(undo.held_bytes());
        Some(undo)
    }

    /// Put an entry back at the front.
    ///
    /// For a refusal, which changes nothing: the entry is still the newest write
    /// that could be put back, so it belongs where it was rather than at the
    /// bottom of the stack.
    pub fn restore(&mut self, undo: Box<Undo>) {
        self.held += undo.held_bytes();
        self.entries.push_front(undo);
        self.trim();
    }

    /// How many writes an undo would reach from here.
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    /// Drop the oldest entries while either bound is exceeded.
    fn trim(&mut self) {
        // Never the entry just recorded: a bound that could throw away the write
        // it was just handed would turn "bounded" into "silently unreversible".
        // The budget is four times `MAX_SNAPSHOT`, so one entry cannot reach it.
        while self.entries.len() > UNDO_DEPTH || (self.held > UNDO_BUDGET && self.entries.len() > 1)
        {
            let Some(oldest) = self.entries.pop_back() else {
                break;
            };
            self.held = self.held.saturating_sub(oldest.held_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.txt");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[tokio::test]
    async fn a_replaced_file_gets_its_old_bytes_back() {
        let (_dir, path) = store("the original contents\n");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;

        std::fs::write(&path, "junk").expect("the write being undone");

        let said = undo.restore().await.expect("it should restore");
        assert!(said.contains("restored"), "{said}");
        assert!(said.contains("write_file"), "{said}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the original contents\n"
        );
    }

    #[tokio::test]
    async fn a_created_file_is_removed_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.txt");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;
        std::fs::write(&path, "junk").expect("the write being undone");

        let said = undo.restore().await.expect("it should restore");
        assert!(said.contains("removed"), "{said}");
        assert!(!path.exists(), "the junk file should be gone");
    }

    #[tokio::test]
    async fn the_directories_a_write_created_go_too() {
        // Otherwise the junk is half gone: the file is removed and an empty tree
        // is left standing where it was.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/b/c/deep.txt");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;

        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "junk").expect("the write being undone");

        undo.restore().await.expect("it should restore");

        assert!(!path.exists());
        assert!(
            !dir.path().join("a").exists(),
            "the whole tree the write created should be gone"
        );
    }

    #[tokio::test]
    async fn a_directory_that_acquired_something_else_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/deep.txt");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;

        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "junk").expect("the write being undone");
        // Somebody put something else in the directory in the meantime.
        std::fs::write(dir.path().join("a/keep.txt"), "not mine to delete").expect("write");

        undo.restore().await.expect("it should restore");

        assert!(!path.exists());
        assert!(
            dir.path().join("a/keep.txt").exists(),
            "a directory with something else in it is not ours to remove"
        );
    }

    #[tokio::test]
    async fn an_edited_file_is_refused_rather_than_overwritten() {
        // The rule that protects the user's own work. The write happened, then
        // somebody changed the file again: putting the old bytes back would
        // silently discard whatever they did.
        let (_dir, path) = store("original\n");
        let undo = Undo::capture("write_file", path.clone(), b"the model's junk").await;
        std::fs::write(&path, "the model's junk").expect("the write being undone");
        std::fs::write(&path, "the user's own edit").expect("edited by hand");

        let refused = undo.restore().await.expect_err("it must refuse");
        assert!(refused.contains("has changed since"), "{refused}");
        assert!(refused.contains("Nothing was changed"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the user's own edit",
            "the refusal must leave the file exactly as it was"
        );
    }

    #[tokio::test]
    async fn a_file_deleted_since_is_refused_too() {
        // Re-creating a file somebody deliberately removed is not what "undo that
        // write" means, and guessing either way would be worse than saying so.
        let (_dir, path) = store("original\n");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;
        std::fs::write(&path, "junk").expect("the write being undone");
        std::fs::remove_file(&path).expect("deleted by hand");

        let refused = undo.restore().await.expect_err("it must refuse");
        assert!(refused.contains("has changed since"), "{refused}");
        assert!(!path.exists(), "and it must not re-create the file");
    }

    #[tokio::test]
    async fn a_file_too_large_to_copy_says_so_instead_of_offering_an_older_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![b'x'; (MAX_SNAPSHOT + 1) as usize]).expect("write");

        let undo = Undo::capture("write_file", path.clone(), b"junk").await;
        std::fs::write(&path, "junk").expect("the write being undone");

        let refused = undo.restore().await.expect_err("it cannot reverse this");
        assert!(refused.contains("could not undo"), "{refused}");
        assert!(
            refused.contains("copy is only kept"),
            "it should say why: {refused}"
        );
    }

    #[tokio::test]
    async fn a_second_restore_is_refused_because_there_is_nothing_left_to_put_back() {
        // One write, one undo. Running it twice must not flip the file back and
        // forth, and must not claim success the second time.
        let (_dir, path) = store("original\n");
        let undo = Undo::capture("write_file", path.clone(), b"junk").await;
        std::fs::write(&path, "junk").expect("the write being undone");

        undo.restore().await.expect("the first should restore");
        let again = undo
            .restore()
            .await
            .expect_err("the second has nothing to do");
        assert!(again.contains("has changed since"), "{again}");
    }

    #[tokio::test]
    async fn an_unreadable_file_is_reported_rather_than_assumed_absent() {
        // `Before::Unavailable` exists for the cases where a copy could not be
        // kept: pretending there was nothing there would make `/undo` delete a
        // file it never wrote.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.txt");
        let undo = Undo {
            tool: "write_file".to_string(),
            path: path.clone(),
            before: Before::Unavailable("it could not be read: nope".to_string()),
            created_dirs: Vec::new(),
            after: 0,
        };

        let refused = undo.restore().await.expect_err("it must refuse");
        assert!(refused.contains("could not be read"), "{refused}");
        assert!(!path.exists(), "and it must not touch anything");
    }

    #[test]
    fn a_fingerprint_tells_two_versions_apart() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_ne!(fingerprint(b""), fingerprint(b"a"));
    }

    // ---- the stack ----------------------------------------------------------

    /// An entry with no file behind it, for the bounds: only the held bytes and
    /// the name matter to a stack.
    fn entry(tool: &str, held: usize) -> Undo {
        Undo {
            tool: tool.to_string(),
            path: PathBuf::from(format!("/tmp/{tool}")),
            before: if held == 0 {
                Before::Nothing
            } else {
                Before::Bytes(vec![b'x'; held])
            },
            created_dirs: Vec::new(),
            after: 0,
        }
    }

    fn names(stack: &mut UndoStack) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(undo) = stack.pop() {
            out.push(undo.tool.clone());
        }
        out
    }

    #[test]
    fn an_undo_reaches_the_newest_write_first() {
        let mut stack = UndoStack::default();
        assert_eq!(stack.depth(), 0);

        stack.push(entry("first", 0));
        stack.push(entry("second", 0));

        assert_eq!(stack.depth(), 2);
        assert_eq!(names(&mut stack), vec!["second", "first"]);
        assert_eq!(stack.depth(), 0, "one undo reaches one write");
    }

    #[test]
    fn only_the_newest_writes_are_kept() {
        // A bound rather than a history: the eleventh write is reachable and the
        // first is not, because a stack deep enough to matter is a stack nobody
        // keeps a mental model of.
        let mut stack = UndoStack::default();
        for index in 0..UNDO_DEPTH + 3 {
            stack.push(entry(&format!("write{index}"), 0));
        }

        assert_eq!(stack.depth(), UNDO_DEPTH);
        let reached = names(&mut stack);
        assert_eq!(
            reached.first().map(String::as_str),
            Some("write12"),
            "newest first"
        );
        assert_eq!(
            reached.last().map(String::as_str),
            Some("write3"),
            "oldest kept"
        );
    }

    #[test]
    fn the_bytes_a_write_held_decide_the_bound_too() {
        // Four entries of a quarter of the budget each: the fifth does not fit,
        // and the oldest goes rather than the one just recorded.
        let quarter = UNDO_BUDGET / 4;
        let mut stack = UndoStack::default();
        for index in 0..4 {
            stack.push(entry(&format!("write{index}"), quarter));
        }
        assert_eq!(stack.depth(), 4);

        stack.push(entry("write4", quarter));

        let reached = names(&mut stack);
        assert_eq!(reached.first().map(String::as_str), Some("write4"));
        assert_eq!(
            reached.last().map(String::as_str),
            Some("write1"),
            "the oldest went, not the newest: {reached:?}"
        );
    }

    #[test]
    fn an_entry_holding_nothing_costs_nothing() {
        // A created file and an unsnapshotable one both keep no copy, so a stack
        // of them is bounded by the count alone.
        assert_eq!(entry("created", 0).held_bytes(), 0);
        assert_eq!(
            Undo {
                tool: "big".to_string(),
                path: PathBuf::from("/tmp/big"),
                before: Before::Unavailable("too large".to_string()),
                created_dirs: Vec::new(),
                after: 0,
            }
            .held_bytes(),
            0
        );

        let mut stack = UndoStack::default();
        for index in 0..UNDO_DEPTH {
            stack.push(entry(&format!("write{index}"), 0));
        }
        assert_eq!(stack.depth(), UNDO_DEPTH, "no bytes, so no eviction");
    }

    #[test]
    fn a_refusal_puts_the_entry_back_where_it_was() {
        // What `/undo` does when the file has moved on: nothing was restored, so
        // the entry is still the newest one to reach for.
        let mut stack = UndoStack::default();
        stack.push(entry("first", 0));
        stack.push(entry("second", 0));

        let taken = stack.pop().expect("the newest");
        assert_eq!(taken.tool, "second");
        stack.restore(taken);

        assert_eq!(stack.depth(), 2);
        assert_eq!(
            names(&mut stack),
            vec!["second", "first"],
            "order is unchanged"
        );
    }

    #[tokio::test]
    async fn an_entry_is_only_put_back_while_its_own_file_is_untouched() {
        // The invariant, with more than one entry to get it wrong across: each
        // carries its own fingerprint, so a file that moved on is refused without
        // saying anything about the entry beneath it.
        let (_dir, first) = store("first version");
        let (_dir2, second) = store("second version");

        let mut stack = UndoStack::default();
        stack.push(Undo::capture("edit_file", first.clone(), b"first edited").await);
        stack.push(Undo::capture("edit_file", second.clone(), b"second edited").await);

        std::fs::write(&second, "second edited").expect("the write lands");
        std::fs::write(&first, "first edited").expect("the write lands");

        // The newest restores, and the one beneath it still has its own check.
        stack
            .pop()
            .expect("the newest")
            .restore()
            .await
            .expect("untouched");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second version");

        let next = stack.pop().expect("the one beneath");
        std::fs::write(&first, "somebody else's work").expect("changed since");
        assert!(next.restore().await.is_err(), "refused");
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "somebody else's work",
            "and left alone"
        );

        // And it goes back so the user can revert by hand and try again.
        stack.restore(next);
        assert_eq!(stack.depth(), 1);
    }
}
