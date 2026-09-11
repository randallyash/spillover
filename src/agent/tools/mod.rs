//! The tools the model can call, and the registry that offers them.
//!
//! Every tool reports its [`Risk`], which is what the approval prompt keys off:
//! read-only tools run silently, and anything that can change the user's files
//! or system must be confirmed.

pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod list;
pub mod read;
pub mod shell;
pub mod write;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::session::ToolSpec;

/// Upper bound on what a single tool may hand back to the model, so one huge
/// file or command output cannot blow out the context window.
pub const MAX_OUTPUT_CHARS: usize = 40_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads only. Runs without asking.
    Read,
    /// Can modify files or the system. Requires confirmation.
    Write,
}

impl Risk {
    /// Whether tools up to this level may be used.
    ///
    /// This is the ceiling a run is held to, not the risk of one tool: a run
    /// allowed `Read` may only use tools that read, which is how a read-only
    /// mode is enforced rather than merely requested.
    pub fn permits(self, needed: Risk) -> bool {
        match self {
            Risk::Write => true,
            Risk::Read => needed == Risk::Read,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value;
    fn risk(&self) -> Risk;

    /// What this specific call is about to do, shown before a risky tool runs.
    ///
    /// Async because the useful answer sometimes requires reading: `edit_file`
    /// shows the actual diff rather than just the path.
    async fn preview(&self, arguments: &Value, workspace: &Path) -> String;

    async fn run(&self, arguments: &Value, workspace: &Path) -> ToolOutcome;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn with_default_tools() -> Self {
        let mut registry = Self::default();
        registry.add(read::ReadFile);
        registry.add(list::ListDir);
        registry.add(glob_tool::Glob);
        registry.add(grep::Grep);
        registry.add(write::WriteFile);
        registry.add(edit::EditFile);
        registry.add(shell::RunShell);
        registry
    }

    pub fn add<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.push(Box::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.as_ref())
    }

    /// The specs for a run held to `ceiling`.
    ///
    /// There is deliberately no accessor that ignores the ceiling: listing
    /// tools is only ever done to offer them to a model, and which tools may be
    /// offered is exactly what the ceiling decides.
    ///
    /// A read-only run is never offered a write tool, so the model does not
    /// spend turns reaching for one and being refused. That refusal still
    /// happens — see the check in the agent loop — because withholding the
    /// offer is a courtesy to a well-behaved model, not a guarantee.
    pub fn specs_permitting(&self, ceiling: Risk) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|tool| ceiling.permits(tool.risk()))
            .map(|tool| tool.spec())
            .collect()
    }
}

/// Reading arguments out of the model's JSON, with usable errors when it gets
/// one wrong: the message goes back to the model so it can correct itself.
pub struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    pub fn new(value: &'a Value) -> Self {
        Self(value)
    }

    pub fn required_str(&self, key: &str) -> Result<&'a str, ToolOutcome> {
        match self.0.get(key).and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => Ok(value),
            Some(_) => Err(ToolOutcome::error(format!(
                "{key} was empty; pass the value you intended"
            ))),
            None => Err(ToolOutcome::error(format!(
                "missing required argument {key:?} (expected a string)"
            ))),
        }
    }

    /// Like [`Args::required_str`], but an empty string is a valid value:
    /// writing an empty file, or replacing text with nothing, are both real
    /// requests.
    pub fn required_text(&self, key: &str) -> Result<&'a str, ToolOutcome> {
        match self.0.get(key).and_then(Value::as_str) {
            Some(value) => Ok(value),
            None => Err(ToolOutcome::error(format!(
                "missing required argument {key:?} (expected a string)"
            ))),
        }
    }

    pub fn optional_str(&self, key: &str) -> Option<&'a str> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    }

    pub fn optional_usize(&self, key: &str) -> Option<usize> {
        self.0
            .get(key)
            .and_then(Value::as_u64)
            .map(|value| value as usize)
    }

    pub fn optional_bool(&self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
}

/// Directory names that are never worth walking: build output, dependency
/// caches, and version control internals.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
];

pub fn should_skip(name: &str) -> bool {
    SKIPPED_DIRS.contains(&name)
}

/// Resolve a model-supplied path against the workspace, expanding `~`.
pub fn resolve(workspace: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    let expanded = if raw == "~" {
        home().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    }
}

fn home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

/// Trim a tool's output to something the model can actually use.
pub fn cap(text: String) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text;
    }
    let head: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    format!("{head}\n… truncated at {MAX_OUTPUT_CHARS} characters")
}

pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spec, which is what a build-mode run is offered.
    fn all_specs() -> Vec<ToolSpec> {
        Registry::with_default_tools().specs_permitting(Risk::Write)
    }

    #[test]
    fn the_default_registry_offers_every_tool() {
        let mut names: Vec<String> = all_specs().into_iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "edit_file",
                "glob",
                "grep",
                "list_dir",
                "read_file",
                "run_shell",
                "write_file",
            ]
        );
    }

    #[test]
    fn every_tool_declares_a_spec_the_model_can_read() {
        for spec in all_specs() {
            assert!(!spec.name.is_empty());
            assert!(
                !spec.description.is_empty(),
                "{} needs a description",
                spec.name
            );
            assert_eq!(
                spec.parameters["type"], "object",
                "{} needs an object schema",
                spec.name
            );
        }
    }

    #[test]
    fn only_mutating_tools_are_marked_write() {
        let registry = Registry::with_default_tools();
        for name in ["read_file", "list_dir", "glob", "grep"] {
            assert_eq!(
                registry.get(name).unwrap().risk(),
                Risk::Read,
                "{name} should be read-only"
            );
        }
        for name in ["write_file", "edit_file", "run_shell"] {
            assert_eq!(
                registry.get(name).unwrap().risk(),
                Risk::Write,
                "{name} should require confirmation"
            );
        }
    }

    #[test]
    fn unknown_tool_names_resolve_to_nothing() {
        assert!(
            Registry::with_default_tools()
                .get("delete_everything")
                .is_none()
        );
    }

    #[test]
    fn a_read_ceiling_withholds_every_tool_that_changes_anything() {
        let registry = Registry::with_default_tools();
        let mut offered: Vec<String> = registry
            .specs_permitting(Risk::Read)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        offered.sort();

        assert_eq!(offered, vec!["glob", "grep", "list_dir", "read_file"]);
        // The three that can change the world are simply not on the table.
        for absent in ["write_file", "edit_file", "run_shell"] {
            assert!(
                !offered.iter().any(|name| name == absent),
                "{absent} must not be offered in a read-only run"
            );
        }
    }

    #[test]
    fn a_write_ceiling_withholds_nothing() {
        let registry = Registry::with_default_tools();
        let build = registry.specs_permitting(Risk::Write);
        let plan = registry.specs_permitting(Risk::Read);

        assert!(
            build.len() > plan.len(),
            "the write ceiling must offer more than the read one"
        );
        for name in ["write_file", "edit_file", "run_shell"] {
            assert!(
                build.iter().any(|spec| spec.name == name),
                "{name} is missing from a write-ceiling run"
            );
        }
        // And nothing is dropped by a write ceiling that the registry holds.
        assert_eq!(build.len(), 7);
    }

    #[test]
    fn the_risk_ceiling_permits_exactly_what_it_says() {
        assert!(Risk::Read.permits(Risk::Read));
        assert!(!Risk::Read.permits(Risk::Write));
        assert!(Risk::Write.permits(Risk::Read));
        assert!(Risk::Write.permits(Risk::Write));
    }

    #[test]
    fn relative_paths_are_resolved_against_the_workspace() {
        let workspace = Path::new("/work");
        assert_eq!(
            resolve(workspace, "src/main.rs"),
            PathBuf::from("/work/src/main.rs")
        );
        assert_eq!(
            resolve(workspace, "./a.txt"),
            PathBuf::from("/work/./a.txt")
        );
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let workspace = Path::new("/work");
        assert_eq!(
            resolve(workspace, "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn a_leading_tilde_expands_to_the_home_directory() {
        let Some(home) = home() else {
            return;
        };
        assert_eq!(
            resolve(Path::new("/work"), "~/notes.txt"),
            home.join("notes.txt")
        );
        assert_eq!(resolve(Path::new("/work"), "~"), home);
    }

    #[test]
    fn required_arguments_produce_a_usable_error_when_missing() {
        let args = json!({});
        let args = Args::new(&args);
        let error = args.required_str("path").expect_err("must fail");
        assert!(error.is_error);
        assert!(error.content.contains("path"), "got: {}", error.content);
    }

    #[test]
    fn an_empty_required_argument_is_rejected() {
        let args = json!({ "path": "   " });
        let error = Args::new(&args)
            .required_str("path")
            .expect_err("must fail");
        assert!(error.content.contains("empty"), "got: {}", error.content);
    }

    #[test]
    fn optional_arguments_read_through() {
        let args = json!({ "limit": 10, "all": true, "note": "hi" });
        let args = Args::new(&args);
        assert_eq!(args.optional_usize("limit"), Some(10));
        assert!(args.optional_bool("all"));
        assert_eq!(args.optional_str("note"), Some("hi"));
        assert!(!args.optional_bool("missing"));
        assert_eq!(args.optional_str("missing"), None);
    }

    #[test]
    fn ignored_directories_are_recognised() {
        for name in ["target", ".git", "node_modules", "dist", "__pycache__"] {
            assert!(should_skip(name), "{name} should be skipped");
        }
        for name in ["src", "main.rs", "targets", "git"] {
            assert!(!should_skip(name), "{name} should not be skipped");
        }
    }

    #[test]
    fn required_text_accepts_an_empty_string() {
        let args = json!({ "content": "" });
        assert_eq!(Args::new(&args).required_text("content").unwrap(), "");
    }

    #[test]
    fn required_text_still_requires_the_key() {
        let args = json!({});
        assert!(Args::new(&args).required_text("content").is_err());
    }

    #[test]
    fn output_is_capped_with_a_visible_marker() {
        let big = "x".repeat(MAX_OUTPUT_CHARS + 100);
        let capped = cap(big);
        assert!(capped.contains("truncated"));
        assert!(capped.chars().count() < MAX_OUTPUT_CHARS + 100);
    }

    #[test]
    fn small_output_passes_through_untouched() {
        assert_eq!(cap("short".to_string()), "short");
    }
}
