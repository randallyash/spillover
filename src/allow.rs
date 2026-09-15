//! Which shell commands may run without being asked about.
//!
//! `run_shell` hands one whole command line to a shell, so "is this safe?" cannot
//! be answered from the program name: the same line can hold a second command, a
//! substitution, or a redirect. A rule is therefore only ever consulted on a
//! command that is a bare word list. If it is not one, spill asks — there is
//! nothing for a rule to be read against.
//!
//! A rule is a prefix of *words*, never a pattern over the raw line, and that
//! difference is the whole design: `"git status"` is a prefix of
//! `"git status; rm -rf ~"`, which is why a prefix of the line cannot be an
//! allow-list.

/// Characters that mean a line is something other than a list of words.
const OPERATORS: &[char] = &[
    ';', '&', '|', '<', '>', '`', '$', '\\', '(', ')', '{', '}', '[', ']', '*', '?', '~', '"',
    '\'', '\n', '\r',
];

/// A command that may run without asking: the words a line must begin with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    words: Vec<String>,
}

impl Rule {
    /// Read a rule from what a person typed or wrote in a config.
    pub fn parse(text: &str) -> Result<Self, String> {
        let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();

        if words.is_empty() {
            return Err("a rule needs at least one word, such as `ls` or `git status`".to_string());
        }

        for word in &words {
            if let Some(character) = word.chars().find(|character| OPERATORS.contains(character)) {
                return Err(format!(
                    "a rule cannot contain {character:?} (in {word:?}). Rules are matched against \
                     commands with no shell operators in them — a command holding one always asks, \
                     because the line can mean more than its words say."
                ));
            }
        }

        Ok(Self { words })
    }

    /// Whether this rule covers a command.
    pub fn matches(&self, command: &str) -> bool {
        if command
            .chars()
            .any(|character| OPERATORS.contains(&character))
        {
            return false;
        }

        let words: Vec<&str> = command.split_whitespace().collect();
        words.len() >= self.words.len()
            && words
                .iter()
                .zip(self.words.iter())
                .all(|(word, expected)| word == expected)
    }

    /// The rule as a person would write it.
    pub fn text(&self) -> String {
        self.words.join(" ")
    }
}

/// The rules in force.
#[derive(Debug, Clone, Default)]
pub struct AllowRules {
    from_config: Vec<Rule>,
    session: Vec<Rule>,
}

impl AllowRules {
    /// The rules a configuration asks for.
    pub fn new(config: &[String]) -> Self {
        Self {
            from_config: config
                .iter()
                .filter_map(|text| Rule::parse(text).ok())
                .collect(),
            session: Vec::new(),
        }
    }

    /// The rule that covers this command, if one does.
    pub fn allows(&self, command: &str) -> Option<&Rule> {
        self.from_config
            .iter()
            .chain(self.session.iter())
            .find(|rule| rule.matches(command))
    }

    /// Stick a rule for the rest of the session.
    pub fn add_session(&mut self, rule: Rule) -> bool {
        if self.session.contains(&rule) || self.from_config.contains(&rule) {
            return false;
        }
        self.session.push(rule);
        true
    }

    /// Drop this session's rules, returning how many went.
    pub fn clear_session(&mut self) -> usize {
        let count = self.session.len();
        self.session.clear();
        count
    }

    /// The rules split by where they came from, in the order they are tried.
    pub fn texts_by_source(&self) -> (Vec<String>, Vec<String>) {
        (
            self.from_config.iter().map(Rule::text).collect(),
            self.session.iter().map(Rule::text).collect(),
        )
    }

    /// Every rule in force as it would be written down.
    pub fn texts(&self) -> Vec<String> {
        let (mut from_config, session) = self.texts_by_source();
        from_config.extend(session);
        from_config
    }

