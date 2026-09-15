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

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::undo::Undo;
use crate::detect::ErrorClass;

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
    /// How to put back what this changed.
    pub undo: Option<Box<Undo>>,
    /// What kind of failure this is, when it is one.
    pub class: Option<ErrorClass>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            undo: None,
            class: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        let content = message.into();
        let class = ErrorClass::classify(&content);
        Self {
            content,
            is_error: true,
            undo: None,
            class: Some(class),
        }
    }

    /// A filesystem or process failure, classified from the error's kind.
    pub fn io(context: impl std::fmt::Display, err: &std::io::Error) -> Self {
        Self {
            content: format!("{context}: {err}"),
            is_error: true,
            undo: None,
            class: Some(ErrorClass::from_io(err)),
        }
    }

    /// Attach the means to reverse this change.
    pub fn undoing(mut self, undo: Undo) -> Self {
        self.undo = Some(Box::new(undo));
        self
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
    /// The whole agent. Seven tools, and that is the budget for 0.1.x.
    #[cfg(test)]
    pub fn with_default_tools() -> Self {
        Self::with_shell_timeout(std::time::Duration::from_secs(
            crate::config::DEFAULT_SHELL_TIMEOUT_SECS,
        ))
    }

    /// The same seven tools, with `run_shell` held to this timeout unless a
    /// call names a shorter or longer one of its own.
    pub fn with_shell_timeout(timeout: std::time::Duration) -> Self {
        let mut registry = Self::default();
        registry.add(read::ReadFile);
        registry.add(list::ListDir);
        registry.add(glob_tool::Glob);
        registry.add(grep::Grep);
        registry.add(write::WriteFile);
        registry.add(edit::EditFile);
        registry.add(shell::RunShell::new(timeout));
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

/// Resolve a model-supplied path against the workspace, expanding `~`, and
/// refuse it if the result would land outside.
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf, ToolOutcome> {
    confine(workspace, &expand(raw), raw.trim())
}

/// The directory part of a glob, up to the first `*`, `?` or `[`, so the
/// pattern can be confined before it is walked.
pub fn glob_dir(pattern: &str) -> &str {
    let literal = match pattern.find(['*', '?', '[']) {
        Some(index) => &pattern[..index],
        None => pattern,
    };
    match literal.rfind(['/', '\\']) {
        Some(index) => &pattern[..=index],
        None => "",
    }
}

/// Whether `candidate` is the workspace or a path inside it.
pub fn is_inside(workspace: &Path, candidate: &Path) -> bool {
    candidate.starts_with(workspace)
}

fn expand(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        home().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    }
}

fn confine(workspace: &Path, requested: &Path, named: &str) -> Result<PathBuf, ToolOutcome> {
    let workspace = std::fs::canonicalize(workspace).map_err(|error| {
        ToolOutcome::io(
            format!(
                "the workspace {} is not a usable directory",
                workspace.display()
            ),
            &error,
        )
    })?;

    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let lexical = lexical_normalise(&requested);

    match std::fs::canonicalize(&lexical) {
        Ok(canon) => require_inside(&workspace, &canon, named),
        Err(_) => {
            let (ancestor, missing) = split_missing(&lexical);
            let base = std::fs::canonicalize(&ancestor).unwrap_or(ancestor);
            require_inside(&workspace, &base, named)?;
            let mut out = base;
            for part in missing {
                out.push(part);
            }
            require_inside(&workspace, &out, named)
        }
    }
}

fn require_inside(workspace: &Path, candidate: &Path, named: &str) -> Result<PathBuf, ToolOutcome> {
    if is_inside(workspace, candidate) {
        Ok(candidate.to_path_buf())
    } else {
        Err(outside(named, workspace))
    }
}

fn outside(named: &str, workspace: &Path) -> ToolOutcome {
    ToolOutcome::error(format!(
        "{named} is outside the workspace ({}); file tools only read and write inside it",
        workspace.display()
    ))
}

/// Drop `.` and apply `..` without touching the filesystem, so a walk out of
/// the workspace is visible before we try to open anything.
fn lexical_normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::Normal(_) => out.push(component),
        }
    }
    out
}

