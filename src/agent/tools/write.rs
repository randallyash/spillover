//! `write_file` — create or replace a file.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, object_schema, resolve};
use crate::agent::undo::Undo;

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write a whole file, creating it or replacing its existing contents. Parent directories \
         are created as needed. Use edit_file to change part of an existing file instead."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {
                    "type": "string",
                    "description": "Path to write, relative to the workspace. An absolute path is accepted only when it stays inside the workspace."
                },
                "content": {
                    "type": "string",
                    "description": "The complete new contents of the file."
                }
            }),
            &["path", "content"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn preview(&self, arguments: &Value, workspace: &Path) -> String {
        let args = Args::new(arguments);
        let Ok(raw) = args.required_str("path") else {
            return "write a file (path missing)".to_string();
        };
        let Ok(content) = args.required_text("content") else {
            return format!("write {raw} (content missing)");
        };
        let path = match resolve(workspace, raw) {
            Ok(path) => path,
            Err(error) => return error.content,
        };
        let incoming = content.len();

        match tokio::fs::metadata(&path).await {
            Ok(metadata) => format!(
                "overwrite {} ({} → {incoming} bytes)",
                path.display(),
                metadata.len()
            ),
            Err(_) => format!("create {} ({incoming} bytes)", path.display()),
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let args = Args::new(arguments);
        let raw = match args.required_str("path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = match args.required_text("content") {
            Ok(content) => content,
            Err(error) => return error,
        };
        let path = match resolve(workspace, raw) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = content.to_string();

        // Read before the write, which is the only moment the old bytes exist.
        // `content` is what will be there afterwards, so the fingerprint needs no
        // read-back.
        let undo = Undo::capture("write_file", path.clone(), content.as_bytes()).await;

        let previous = tokio::fs::metadata(&path).await.map(|m| m.len()).ok();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(error) = tokio::fs::create_dir_all(parent).await {
                    return ToolOutcome::io(
                        format!("could not create {}", parent.display()),
                        &error,
                    );
                }
            }
        }

        if let Err(error) = tokio::fs::write(&path, &content).await {
            return ToolOutcome::io(format!("could not write {}", path.display()), &error);
        }

        let verb = match previous {
            Some(bytes) => format!("replaced {bytes} bytes with"),
            None => "created".to_string(),
        };
        ToolOutcome::ok(format!(
            "{verb} {} bytes in {}",
            content.len(),
            path.display()
        ))
        // Only on a successful write: a write that failed changed nothing, and
        // offering to undo it would be offering to undo something that is not
        // there.
        .undoing(undo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_a_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile
            .run(&json!({"path": "new.txt", "content": "hello"}), dir.path())
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("created"), "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile
            .run(
                &json!({"path": "a/b/c/deep.txt", "content": "x"}),
                dir.path(),
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(dir.path().join("a/b/c/deep.txt").exists());
    }

    #[tokio::test]
    async fn replacing_a_file_reports_the_previous_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), "0123456789").expect("write");
        let outcome = WriteFile
            .run(&json!({"path": "f.txt", "content": "short"}), dir.path())
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(
            outcome.content.contains("replaced 10 bytes"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_empty_content_writes_an_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile
            .run(&json!({"path": "empty.txt", "content": ""}), dir.path())
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("empty.txt")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn missing_arguments_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile.run(&json!({"path": "x.txt"}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("content"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn the_preview_names_the_target_and_whether_it_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preview = WriteFile
            .preview(&json!({"path": "fresh.txt", "content": "abc"}), dir.path())
            .await;
        assert!(preview.contains("create"), "{preview}");
        assert!(preview.contains("3 bytes"), "{preview}");

        std::fs::write(dir.path().join("old.txt"), "12345").expect("write");
        let preview = WriteFile
            .preview(&json!({"path": "old.txt", "content": "abc"}), dir.path())
            .await;
        assert!(preview.contains("overwrite"), "{preview}");
        assert!(preview.contains("5 → 3 bytes"), "{preview}");
    }

    #[tokio::test]
    async fn writing_into_a_path_that_is_a_directory_fails_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("adir")).expect("mkdir");
        let outcome = WriteFile
            .run(&json!({"path": "adir", "content": "x"}), dir.path())
            .await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn a_successful_write_hands_back_the_means_to_reverse_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), "the original").expect("write");

        let outcome = WriteFile
            .run(&json!({"path": "f.txt", "content": "junk"}), dir.path())
            .await;

        let undo = outcome.undo.expect("a write should be reversible");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "junk"
        );
        undo.restore().await.expect("and the undo should work");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "the original"
        );
    }

    #[tokio::test]
    async fn a_created_file_is_remembered_as_having_had_nothing_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile
            .run(
                &json!({"path": "brand/new.txt", "content": "junk"}),
                dir.path(),
            )
            .await;

        let undo = outcome.undo.expect("even a creation can be undone");
        undo.restore().await.expect("undone");
        assert!(
            !dir.path().join("brand/new.txt").exists(),
            "the created file should be gone"
        );
        assert!(
            !dir.path().join("brand").exists(),
            "and so should the directory it created"
        );
    }

    #[tokio::test]
    async fn a_path_outside_the_workspace_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = WriteFile
            .run(
                &json!({"path": "/tmp/spill-outside.txt", "content": "nope"}),
                dir.path(),
            )
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("outside the workspace"),
            "{}",
            outcome.content
        );
        assert!(
            !std::path::Path::new("/tmp/spill-outside.txt").exists(),
            "must not have written outside the workspace"
        );
    }

    #[tokio::test]
    async fn a_write_that_failed_offers_nothing_to_undo() {
        // Nothing changed, so there is nothing to put back — and offering to
        // undo it would make `/undo` reach a write that never happened.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("adir")).expect("mkdir");

        let outcome = WriteFile
            .run(&json!({"path": "adir", "content": "x"}), dir.path())
            .await;

        assert!(outcome.is_error);
        assert!(outcome.undo.is_none(), "a failed write changed nothing");
    }
}