    /// Whether this is exactly what a configuration saying nothing would give.
    pub fn is_default(&self) -> bool {
        self.session.is_empty()
            && self
                .from_config
                .iter()
                .map(Rule::text)
                .eq(READ_ONLY_SHELL.iter().map(|rule| rule.to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.from_config.is_empty() && self.session.is_empty()
    }
}

/// The commands that run unasked before anyone has said anything.
pub const READ_ONLY_SHELL: &[&str] = &[
    // Reading and listing: the reach of read_file and list_dir.
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "file",
    "stat",
    "which",
    "du",
    "df",
    "tree",
    // Searching: the reach of grep.
    "grep",
    "rg",
    // Reading a repository's own state. `git diff` and `git log` consult the
    // repository's config for an external diff driver, so a repository that asks
    // for one can run it — which is why this list stays short, and why
    // `allow_shell = []` exists for anyone who would rather nothing ran unasked.
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git blame",
    "git grep",
    "git rev-parse",
    "git describe",
    "git shortlog",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str) -> Rule {
        Rule::parse(text).expect("a valid rule")
    }

    fn defaults() -> AllowRules {
        AllowRules::new(
            &READ_ONLY_SHELL
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_rule_covers_the_words_it_names_and_whatever_follows() {
        let status = rule("git status");

        assert!(status.matches("git status"));
        assert!(status.matches("git status --short"));
        assert!(
            status.matches("git   status   --short"),
            "spacing is not the rule"
        );
        // A different subcommand is a different command, and a longer word is a
        // different program: `ls` must not cover `lsblk`.
        assert!(!status.matches("git stash"));
        assert!(!rule("ls").matches("lsblk"));
        assert!(!rule("ls").matches("lsfoo -la"));
    }

    #[test]
    fn a_command_holding_a_shell_operator_is_never_covered() {
        // The reason a rule is words and not a pattern over the line: every one
        // of these starts with `ls` and none of them runs `ls`.
        let ls = rule("ls");
        for command in [
            "ls; rm -rf ~",
            "ls && rm -rf ~",
            "ls || true",
            "ls | rm -rf ~",
            "ls > /etc/passwd",
            "ls >> /etc/passwd",
            "ls < /etc/passwd",
            "ls `rm -rf ~`",
            "ls $(rm -rf ~)",
            "ls ${HOME}",
            "ls ~/projects",
            "ls *",
            "ls ?",
            "ls [a-z]*",
            "ls 'a file'",
            "ls \"a file\"",
            "ls (x)",
            "ls {a,b}",
            "ls a\\ b",
            "ls\nrm -rf ~",
        ] {
            assert!(!ls.matches(command), "{command:?} should not be covered");
        }
    }

    #[test]
    fn a_command_that_sets_an_environment_variable_first_is_not_covered() {
        // The first word is the assignment, so no rule naming a program matches
        // it — which is exactly the case worth failing closed on, since
        // `LD_PRELOAD=… ls` is not `ls`.
        let ls = rule("ls");
        assert!(!ls.matches("FOO=bar ls"));
        assert!(!ls.matches("LD_PRELOAD=./evil.so ls"));
    }

    #[test]
    fn a_rule_cannot_contain_an_operator_either() {
        assert!(Rule::parse("ls").is_ok());
        assert!(
            Rule::parse("  git   status  ").is_ok(),
            "spacing is not the rule"
        );

        for text in [
            "",
            "   ",
            "ls; rm -rf ~",
            "ls | rm",
            "git diff HEAD~1",
            "echo $HOME",
        ] {
            assert!(Rule::parse(text).is_err(), "{text:?} should be refused");
        }

        // And the refusal says what is wrong rather than just saying no.
        let error = Rule::parse("ls; rm").expect_err("refused");
        assert!(error.contains("cannot contain"), "{error}");
        assert!(error.contains("always asks"), "{error}");
    }

    #[test]
    fn every_default_rule_is_a_rule_and_survives_being_written_down() {
        // The default list is also what `/allow` prints and what `/allow save`
        // writes, so each entry has to parse and round-trip as itself.
        for text in READ_ONLY_SHELL {
            let parsed = Rule::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(&parsed.text(), text, "{text} did not round-trip");
        }

        assert!(defaults().is_default());
    }

    #[test]
    fn the_default_list_leaves_out_the_programs_whose_flags_can_write_or_execute() {
        // The claim the default list rests on. Each of these can write or execute
        // through a flag, and a rule cannot see flags.
        let rules = defaults();
        for command in [
            "find . -delete",
            "find . -exec rm {} +",
            "sort -o out.txt in.txt",
            "sed -i s/a/b/ file",
            "xargs rm",
            "awk BEGIN{system(\"rm -rf ~\")}",
            "make",
            "make install",
            "cargo test",
            "npm test",
            "python -c import os",
        ] {
            assert!(
                rules.allows(command).is_none(),
                "{command:?} must be asked about"
            );
        }
    }

    #[test]
    fn the_default_list_covers_reading_and_searching() {
        let rules = defaults();
        for command in [
            "ls",
            "ls -la src",
            "cat notes.txt",
            "head -20 log",
            "tail -f log",
            "wc -l file",
            "rg -n pattern",
            "grep -R pattern .",
            "git status",
            "git status --porcelain",
            "git diff",
            "git log --oneline -10",
            "git show HEAD",
        ] {
            assert!(
                rules.allows(command).is_some(),
                "{command:?} should run unasked"
            );
        }
    }

    #[test]
    fn an_empty_list_asks_about_everything() {
        // The escape hatch: `allow_shell = []` in a config.
        let rules = AllowRules::new(&[]);
        assert!(rules.is_empty());
        assert!(rules.allows("ls").is_none());
        assert!(rules.allows("git status").is_none());
    }

    #[test]
    fn a_rule_that_will_not_parse_is_not_in_force() {
        // Fail closed: a configuration that slipped a bad rule past validation
        // must mean a question asked, never a command run.
        let rules = AllowRules::new(&["ls; rm -rf ~".to_string(), "ls".to_string()]);
        assert!(rules.allows("ls").is_some(), "the good rule still applies");
        assert!(rules.allows("cat x").is_none());

        let nothing = AllowRules::new(&["ls; rm -rf ~".to_string()]);
        assert!(nothing.is_empty(), "the bad rule is not in force");
    }

    #[test]
    fn a_session_rule_is_added_once_and_can_be_cleared() {
        let mut rules = AllowRules::new(&[]);
        assert!(rules.add_session(rule("cargo test")), "added");
        assert!(!rules.add_session(rule("cargo test")), "not added twice");
        assert!(rules.allows("cargo test -- --nocapture").is_some());

        // A rule already in the config is not a session rule as well: saying
        // "this is stuck for the session" about something that is already
        // permanent would be a lie in the listing.
        let mut with_defaults = defaults();
        assert!(!with_defaults.add_session(rule("git status")));

        assert_eq!(rules.clear_session(), 1);
        assert_eq!(rules.clear_session(), 0, "clearing twice is not a change");
        assert!(rules.allows("cargo test").is_none());
    }

    #[test]
    fn the_rules_are_reported_split_by_where_they_came_from() {
        // What `/allow` prints, and what `/allow save` writes: the difference
        // between a rule that lasts until the file changes and one that lasts
        // until spill exits is the reason to print them separately at all.
        let mut rules = AllowRules::new(&["git status".to_string()]);
        rules.add_session(rule("cargo test"));

        let (from_config, session) = rules.texts_by_source();
        assert_eq!(from_config, vec!["git status".to_string()]);
        assert_eq!(session, vec!["cargo test".to_string()]);

        // And written down, they are one list with the configuration's first.
        assert_eq!(
            rules.texts(),
            vec!["git status".to_string(), "cargo test".to_string()]
        );
    }

    #[test]
    fn a_list_that_is_not_the_default_is_recognised_as_such() {
        // What the report keys on to decide whether to print the list at all.
        assert!(defaults().is_default());
        assert!(
            !AllowRules::new(&[]).is_default(),
            "asking about everything is a choice"
        );
        assert!(
            !AllowRules::new(&["make".to_string()]).is_default(),
            "a shorter list is a choice"
        );

        let mut with_session = defaults();
        with_session.add_session(rule("make"));
        assert!(!with_session.is_default(), "a session rule is a choice too");
    }
}
