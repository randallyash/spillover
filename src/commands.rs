//! The slash commands, and what a line of input means.
//!
//! A command earns its place here on one test: it has no other affordance, and
//! it is about this program rather than about the conversation. So the set is
//! mostly about the tier chain and what it costs — the two things spill does
//! that a single-model agent cannot — plus the conversation operations that have
//! nowhere else to live. Everything a keystroke already does is not repeated
//! here, and neither is anything belonging to another tool's config.
//!
//! The catalogue is data, so the parser, the menu that appears as you type, and
//! the help overlay all read the same list and cannot drift apart.

/// What a command's argument should be called in the menu, if it takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    None,
    /// A required free-form argument.
    Required(&'static str),
    /// An argument that may be omitted.
    Optional(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub name: &'static str,
    pub arg: Arg,
    pub summary: &'static str,
}

/// Every command, in the order the menu and the help overlay show them:
/// grouped by what they are about, most specific to spill first.
pub const CATALOGUE: &[Spec] = &[
    Spec {
        name: "tier",
        arg: Arg::Optional("<name|number|auto>"),
        summary: "Show the chain, or choose which tier answers",
    },
    Spec {
        name: "escalate",
        arg: Arg::None,
        summary: "Spill to the next tier now, without waiting for it to stall",
    },
    Spec {
        name: "consult",
        arg: Arg::None,
        summary: "Ask the tier below one question, and keep driving",
    },
    Spec {
        name: "on-stuck",
        arg: Arg::Required("<escalate|consult|auto>"),
        summary: "What a stuck tier does for the rest of the session",
    },
    Spec {
        name: "retry",
        arg: Arg::Optional("<name|number>"),
        summary: "Send the last turn again, here or on another tier",
    },
    Spec {
        name: "drop",
        arg: Arg::None,
        summary: "Discard the active tier's own conversation and start it fresh",
    },
    Spec {
        name: "sticky",
        arg: Arg::Required("<on|off>"),
        summary: "Whether a spill keeps the lower tier for the rest of the session",
    },
    Spec {
        name: "cost",
        arg: Arg::None,
        summary: "Tokens and cache reads so far, tier by tier",
    },
    Spec {
        name: "context",
        arg: Arg::None,
        summary: "What gets sent on each turn, and how large it has grown",
    },
    Spec {
        name: "why",
        arg: Arg::None,
        summary: "Why the last tier was abandoned, with the numbers behind it",
    },
    Spec {
        name: "compact",
        arg: Arg::None,
        summary: "Fold earlier turns into a short ledger to shrink what is sent",
    },
    Spec {
        name: "clear",
        arg: Arg::None,
        summary: "Start a new conversation, keeping the tiers as they are",
    },
    Spec {
        name: "allow",
        // `save` and `clear` are read as subcommands rather than as the words of
        // a rule, which is the same bargain `/on-stuck auto` makes: a rule for a
        // program literally called `clear` is not worth an unambiguous grammar.
        arg: Arg::Optional("<words>|[save] <words>|clear"),
        summary: "Let a shell command run without asking",
    },
    Spec {
        name: "undo",
        arg: Arg::None,
        summary: "Put back the last file an approved write changed",
    },
    Spec {
        name: "help",
        arg: Arg::None,
        summary: "Show every command and key",
    },
    Spec {
        name: "quit",
        arg: Arg::None,
        summary: "Leave spill",
    },
];

/// A parsed line of input: a command, or text to send to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// An ordinary message.
    Prompt(String),
    Command {
        name: String,
        argument: String,
    },
}

impl Input {
    /// Whether this is a command rather than a prompt.
    #[cfg(test)]
    pub fn is_command(&self) -> bool {
        matches!(self, Self::Command { .. })
    }
}

