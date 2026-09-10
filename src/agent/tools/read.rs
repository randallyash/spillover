//! `read_file` — read a text file, with line numbers.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, cap, object_schema, resolve};

/// Lines returned when the caller does not say.
const DEFAULT_LIMIT: usize = 2_000;

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file from disk. Returns the contents with line numbers. Use offset and \
         limit for files that are too large to read at once."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace or absolute."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line to start from. Defaults to the first line."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return."
                }
            }),
            &["path"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    async fn preview(&self, arguments: &Value, _workspace: &Path) -> String {
        match Args::new(arguments).required_str("path") {
            Ok(path) => format!("read {path}"),
            Err(error) => error.content,
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let args = Args::new(arguments);
        let raw = match args.required_str("path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let path = resolve(workspace, raw);

        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                return ToolOutcome::error(format!("could not read {}: {error}", path.display()));
            }
        };

        // A NUL byte in the first chunk is the cheap and reliable signal that
        // this is a binary file, which is not useful to hand to a model.
        if text.contains('\0') {
            return ToolOutcome::error(format!(
                "{} looks like a binary file, so its contents are not returned",
                path.display()
            ));
        }

        let offset = args.optional_usize("offset").unwrap_or(1).max(1);
        let limit = args.optional_usize("limit").unwrap_or(DEFAULT_LIMIT).max(1);
        let total = text.lines().count();

        if offset > total && total > 0 {
            return ToolOutcome::error(format!(
                "offset {offset} is past the end of {} ({total} lines)",
                path.display()
            ));
        }

        let mut out = String::new();
        let mut shown = 0usize;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if number < offset {
                continue;
            }
            if shown == limit {
                break;
            }
            out.push_str(&format!("{number:>6}| {line}\n"));
            shown += 1;
        }

        if shown == 0 {
            return ToolOutcome::ok(format!("{} is empty", path.display()));
        }

        let first = offset;
        let last = offset + shown - 1;
        let mut header = format!("{} (lines {first}-{last} of {total})\n", path.display());
        if last < total {
            header.push_str(&format!(
                "… {} more lines; call read_file again with offset={}\n",
                total - last,
                last + 1
            ));
        }
        header.push_str(&out);
        ToolOutcome::ok(cap(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::Registry;

    fn workspace_with(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.txt"), contents).expect("write fixture");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn tool() -> ReadFile {
        ReadFile
    }

    #[tokio::test]
    async fn reads_a_file_with_line_numbers() {
        let (_dir, workspace) = workspace_with("alpha\nbeta\n");
        let outcome = tool().run(&json!({"path": "notes.txt"}), &workspace).await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("1| alpha"), "{}", outcome.content);
        assert!(outcome.content.contains("2| beta"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn reports_the_total_line_count() {
        let (_dir, workspace) = workspace_with("a\nb\nc\n");
        let outcome = tool().run(&json!({"path": "notes.txt"}), &workspace).await;
        assert!(outcome.content.contains("of 3"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn honours_offset_and_limit() {
        let (_dir, workspace) = workspace_with("one\ntwo\nthree\nfour\nfive\n");
        let outcome = tool()
            .run(
                &json!({"path": "notes.txt", "offset": 2, "limit": 2}),
                &workspace,
            )
            .await;
        assert!(outcome.content.contains("2| two"), "{}", outcome.content);
        assert!(outcome.content.contains("3| three"), "{}", outcome.content);
        assert!(!outcome.content.contains("4| four"), "{}", outcome.content);
        assert!(
            outcome.content.contains("offset=4"),
            "should point at the next line: {}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_as_a_tool_error() {
        let (_dir, workspace) = workspace_with("x");
        let outcome = tool().run(&json!({"path": "nope.txt"}), &workspace).await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("could not read"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_binary_file_is_refused_rather_than_dumped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("blob.bin"), b"ab\0cd").expect("write fixture");
        let outcome = tool().run(&json!({"path": "blob.bin"}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("binary"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn an_empty_file_says_so() {
        let (_dir, workspace) = workspace_with("");
        let outcome = tool().run(&json!({"path": "notes.txt"}), &workspace).await;
        assert!(!outcome.is_error);
        assert!(outcome.content.contains("empty"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_rejected() {
        let (_dir, workspace) = workspace_with("only\n");
        let outcome = tool()
            .run(&json!({"path": "notes.txt", "offset": 99}), &workspace)
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("past the end"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_missing_path_argument_is_reported() {
        let (_dir, workspace) = workspace_with("x");
        let outcome = tool().run(&json!({}), &workspace).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("path"), "{}", outcome.content);
    }

    #[test]
    fn is_declared_read_only() {
        assert_eq!(
            Registry::with_default_tools()
                .get("read_file")
                .unwrap()
                .risk(),
            Risk::Read
        );
    }
}
