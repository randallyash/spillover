//! `glob` — find files by path pattern.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::{Args, Risk, Tool, ToolOutcome, cap, object_schema, should_skip};

const MAX_MATCHES: usize = 500;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files whose path matches a glob pattern, for example \"**/*.rs\" or \"src/*.toml\". \
         Use this when you know the shape of a name but not where it lives."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern. ** matches across directories, * within one."
                }
            }),
            &["pattern"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    async fn preview(&self, arguments: &Value, _workspace: &Path) -> String {
        match Args::new(arguments).required_str("pattern") {
            Ok(pattern) => format!("find files matching {pattern}"),
            Err(error) => error.content,
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let pattern = match Args::new(arguments).required_str("pattern") {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };

        // Make the pattern absolute so it walks the workspace rather than the
        // process's working directory.
        let absolute = if Path::new(pattern).is_absolute() {
            pattern.to_string()
        } else {
            format!("{}/{}", workspace.display(), pattern)
        };

        let paths = match glob::glob(&absolute) {
            Ok(paths) => paths,
            Err(error) => {
                return ToolOutcome::error(format!("{pattern:?} is not a valid glob: {error}"));
            }
        };

        let mut matches: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        for entry in paths.flatten() {
            if entry
                .components()
                .any(|component| should_skip(&component.as_os_str().to_string_lossy()))
            {
                skipped += 1;
                continue;
            }
            matches.push(display_path(workspace, &entry));
        }

        if matches.is_empty() {
            return ToolOutcome::ok(format!(
                "no files match {pattern:?}{}",
                if skipped > 0 {
                    format!(" ({skipped} matches were inside ignored directories)")
                } else {
                    String::new()
                }
            ));
        }

        matches.sort();
        let total = matches.len();
        let mut out = matches
            .iter()
            .take(MAX_MATCHES)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if total > MAX_MATCHES {
            out.push_str(&format!("\n… {} more matches", total - MAX_MATCHES));
        }

        ToolOutcome::ok(cap(format!("{total} matches for {pattern:?}\n{out}")))
    }
}

fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths come back with the platform's own separator, so comparisons are
    /// made on a flattened copy. These tests are about which files matched, not
    /// about how the separator is spelled.
    fn flat(text: &str) -> String {
        text.replace('\\', "/")
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
        std::fs::write(dir.path().join("src/main.rs"), "x").expect("write");
        std::fs::write(dir.path().join("src/deep/lib.rs"), "x").expect("write");
        std::fs::write(dir.path().join("README.md"), "x").expect("write");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::write(dir.path().join("target/debug/junk.rs"), "x").expect("write");
        dir
    }

    #[tokio::test]
    async fn finds_files_across_directories() {
        let dir = fixture();
        let outcome = Glob.run(&json!({"pattern": "**/*.rs"}), dir.path()).await;
        assert!(!outcome.is_error, "{}", outcome.content);

        let content = flat(&outcome.content);
        assert!(content.contains("src/main.rs"), "{content}");
        assert!(content.contains("src/deep/lib.rs"), "{content}");
    }

    #[tokio::test]
    async fn skips_build_output_and_other_ignored_directories() {
        let dir = fixture();
        let outcome = Glob.run(&json!({"pattern": "**/*.rs"}), dir.path()).await;

        let content = flat(&outcome.content);
        assert!(
            !content.contains("target/"),
            "target/ should be ignored: {content}"
        );
        // And the check above is meaningful: the same run did find real matches.
        assert!(content.contains("src/main.rs"), "{content}");
    }

    #[tokio::test]
    async fn results_are_relative_to_the_workspace() {
        let dir = fixture();
        let outcome = Glob.run(&json!({"pattern": "src/*.rs"}), dir.path()).await;

        let content = flat(&outcome.content);
        let workspace = flat(&dir.path().display().to_string());
        assert!(
            !content.contains(&workspace),
            "results should be relative, not absolute: {content}"
        );
        assert!(content.contains("src/main.rs"), "{content}");
    }

    #[tokio::test]
    async fn no_matches_is_not_an_error() {
        let dir = fixture();
        let outcome = Glob.run(&json!({"pattern": "**/*.zig"}), dir.path()).await;
        assert!(!outcome.is_error);
        assert!(
            outcome.content.contains("no files match"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_invalid_pattern_is_a_tool_error() {
        let dir = fixture();
        let outcome = Glob.run(&json!({"pattern": "[unclosed"}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("valid glob"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_missing_pattern_is_reported() {
        let dir = fixture();
        let outcome = Glob.run(&json!({}), dir.path()).await;
        assert!(outcome.is_error);
    }
}