/// Whether a line is a command, cheaply enough to ask while rendering.
///
/// The footer changes its hints on this, so it is called every frame and
/// deliberately does no allocating.
///
/// This is deliberately looser than [`parse`]: a prefix counts, so the hints
/// change the moment a command starts being typed. Only the display depends on
/// it, and `parse` still insists on an exact name, so a path like `/cl` cannot
/// be run as a command just because it looks like the start of one.
pub fn looks_like_command(input: &str) -> bool {
    let Some(rest) = input.trim_start().strip_prefix('/') else {
        return false;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    if name.is_empty() {
        return true;
    }
    CATALOGUE
        .iter()
        .any(|spec| spec.name.starts_with(&name.to_lowercase()))
}

/// Read a line the way the user meant it.
///
/// A slash only means a command at the very start of the line, and only when the
/// word after it is one we know. So "what does /usr/local hold" is a question,
/// and a path is never mistaken for an instruction. `//` at the start forces a
/// literal slash, for the rare message that has to begin with one.
pub fn parse(input: &str) -> Input {
    let trimmed = input.trim();

    if let Some(literal) = trimmed.strip_prefix("//") {
        return Input::Prompt(format!("/{literal}"));
    }

    let Some(rest) = trimmed.strip_prefix('/') else {
        return Input::Prompt(trimmed.to_string());
    };

    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_lowercase();
    let argument = parts.next().unwrap_or("").trim().to_string();

    if find(&name).is_none() {
        // Not a command we know: treat it as text rather than guessing.
        return Input::Prompt(trimmed.to_string());
    }

    Input::Command { name, argument }
}

/// A command by name, case-insensitively.
pub fn find(name: &str) -> Option<&'static Spec> {
    let wanted = name.to_lowercase();
    CATALOGUE.iter().find(|spec| spec.name == wanted)
}

/// Commands whose name begins with what has been typed so far.
///
/// An empty prefix matches everything, which is what makes typing a lone `/`
/// open the whole menu.
pub fn matching(prefix: &str) -> Vec<&'static Spec> {
    let wanted = prefix.trim_start_matches('/').to_lowercase();
    CATALOGUE
        .iter()
        .filter(|spec| spec.name.starts_with(&wanted))
        .collect()
}

