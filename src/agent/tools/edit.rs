//! `edit_file` — replace an exact string inside a file.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, object_schema, resolve};

/// How many changed lines the preview will show.
const MAX_PREVIEW_LINES: usize = 24;

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a file. old_string must match the file exactly, including \
         indentation, and must appear only once unless replace_all is set — include surrounding \
         lines to make it unique. Returns an error if the text is not found."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": {
                    "type": "string",
                    "description": "Path to the file to change."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "Text to replace it with. May be empty to delete."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match."
                }
            }),
            &["path", "old_string", "new_string"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn preview(&self, arguments: &Value, workspace: &Path) -> String {
        let args = Args::new(arguments);
        let (Ok(raw), Ok(old), Ok(new)) = (
            args.required_str("path"),
            args.required_text("old_string"),
            args.required_text("new_string"),
        ) else {
            return "edit a file (arguments incomplete)".to_string();
        };
        let path = resolve(workspace, raw);

        let occurrences = match tokio::fs::read_to_string(&path).await {
            Ok(text) => count_matches(&text, old),
            Err(error) => return format!("edit {} — cannot read it: {error}", path.display()),
        };

        let mut preview = format!(
            "edit {} — {} occurrence{}",
            path.display(),
            occurrences,
            if occurrences == 1 { "" } else { "s" }
        );
        if args.optional_bool("replace_all") && occurrences > 1 {
            preview.push_str(" (replace_all)");
        }
        preview.push('\n');
        preview.push_str(&render_change(old, new));
        preview
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let args = Args::new(arguments);
        let raw = match args.required_str("path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let old = match args.required_text("old_string") {
            Ok(old) => old,
            Err(error) => return error,
        };
        let new = match args.required_text("new_string") {
            Ok(new) => new,
            Err(error) => return error,
        };
        let replace_all = args.optional_bool("replace_all");
        let path = resolve(workspace, raw);

        // An empty search string would match everywhere and corrupt the file.
        if old.is_empty() {
            return ToolOutcome::error(
                "old_string must not be empty; give the text you want to replace",
            );
        }
        if old == new {
            return ToolOutcome::error(
                "old_string and new_string are identical, so there is nothing to change",
            );
        }

        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                return ToolOutcome::error(format!("could not read {}: {error}", path.display()));
            }
        };

        let occurrences = count_matches(&text, old);
        if occurrences == 0 {
            return ToolOutcome::error(format!(
                "old_string was not found in {}; read the file and match its text exactly, \
                 including indentation",
                path.display()
            ));
        }
        if occurrences > 1 && !replace_all {
            return ToolOutcome::error(format!(
                "old_string appears {occurrences} times in {}; include more surrounding text to \
                 make it unique, or pass replace_all = true",
                path.display()
            ));
        }

        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };

        if let Err(error) = tokio::fs::write(&path, &updated).await {
            return ToolOutcome::error(format!("could not write {}: {error}", path.display()));
        }

        ToolOutcome::ok(format!(
            "replaced {occurrences} occurrence{} in {} ({} → {} bytes)",
            if occurrences == 1 { "" } else { "s" },
            path.display(),
            text.len(),
            updated.len()
        ))
    }
}

fn count_matches(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// A compact before/after view, since a line diff is the readable form here.
fn render_change(old: &str, new: &str) -> String {
    let mut out = String::new();
    for (index, line) in old.lines().enumerate() {
        if index == MAX_PREVIEW_LINES {
            out.push_str("- …\n");
            break;
        }
        out.push_str(&format!("- {line}\n"));
    }
    if old.is_empty() {
        out.push_str("- (nothing)\n");
    }
    for (index, line) in new.lines().enumerate() {
        if index == MAX_PREVIEW_LINES {
            out.push_str("+ …\n");
            break;
        }
        out.push_str(&format!("+ {line}\n"));
    }
    if new.is_empty() {
        out.push_str("+ (nothing)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), contents).expect("write");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let (_dir, workspace) = fixture("alpha\nbeta\ngamma\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "beta", "new_string": "BETA"}),
                &workspace,
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(workspace.join("f.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn reports_how_many_occurrences_changed() {
        let (_dir, workspace) = fixture("x\nx\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &workspace,
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(
            outcome.content.contains("replaced 2 occurrences"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn refuses_an_ambiguous_match_without_replace_all() {
        let (_dir, workspace) = fixture("x\nx\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
                &workspace,
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("2 times"), "{}", outcome.content);
        assert!(
            outcome.content.contains("replace_all"),
            "the error should say how to proceed: {}",
            outcome.content
        );
        // The file must be untouched.
        assert_eq!(
            std::fs::read_to_string(workspace.join("f.txt")).unwrap(),
            "x\nx\n"
        );
    }

    #[tokio::test]
    async fn a_missing_match_is_a_usable_error() {
        let (_dir, workspace) = fixture("alpha\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
                &workspace,
            )
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("was not found"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_empty_old_string_is_refused() {
        let (_dir, workspace) = fixture("alpha\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "", "new_string": "y"}),
                &workspace,
            )
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("must not be empty"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_unchanged_edit_is_refused() {
        let (_dir, workspace) = fixture("alpha\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "alpha", "new_string": "alpha"}),
                &workspace,
            )
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("identical"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn an_empty_new_string_deletes_the_text() {
        let (_dir, workspace) = fixture("keep\nremove me\nkeep2\n");
        let outcome = EditFile
            .run(
                &json!({"path": "f.txt", "old_string": "remove me\n", "new_string": ""}),
                &workspace,
            )
            .await;
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(workspace.join("f.txt")).unwrap(),
            "keep\nkeep2\n"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_a_tool_error() {
        let (_dir, workspace) = fixture("x");
        let outcome = EditFile
            .run(
                &json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
                &workspace,
            )
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("could not read"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn the_preview_shows_the_change_before_it_happens() {
        let (_dir, workspace) = fixture("before\n");
        let preview = EditFile
            .preview(
                &json!({"path": "f.txt", "old_string": "before", "new_string": "after"}),
                &workspace,
            )
            .await;
        assert!(preview.contains("- before"), "{preview}");
        assert!(preview.contains("+ after"), "{preview}");
        assert!(preview.contains("1 occurrence"), "{preview}");
        // Nothing should have been written by a preview.
        assert_eq!(
            std::fs::read_to_string(workspace.join("f.txt")).unwrap(),
            "before\n"
        );
    }

    #[test]
    fn an_empty_search_never_counts_as_a_match() {
        assert_eq!(count_matches("abc", ""), 0);
        assert_eq!(count_matches("aaa", "a"), 3);
    }
}
