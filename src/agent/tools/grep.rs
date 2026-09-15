//! `grep` — search file contents with a regular expression.

use std::path::Path;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::agent::tools::{
    Args, Risk, Tool, ToolOutcome, cap, object_schema, resolve, should_skip,
};

const MAX_MATCHES: usize = 200;
/// Files larger than this are skipped: they are almost always data, not source.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression, recursively. Returns matching lines as \
         path:line: text. Use the glob argument to restrict which files are searched, for example \
         \"*.rs\". Build output and version control directories are skipped."
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the workspace. An absolute path is accepted only when it stays inside the workspace. Defaults to the workspace root."
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files whose name matches this glob, e.g. \"*.toml\"."
                }
            }),
            &["pattern"],
        )
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    async fn preview(&self, arguments: &Value, _workspace: &Path) -> String {
        let args = Args::new(arguments);
        match args.required_str("pattern") {
            Ok(pattern) => match args.optional_str("path") {
                Some(path) => format!("search for {pattern} under {path}"),
                None => format!("search for {pattern}"),
            },
            Err(error) => error.content,
        }
    }

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome {
        let args = Args::new(arguments);
        let pattern = match args.required_str("pattern") {
            Ok(pattern) => pattern.to_string(),
            Err(error) => return error,
        };
        let root = match resolve(workspace, args.optional_str("path").unwrap_or(".")) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let file_glob = args.optional_str("glob").map(str::to_string);
        let workspace = workspace.to_path_buf();

        // Walking a tree is blocking work; keeping it off the runtime thread
        // means the UI keeps redrawing while a big search runs.
        let search = tokio::task::spawn_blocking(move || {
            search_tree(&root, &workspace, &pattern, file_glob.as_deref())
        })
        .await;

        match search {
            Ok(Ok(report)) => ToolOutcome::ok(cap(report)),
            Ok(Err(message)) => ToolOutcome::error(message),
            Err(error) => ToolOutcome::error(format!("the search task failed: {error}")),
        }
    }
}

