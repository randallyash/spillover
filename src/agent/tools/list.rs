//! `list_dir` — list the entries of one directory.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, cap, object_schema, resolve};

/// Directories are shown first, so this is also a cap on how far down the
/// listing gets before it stops being useful.
const MAX_ENTRIES: usize = 500;

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory. Directories are marked with a trailing slash. Use this \
         to orient yourself before reading or searching files."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {
                    "type": "string",
                    "description": "Directory to list, relative to the workspace or absolute. \
                                    Defaults to the workspace root."
                }
            }),
            &[],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    async fn preview(&self, arguments: &Value, _workspace: &Path) -> String {
        match Args::new(arguments).optional_str("path") {
            Some(path) => format!("list {path}"),
            None => "list the workspace root".to_string(),
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let raw = Args::new(arguments).optional_str("path").unwrap_or(".");
        let path = resolve(workspace, raw);

        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(entries) => entries,
            Err(error) => {
                return ToolOutcome::error(format!("could not list {}: {error}", path.display()));
            }
        };

        let mut names: Vec<(bool, String)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_type = entry.file_type().await;
            let is_dir = file_type.map(|kind| kind.is_dir()).unwrap_or(false);
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if is_dir {
                name.push('/');
            }
            names.push((is_dir, name));
        }

        if names.is_empty() {
            return ToolOutcome::ok(format!("{} is empty", path.display()));
        }

        names.sort_by(|a, b| match (a.0, b.0) {
            // Directories first, then by name within each group.
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });

        let total = names.len();
        let mut out = String::new();
        for (_, name) in names.iter().take(MAX_ENTRIES) {
            out.push_str(name);
            out.push('\n');
        }
        if total > MAX_ENTRIES {
            out.push_str(&format!("… {} more entries\n", total - MAX_ENTRIES));
        }

        ToolOutcome::ok(cap(format!("{}/ ({total} entries)\n{out}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("README.md"), "hi").expect("write");
        std::fs::write(dir.path().join("Cargo.toml"), "x").expect("write");
        dir
    }

    #[tokio::test]
    async fn lists_entries_with_directories_first_and_marked() {
        let dir = fixture();
        let outcome = ListDir.run(&json!({}), dir.path()).await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("src/"), "{}", outcome.content);
        assert!(
            outcome.content.contains("Cargo.toml"),
            "{}",
            outcome.content
        );

        // Line 1 is the header, so the entries start on line 2.
        let first_entry = outcome.content.lines().nth(1).unwrap_or("");
        assert!(
            first_entry.ends_with('/'),
            "a directory should be listed first, got {first_entry:?} in:\n{}",
            outcome.content
        );
        assert_eq!(first_entry, "src/");
    }

    #[tokio::test]
    async fn reports_the_entry_count() {
        let dir = fixture();
        let outcome = ListDir.run(&json!({}), dir.path()).await;
        assert!(outcome.content.contains("3 entries"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn accepts_a_subdirectory() {
        let dir = fixture();
        std::fs::write(dir.path().join("src/main.rs"), "x").expect("write");
        let outcome = ListDir.run(&json!({"path": "src"}), dir.path()).await;
        assert!(outcome.content.contains("main.rs"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn a_missing_directory_is_a_tool_error() {
        let dir = fixture();
        let outcome = ListDir.run(&json!({"path": "nope"}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("could not list"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_empty_directory_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = ListDir.run(&json!({}), dir.path()).await;
        assert!(!outcome.is_error);
        assert!(outcome.content.contains("empty"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn a_file_instead_of_a_directory_is_a_tool_error() {
        let dir = fixture();
        let outcome = ListDir.run(&json!({"path": "README.md"}), dir.path()).await;
        assert!(outcome.is_error);
    }
}