/// Split a path that does not exist yet into the longest ancestor that does,
/// and the components still to be created.
fn split_missing(path: &Path) -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        match ancestor.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                match ancestor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        ancestor = parent.to_path_buf();
                    }
                    _ => break,
                }
            }
            None => break,
        }
    }
    missing.reverse();
    (ancestor, missing)
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

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn relative_paths_are_resolved_against_the_workspace() {
        let dir = workspace();
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/main.rs"), "").expect("write");
        let got = resolve(dir.path(), "src/main.rs").expect("inside");
        assert_eq!(
            got,
            std::fs::canonicalize(dir.path().join("src/main.rs")).expect("canon")
        );
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_still_confined() {
        let dir = workspace();
        let got = resolve(dir.path(), "a/b/new.txt").expect("inside");
        let root = std::fs::canonicalize(dir.path()).expect("canon");
        assert!(
            is_inside(&root, &got),
            "{} should be under {}",
            got.display(),
            root.display()
        );
        assert_eq!(got.file_name().and_then(|n| n.to_str()), Some("new.txt"));
    }

    #[test]
    fn an_absolute_path_inside_the_workspace_is_accepted() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "x").expect("write");
        let absolute = std::fs::canonicalize(dir.path().join("a.txt")).expect("canon");
        let got = resolve(dir.path(), &absolute.to_string_lossy()).expect("inside");
        assert_eq!(got, absolute);
    }

    #[test]
    fn an_absolute_path_outside_the_workspace_is_refused() {
        let dir = workspace();
        let error = resolve(dir.path(), "/etc/hosts").expect_err("must refuse");
        assert!(error.is_error);
        assert!(
            error.content.contains("outside the workspace"),
            "{}",
            error.content
        );
    }

    #[test]
    fn a_parent_walk_out_of_the_workspace_is_refused() {
        let dir = workspace();
        let error = resolve(dir.path(), "../secret").expect_err("must refuse");
        assert!(
            error.content.contains("outside the workspace"),
            "{}",
            error.content
        );
    }

    #[test]
    fn a_leading_tilde_is_refused_when_home_is_not_the_workspace() {
        let dir = workspace();
        let Some(home) = home() else {
            return;
        };
        if is_inside(
            &std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf()),
            &home,
        ) {
            return;
        }
        let error = resolve(dir.path(), "~/notes.txt").expect_err("must refuse");
        assert!(
            error.content.contains("outside the workspace"),
            "{}",
            error.content
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_leaves_the_workspace_is_refused() {
        let dir = workspace();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "nope").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link"))
            .expect("symlink");
        let error = resolve(dir.path(), "link").expect_err("must refuse");
        assert!(
            error.content.contains("outside the workspace"),
            "{}",
            error.content
        );
    }

    #[test]
    fn glob_dir_is_the_literal_prefix_before_the_first_wildcard() {
        assert_eq!(glob_dir("src/*.rs"), "src/");
        assert_eq!(glob_dir("**/*.toml"), "");
        assert_eq!(glob_dir("main.rs"), "");
        assert_eq!(glob_dir("/etc/**"), "/etc/");
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

    #[test]
    fn the_tool_set_is_closed() {
        // The rule, made mechanical. Seven tools is the budget for 0.1.x: every
        // one is another way for a local model to loop and another way for spill
        // to drift into being a worse copy of a harness it is not trying to be.
        //
        // This test failing is not a chore to be updated — it is the argument
        // having to be made. If an eighth tool is genuinely worth it, delete this
        // and say in the commit what it buys that the seven do not.
        let registry = Registry::with_default_tools();
        let mut names: Vec<String> = registry
            .specs_permitting(Risk::Write)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        names.sort();

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
            ],
            "the agent is these seven tools and no more"
        );
    }
}