/// The word being typed after the slash, if the line is still a command name
/// rather than a command's argument.
///
/// `Some("")` for a lone `/`. This is what decides whether the menu should be
/// open: once there is a space, the user is typing an argument and the menu
/// would only be in the way.
pub fn completion_prefix(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('/')?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// How a command is written out, with its argument, for the menu.
pub fn usage(spec: &Spec) -> String {
    match spec.arg {
        Arg::None => format!("/{}", spec.name),
        Arg::Required(argument) | Arg::Optional(argument) => {
            format!("/{} {argument}", spec.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_on_stuck_parses() {
        let parsed = parse("/on-stuck consult");
        println!("PARSED: {parsed:?}");
        assert_eq!(
            parsed,
            Input::Command {
                name: "on-stuck".into(),
                argument: "consult".into()
            }
        );
    }

    #[test]
    fn every_command_has_a_unique_name_and_a_summary() {
        let mut names: Vec<&str> = CATALOGUE.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "two commands share a name");

        for spec in CATALOGUE {
            assert!(!spec.summary.is_empty(), "{} has no summary", spec.name);
            // The summary has to share one menu row with the command and its
            // argument, so a long one would be clipped before it said anything.
            assert!(
                spec.summary.chars().count() <= 64,
                "{} has a summary too long for a menu row ({} chars)",
                spec.name,
                spec.summary.chars().count()
            );
            assert!(
                spec.summary.starts_with(char::is_uppercase),
                "{} reads as a continuation: {}",
                spec.name,
                spec.summary
            );
        }
    }

    #[test]
    fn the_commands_are_about_this_program_rather_than_the_conversation() {
        // The line this set is held to: a command has no other affordance and is
        // about spill itself. These belong to a harness spill does not have, so
        // they must not appear.
        for foreign in [
            "init", "memory", "skills", "agents", "mcp", "taste", "theme", "config", "model",
            "login", "share", "resume", "review",
        ] {
            assert!(
                find(foreign).is_none(),
                "/{foreign} is another tool's job or has no machinery here"
            );
        }
    }

    #[test]
    fn a_plain_message_is_a_prompt() {
        assert_eq!(
            parse("explain the parser"),
            Input::Prompt("explain the parser".into())
        );
    }

    #[test]
    fn a_known_command_is_parsed_with_its_argument() {
        assert_eq!(
            parse("/tier 2"),
            Input::Command {
                name: "tier".into(),
                argument: "2".into()
            }
        );
        assert_eq!(
            parse("/tier DeepSeek"),
            Input::Command {
                name: "tier".into(),
                argument: "DeepSeek".into()
            }
        );
        // No argument is not an error; the command decides what that means.
        assert_eq!(
            parse("/cost"),
            Input::Command {
                name: "cost".into(),
                argument: String::new()
            }
        );
    }

    #[test]
    fn a_slash_in_the_middle_of_a_sentence_is_just_text() {
        // A question about a path must not become an instruction.
        assert_eq!(
            parse("what is in /usr/local/bin"),
            Input::Prompt("what is in /usr/local/bin".into())
        );
    }

    #[test]
    fn an_unknown_command_is_sent_as_text_rather_than_guessed_at() {
        assert_eq!(
            parse("/etc/hosts has my hostname"),
            Input::Prompt("/etc/hosts has my hostname".into())
        );
        assert_eq!(parse("/nonsense"), Input::Prompt("/nonsense".into()));
    }

    #[test]
    fn a_double_slash_escapes_to_a_literal_one() {
        assert_eq!(
            parse("//etc/hosts please read it"),
            Input::Prompt("/etc/hosts please read it".into())
        );
    }

    #[test]
    fn a_command_name_is_case_insensitive() {
        assert!(parse("/TIER").is_command());
        assert!(parse("/Tier Grok").is_command());
    }

    #[test]
    fn a_command_can_be_recognised_without_parsing_it() {
        assert!(looks_like_command("/cost"));
        assert!(looks_like_command("/TIER grok"));
        assert!(looks_like_command("/comp"));
        // Not a command: a path, a word we do not know, or plain text.
        assert!(!looks_like_command("/usr/local/bin"));
        assert!(!looks_like_command("/nonsense"));
        assert!(!looks_like_command("hello"));
        assert!(!looks_like_command(""));
    }

    #[test]
    fn the_menu_matches_on_a_prefix_and_opens_on_a_lone_slash() {
        let all = matching("");
        assert_eq!(all.len(), CATALOGUE.len(), "a lone slash shows everything");

        let t = matching("t");
        assert!(t.iter().any(|spec| spec.name == "tier"), "{t:?}");
        assert!(
            !t.iter().any(|spec| spec.name == "cost"),
            "a non-matching command must not be listed: {t:?}"
        );

        assert!(matching("tie").iter().any(|spec| spec.name == "tier"));
        assert!(matching("zzz").is_empty());
        // A typed leading slash is tolerated, since that is what is on screen.
        assert!(matching("/tier").iter().any(|spec| spec.name == "tier"));
    }

    #[test]
    fn the_menu_closes_once_an_argument_is_being_typed() {
        assert_eq!(completion_prefix("/"), Some(""));
        assert_eq!(completion_prefix("/ti"), Some("ti"));
        assert_eq!(
            completion_prefix("/tier "),
            None,
            "the argument is not a command name"
        );
        assert_eq!(completion_prefix("/tier 2"), None);
        assert_eq!(completion_prefix("hello"), None);
    }

    #[test]
    fn usage_shows_the_argument_where_there_is_one() {
        let tier = find("tier").expect("tier");
        assert_eq!(usage(tier), "/tier <name|number|auto>");

        let cost = find("cost").expect("cost");
        assert_eq!(usage(cost), "/cost");
    }
}