fn search_tree(
    root: &Path,
    workspace: &Path,
    pattern: &str,
    file_glob: Option<&str>,
) -> Result<String, String> {
    let regex = Regex::new(pattern)
        .map_err(|error| format!("{pattern:?} is not a valid regular expression: {error}"))?;

    let name_filter = match file_glob {
        Some(glob) => Some(
            glob::Pattern::new(glob)
                .map_err(|error| format!("{glob:?} is not a valid file glob: {error}"))?,
        ),
        None => None,
    };

    // WalkDir accepts a file root as well as a directory, so searching a single
    // file needs no special case.
    let walker = WalkDir::new(root).into_iter();

    let mut hits: Vec<String> = Vec::new();
    let mut files_searched = 0usize;
    let mut hit_limit = false;

    for entry in walker.filter_entry(|entry| {
        // Never skip the root itself: it may legitimately live inside a
        // directory whose name we ignore when descending.
        if entry.path() == root {
            return true;
        }
        entry
            .file_name()
            .to_str()
            .map(|name| !should_skip(name))
            .unwrap_or(true)
    }) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if let Some(filter) = &name_filter {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !filter.matches(name) {
                continue;
            }
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }

        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // Cheap binary check: a NUL byte in the head means it is not source.
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        files_searched += 1;
        let display = path.strip_prefix(workspace).unwrap_or(path).display();

        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                hits.push(format!("{display}:{}: {}", index + 1, line.trim_end()));
                if hits.len() >= MAX_MATCHES {
                    hit_limit = true;
                    break;
                }
            }
        }
        if hit_limit {
            break;
        }
    }

    if hits.is_empty() {
        return Ok(format!(
            "no matches for {pattern:?} ({files_searched} files searched)"
        ));
    }

    let total = hits.len();
    let mut report = format!("{total} matches in {files_searched} files for {pattern:?}\n");
    report.push_str(&hits.join("\n"));
    if hit_limit {
        report.push_str(&format!(
            "\n… stopping at {MAX_MATCHES} matches; narrow the pattern or path"
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths come back with the platform's own separator, so comparisons are
    /// made on a flattened copy. These tests are about which lines matched, not
    /// about how the separator is spelled.
    fn flat(text: &str) -> String {
        text.replace('\\', "/")
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("target")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {}\nlet todo = 1;\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("src/lib.rs"), "// nothing here\n").expect("write");
        std::fs::write(dir.path().join("target/junk.rs"), "todo todo todo\n").expect("write");
        std::fs::write(dir.path().join("data.bin"), b"todo\0binary").expect("write");
        dir
    }

    #[tokio::test]
    async fn finds_matching_lines_with_path_and_line_number() {
        let dir = fixture();
        let outcome = Grep.run(&json!({"pattern": "todo"}), dir.path()).await;
        assert!(!outcome.is_error, "{}", outcome.content);

        let content = flat(&outcome.content);
        assert!(
            content.contains("src/main.rs:2: let todo = 1;"),
            "got: {content}"
        );
    }

    #[tokio::test]
    async fn skips_ignored_directories() {
        let dir = fixture();
        let outcome = Grep.run(&json!({"pattern": "todo"}), dir.path()).await;

        let content = flat(&outcome.content);
        assert!(
            !content.contains("target/"),
            "target/ must be skipped: {content}"
        );
        // And the check above is meaningful: the same run did find real matches.
        assert!(content.contains("src/main.rs"), "{content}");
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = fixture();
        let outcome = Grep.run(&json!({"pattern": "todo"}), dir.path()).await;

        let content = flat(&outcome.content);
        assert!(content.contains("src/main.rs"), "{content}");
        assert!(
            !content.contains("data.bin"),
            "binary files must be skipped: {content}"
        );
    }

    #[tokio::test]
    async fn a_glob_argument_restricts_the_files_searched() {
        let dir = fixture();
        let outcome = Grep
            .run(&json!({"pattern": "nothing", "glob": "*.rs"}), dir.path())
            .await;

        let content = flat(&outcome.content);
        assert!(content.contains("src/lib.rs"), "{content}");
    }

    #[tokio::test]
    async fn can_search_a_single_file() {
        let dir = fixture();
        let outcome = Grep
            .run(
                &json!({"pattern": "main", "path": "src/main.rs"}),
                dir.path(),
            )
            .await;

        let content = flat(&outcome.content);
        assert!(content.contains("src/main.rs"), "{content}");
        assert!(!content.contains("lib.rs"), "{content}");
    }

    #[tokio::test]
    async fn supports_a_real_regular_expression() {
        let dir = fixture();
        let outcome = Grep
            .run(&json!({"pattern": "^fn\\s+\\w+"}), dir.path())
            .await;
        assert!(outcome.content.contains("fn main"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn no_matches_is_not_an_error() {
        let dir = fixture();
        let outcome = Grep.run(&json!({"pattern": "zzzz"}), dir.path()).await;
        assert!(!outcome.is_error);
        assert!(
            outcome.content.contains("no matches"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_invalid_regex_is_a_usable_tool_error() {
        let dir = fixture();
        let outcome = Grep.run(&json!({"pattern": "("}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("regular expression"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn an_invalid_file_glob_is_a_usable_tool_error() {
        let dir = fixture();
        let outcome = Grep
            .run(&json!({"pattern": "x", "glob": "[bad"}), dir.path())
            .await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("file glob"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn a_path_outside_the_workspace_is_refused() {
        let dir = fixture();
        let outcome = Grep
            .run(&json!({"pattern": "root", "path": "/etc"}), dir.path())
            .await;
        assert!(outcome.is_error);
        assert!(
            outcome.content.contains("outside the workspace"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_missing_pattern_is_reported() {
        let dir = fixture();
        let outcome = Grep.run(&json!({}), dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("pattern"), "{}", outcome.content);
    }
}
