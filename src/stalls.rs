//! Why a tier was abandoned, and what nearly stopped one that was not.
//!
//! Handing a turn to another model is the one decision this program makes on its
//! own, and an unexplainable one is worse than no decision at all: a user who
//! cannot tell why their turn was taken away will turn the fallback off. So the
//! counters behind the verdict are kept, not just the verdict — the reason says
//! what tripped, and these say how close every *other* signal came.
//!
//! The same figures go to a log file, because thresholds are tuned from what
//! actually happened rather than from what a handful of sessions felt like.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::detect::progress::ProgressCounters;
use crate::detect::repetition::RepetitionCounters;
use crate::detect::{Phase, StuckReason, Timing};

/// How much of its own allowance a wait may use before it counts as nearly
/// having run out.
///
/// Four fifths rather than one short, because waiting is continuous where the
/// other signals are counted: there is no "one more" for a gap, and the useful
/// question is whether the budget is close to being wrong.
const ALMOST_WAIT: f64 = 0.8;

/// Everything the detectors saw, at the moment the attempt ended.
///
/// Held as the detectors' own counts rather than as a summary of the verdict,
/// because the interesting number is usually the one that did *not* fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub steps_used: usize,
    pub steps_allowed: usize,
    pub repetition: RepetitionCounters,
    pub progress: ProgressCounters,
    pub timing: Timing,
}

/// A signal that came within one step of tripping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// The same line, over and over.
    Repeats { seen: usize, allowed: usize },
    /// The same span of tokens recurring inside varying text.
    Span { seen: usize, allowed: usize },
    /// The same call with the same arguments.
    SameCall { seen: usize, allowed: usize },
    /// Consecutive failures, whatever they were.
    Failures { seen: usize, allowed: usize },
    /// The same *kind* of failure from the same tool.
    SameError {
        class: crate::detect::ErrorClass,
        seen: usize,
        allowed: usize,
    },
    /// The step budget, with one step left.
    Steps { used: usize, allowed: usize },
    /// A silence that used up most of its allowance.
    Wait {
        gap_ms: u64,
        allowance_ms: u64,
        phase: Phase,
    },
}

impl Miss {
    /// How close, as a share of the allowance. Used to pick the closest of
    /// several near misses, so only the most informative one is reported.
    fn closeness(self) -> f64 {
        match self {
            Self::Repeats { seen, allowed }
            | Self::Span { seen, allowed }
            | Self::SameCall { seen, allowed }
            | Self::Failures { seen, allowed }
            | Self::SameError { seen, allowed, .. } => share(seen, allowed),
            Self::Steps { used, allowed } => share(used, allowed),
            Self::Wait {
                gap_ms,
                allowance_ms,
                ..
            } => share(gap_ms as usize, allowance_ms as usize),
        }
    }

    /// One line for the transcript, saying what nearly happened and against what.
    pub fn sentence(self) -> String {
        match self {
            Self::Repeats { seen, allowed } => {
                format!("repeated the same line {seen} of {allowed} times")
            }
            Self::Span { seen, allowed } => {
                format!("repeated a span of text {seen} of {allowed} times")
            }
            Self::SameCall { seen, allowed } => {
                format!("made the same call, arguments included, {seen} of {allowed} times")
            }
            Self::Failures { seen, allowed } => {
                format!("saw {seen} of {allowed} tool failures in a row")
            }
            Self::SameError {
                class,
                seen,
                allowed,
            } => format!("hit the same {class} failure {seen} of {allowed} times"),
            Self::Steps { used, allowed } => format!("needed {used} of {allowed} steps"),
            Self::Wait {
                gap_ms,
                allowance_ms,
                phase,
            } => format!(
                "went quiet for {}s of a {}s {phase} allowance",
                ms_to_seconds(gap_ms),
                ms_to_seconds(allowance_ms)
            ),
        }
    }
}

impl fmt::Display for Miss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence())
    }
}

impl Counters {
    /// The closest any signal came to tripping, for a turn that stayed.
    ///
    /// Only signals that were actually exercised count: a run of zero is not
    /// "one short of one", it is a thing that never happened, and reporting it
    /// would put a warning on every clean turn.
    pub fn closest_miss(&self) -> Option<Miss> {
        // The class budget can be tighter than the tier's own, so the failure
        // streak is judged against whichever would have fired.
        let class_budget = self
            .progress
            .error_class
            .and_then(|class| class.repeat_budget(self.progress.threshold));

        let candidates = [
            near(
                self.repetition.consecutive,
                self.repetition.threshold,
                |seen, allowed| Miss::Repeats { seen, allowed },
            ),
            near(
                self.repetition.span_repeats,
                self.repetition.threshold,
                |seen, allowed| Miss::Span { seen, allowed },
            ),
            near(
                self.progress.same_run,
                self.progress.threshold,
                |seen, allowed| Miss::SameCall { seen, allowed },
            ),
            near(
                self.progress.failure_run,
                self.progress.threshold,
                |seen, allowed| Miss::Failures { seen, allowed },
            ),
            class_budget.and_then(|allowed| {
                near(self.progress.error_run, allowed, |seen, allowed| {
                    Miss::SameError {
                        class: self
                            .progress
                            .error_class
                            .unwrap_or(crate::detect::ErrorClass::Other),
                        seen,
                        allowed,
                    }
                })
            }),
            near(self.steps_used, self.steps_allowed, |used, allowed| {
                Miss::Steps { used, allowed }
            }),
            self.nearly_timed_out(),
        ];

        candidates
            .into_iter()
            .flatten()
            .max_by(|a, b| a.closeness().total_cmp(&b.closeness()))
    }

    /// Whether a wait came close enough to its own allowance to be worth saying.
    fn nearly_timed_out(&self) -> Option<Miss> {
        let fraction = self.timing.worst_fraction()?;
        (fraction >= ALMOST_WAIT).then_some(Miss::Wait {
            gap_ms: self.timing.worst_gap_ms,
            allowance_ms: self.timing.worst_allowance_ms,
            phase: self.timing.worst_phase,
        })
    }
}

/// A one-short-of-tripping signal, or nothing when the count never got there.
fn near(count: usize, allowed: usize, build: impl Fn(usize, usize) -> Miss) -> Option<Miss> {
    // `count > 0` first: a signal that never happened is not one short of
    // happening, and without this every clean turn would report a near miss.
    (allowed > 1 && count > 0 && count + 1 >= allowed).then(|| build(count, allowed))
}

fn share(seen: usize, allowed: usize) -> f64 {
    if allowed == 0 {
        return 0.0;
    }
    seen as f64 / allowed as f64
}

/// Seconds with one decimal, so a 8.4s gap against a 10s allowance does not read
/// as a whole second and look further away than it was.
fn ms_to_seconds(ms: u64) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

/// A stall that happened, with everything behind it.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub tier_id: String,
    pub tier_name: String,
    pub reason: StuckReason,
    pub counters: Counters,
}

impl Verdict {
    /// The machine-readable name of what tripped.
    ///
    /// A stable token rather than the prose summary: this is what a log is
    /// grouped and counted by, and wording that reads well in a transcript is
    /// the wrong thing to match on.
    pub fn trigger(&self) -> &'static str {
        trigger_of(&self.reason)
    }

    /// The `/why` report: the verdict first, then every counter it was decided
    /// against, including the ones that never fired.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} was abandoned: {}\n",
            self.tier_name,
            self.reason.summary()
        ));

        let c = &self.counters;
        let steps = &c.steps_used;
        out.push_str("\ndecided from:\n");
        out.push_str(&format!(
            "  steps       {steps} of {} used\n",
            c.steps_allowed
        ));
        out.push_str(&format!(
            "  repeated    {} identical line(s) in a row, allowed {}\n",
            c.repetition.consecutive, c.repetition.threshold
        ));
        out.push_str(&format!(
            "  spans       worst recurring span seen {} time(s), allowed {}\n",
            c.repetition.span_repeats, c.repetition.threshold
        ));
        out.push_str(&format!(
            "  calls       {} identical call(s) in a row, allowed {}\n",
            c.progress.same_run, c.progress.threshold
        ));
        out.push_str(&format!(
            "  failures    {} in a row, allowed {}\n",
            c.progress.failure_run, c.progress.threshold
        ));
        match c.progress.error_class {
            Some(class) => out.push_str(&format!(
                "  same error  {} x{} (budget {})\n",
                class,
                c.progress.error_run,
                class
                    .repeat_budget(c.progress.threshold)
                    .map(|budget| budget.to_string())
                    .unwrap_or_else(|| "none of its own".to_string())
            )),
            None => out.push_str("  same error  none\n"),
        }
        // Nothing waited on is not a wait of zero, and printing a 0.0s
        // allowance would read as a broken figure rather than an absent one.
        if c.timing.worst_allowance_ms == 0 {
            out.push_str("  silence     nothing was waited on\n");
        } else {
            out.push_str(&format!(
                "  silence     worst {}s of a {}s {} allowance\n",
                ms_to_seconds(c.timing.worst_gap_ms),
                ms_to_seconds(c.timing.worst_allowance_ms),
                c.timing.worst_phase
            ));
        }
        out.push_str(&format!(
            "  budgets     {}s first token, {}s idle, rate limited at {} repeats\n",
            ms_to_seconds(c.timing.first_token_ms),
            ms_to_seconds(c.timing.idle_ms),
            c.repetition.threshold
        ));

        out.trim_end().to_string()
    }
}

/// The trigger token for a reason.
pub fn trigger_of(reason: &StuckReason) -> &'static str {
    match reason {
        StuckReason::Repetition { .. } => "repetition",
        StuckReason::RepeatedToolCall { .. } => "identical_tool_call",
        StuckReason::RepeatedToolFailure { .. } => "tool_failures",
        StuckReason::RepeatedToolError { .. } => "same_tool_error",
        StuckReason::Stall { .. } => "stalled",
        StuckReason::StepLimit { .. } => "step_limit",
        StuckReason::Failed { .. } => "failed",
        StuckReason::Cancelled => "cancelled",
    }
}

/// What ran in place of the tier that stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The turn was handed to the tier below.
    Escalate,
    /// The tier below was asked one question instead.
    Consult,
    /// Neither: there was no tier below, so the turn ended here.
    ///
    /// Worth logging rather than discarding, because this is the *only* outcome
    /// a single-tier setup can ever have. A log that recorded only handovers
    /// would be empty for exactly the user the local-first promise is for, and
    /// the thresholds for a local tier are the ones most worth tuning.
    Ended,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Escalate => "escalate",
            Self::Consult => "consult",
            Self::Ended => "ended",
        }
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line of the spill log.
#[derive(Debug, Clone)]
pub struct SpillEntry {
    pub at: String,
    /// Which turn of the conversation this was, counted in user messages.
    pub turn: usize,
    /// The tier that was abandoned.
    pub from: String,
    /// What took over: the next tier, or the consultant that was asked. `None`
    /// when nothing did.
    pub to: Option<String>,
    pub policy: Policy,
    /// The machine-readable token for what tripped.
    ///
    /// Carried as its own field rather than derived from the prose at write
    /// time: a log is grouped and counted by this, and matching on wording that
    /// exists to read well in a transcript is how a log quietly stops working
    /// the first time a message is reworded.
    pub trigger: &'static str,
    pub reason: String,
    pub counters: Counters,
}

impl SpillEntry {
    /// The record for one verdict.
    ///
    /// The only way an entry is built, so the trigger cannot drift from the
    /// reason it describes and the tier that stalled cannot be misnamed.
    pub fn from_verdict(
        verdict: &Verdict,
        at: String,
        turn: usize,
        to: Option<String>,
        policy: Policy,
    ) -> Self {
        Self {
            at,
            turn,
            from: verdict.tier_id.clone(),
            to,
            policy,
            trigger: verdict.trigger(),
            reason: verdict.reason.summary(),
            counters: verdict.counters,
        }
    }

    /// The record, with the counters nested so the schema reads as the shape of
    /// the decision rather than as a flat list of numbers.
    pub fn to_json(&self) -> serde_json::Value {
        let c = &self.counters;
        serde_json::json!({
            "at": self.at,
            "turn": self.turn,
            "from": self.from,
            "to": self.to,
            "trigger": self.trigger,
            "policy": self.policy.as_str(),
            "reason": self.reason,
            "steps": { "used": c.steps_used, "allowed": c.steps_allowed },
            "repeats": {
                "lines": c.repetition.consecutive,
                "span": c.repetition.span_repeats,
                "allowed": c.repetition.threshold,
            },
            "calls": {
                "identical": c.progress.same_run,
                "failures": c.progress.failure_run,
                "same_error": c.progress.error_run,
                "class": c.progress.error_class.map(|class| class.label()),
                "allowed": c.progress.threshold,
            },
            "wait": {
                "worst_ms": c.timing.worst_gap_ms,
                "allowance_ms": c.timing.worst_allowance_ms,
                "phase": c.timing.worst_phase.to_string(),
                "first_token_ms": c.timing.first_token_ms,
                "idle_ms": c.timing.idle_ms,
            },
        })
    }
}

/// Where spills are written.
///
/// Beside the sessions, under the platform's state directory — on Linux,
/// `~/.local/state/spill/spills.jsonl`. State rather than config or cache: this
/// is a record of what happened, which is neither something the user edits nor
/// something safe to throw away.
#[derive(Debug, Clone)]
pub struct SpillLog {
    path: PathBuf,
}

impl SpillLog {
    /// The default location, or `None` when there is no state directory to
    /// write to.
    pub fn default_path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "spill")?;
        Some(dirs.state_dir()?.join("spills.jsonl"))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one spill. JSON Lines, one record per line, so the file can be
    /// read a line at a time and a truncated last line costs one record rather
    /// than the whole file.
    pub fn append(&self, entry: &SpillEntry) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // Formatted into one buffer and written in one call, deliberately. The
        // `Display` impl for a JSON value writes in many small pieces, and each
        // piece was its own `write` — so two processes spilling at once
        // interleaved mid-record and produced lines that were not JSON at all.
        // An append is atomic against other appends, but only for a single
        // write, so the record has to be whole before it reaches the file.
        let mut line = entry.to_json().to_string();
        line.push('\n');
        file.write_all(line.as_bytes())
    }
}

/// The current time as RFC3339 in UTC.
///
/// Hand-rolled rather than pulled in: the tree has no date library, and a
/// timestamp is not worth one. Sorts lexicographically, which is why the log is
/// readable in order without parsing.
pub fn timestamp(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// The civil date for a count of days since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`, which is the standard way to do this
/// without a library: it shifts the year to start in March so the leap day lands
/// at the end, where it cannot disturb the month arithmetic.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{ErrorClass, Phase};

    /// The README with its line endings normalised.
    ///
    /// Git checks the file out with CRLF on Windows, and `include_str!` embeds
    /// the bytes as they are — so a sample copied out of it has `\r\n` where the
    /// program's own output has `\n`, and the same content compares unequal on
    /// one platform and not another. The invariant is about what the text says,
    /// never about how the file happens to be stored.
    fn readme() -> String {
        include_str!("../README.md").replace("\r\n", "\n")
    }

    fn counters() -> Counters {
        Counters {
            steps_used: 1,
            steps_allowed: 12,
            repetition: RepetitionCounters {
                consecutive: 1,
                span_repeats: 1,
                threshold: 4,
            },
            progress: ProgressCounters {
                same_run: 1,
                failure_run: 0,
                error_run: 0,
                error_class: None,
                threshold: 4,
            },
            timing: Timing {
                worst_gap_ms: 100,
                worst_allowance_ms: 10_000,
                first_token_ms: 60_000,
                idle_ms: 10_000,
                worst_phase: Phase::Idle,
            },
        }
    }

    // ---- near misses -------------------------------------------------------

    #[test]
    fn a_clean_turn_reports_no_near_miss() {
        assert_eq!(counters().closest_miss(), None);
    }

    #[test]
    fn a_run_one_short_of_the_threshold_is_nearly_a_loop() {
        let counters = Counters {
            repetition: RepetitionCounters {
                consecutive: 3,
                span_repeats: 1,
                threshold: 4,
            },
            ..counters()
        };
        assert_eq!(
            counters.closest_miss(),
            Some(Miss::Repeats {
                seen: 3,
                allowed: 4
            })
        );
    }

    #[test]
    fn two_short_of_the_threshold_is_not_nearly_anything() {
        let counters = Counters {
            repetition: RepetitionCounters {
                consecutive: 2,
                span_repeats: 1,
                threshold: 4,
            },
            ..counters()
        };
        assert_eq!(counters.closest_miss(), None);
    }

    #[test]
    fn a_clean_turn_does_not_report_a_near_miss_on_a_signal_it_never_used() {
        // A count of zero is not one short of one. Without this rule every turn
        // that touched a tool would warn, and the warning would mean nothing.
        let counters = Counters {
            steps_used: 0,
            ..counters()
        };
        assert_eq!(counters.closest_miss(), None);
    }

    #[test]
    fn the_last_step_is_nearly_out_of_steps() {
        let counters = Counters {
            steps_used: 11,
            steps_allowed: 12,
            ..counters()
        };
        assert_eq!(
            counters.closest_miss(),
            Some(Miss::Steps {
                used: 11,
                allowed: 12
            })
        );
    }

    #[test]
    fn a_wait_that_used_most_of_its_allowance_counts_as_nearly_spilled() {
        let counters = Counters {
            timing: Timing {
                worst_gap_ms: 8_400,
                worst_allowance_ms: 10_000,
                ..counters().timing
            },
            ..counters()
        };
        assert_eq!(
            counters.closest_miss(),
            Some(Miss::Wait {
                gap_ms: 8_400,
                allowance_ms: 10_000,
                phase: Phase::Idle,
            })
        );
    }

    #[test]
    fn a_wait_that_used_half_its_allowance_is_not_worth_saying() {
        let counters = Counters {
            timing: Timing {
                worst_gap_ms: 5_000,
                worst_allowance_ms: 10_000,
                ..counters().timing
            },
            ..counters()
        };
        assert_eq!(counters.closest_miss(), None);
    }

    #[test]
    fn a_wait_is_judged_against_the_budget_that_applied_to_it() {
        // The point of carrying the allowance beside the gap: 8s is unremarkable
        // against a 60s first-token budget and nearly fatal against a 10s idle
        // one, and a bare gap cannot tell the two apart.
        let generous = Counters {
            timing: Timing {
                worst_gap_ms: 8_000,
                worst_allowance_ms: 60_000,
                worst_phase: Phase::FirstToken,
                ..counters().timing
            },
            ..counters()
        };
        assert_eq!(generous.closest_miss(), None, "a slow start is not a stall");

        let tight = Counters {
            timing: Timing {
                worst_gap_ms: 8_000,
                worst_allowance_ms: 10_000,
                ..generous.timing
            },
            ..generous
        };
        assert!(
            matches!(tight.closest_miss(), Some(Miss::Wait { .. })),
            "the same silence against the idle budget is nearly a stall"
        );
    }

    #[test]
    fn the_closest_signal_is_the_one_reported() {
        // One line is the whole point, so several near misses resolve to the
        // nearest. Three of four repeats is 0.75 of the way there and eleven of
        // twelve steps is 0.92, so the steps are what gets said.
        let counters = Counters {
            steps_used: 11,
            steps_allowed: 12,
            repetition: RepetitionCounters {
                consecutive: 3,
                span_repeats: 1,
                threshold: 4,
            },
            ..counters()
        };
        assert_eq!(
            counters.closest_miss(),
            Some(Miss::Steps {
                used: 11,
                allowed: 12
            })
        );
    }

    #[test]
    fn a_failure_streak_is_judged_against_the_tighter_class_budget() {
        // A class with a budget of its own can fire before the tier's allowance,
        // so the near miss has to be measured against whichever would have fired.
        let counters = Counters {
            progress: ProgressCounters {
                same_run: 1,
                failure_run: 2,
                error_run: 2,
                error_class: Some(ErrorClass::NotFound),
                threshold: 5,
            },
            ..counters()
        };
        assert_eq!(
            counters.closest_miss(),
            Some(Miss::SameError {
                class: ErrorClass::NotFound,
                seen: 2,
                allowed: 3,
            }),
            "three is the class budget here, not the tier's five"
        );
    }

    #[test]
    fn every_near_miss_reads_as_a_sentence() {
        for miss in [
            Miss::Repeats {
                seen: 3,
                allowed: 4,
            },
            Miss::Span {
                seen: 2,
                allowed: 3,
            },
            Miss::SameCall {
                seen: 2,
                allowed: 3,
            },
            Miss::Failures {
                seen: 2,
                allowed: 3,
            },
            Miss::SameError {
                class: ErrorClass::NotFound,
                seen: 2,
                allowed: 3,
            },
            Miss::Steps {
                used: 11,
                allowed: 12,
            },
            Miss::Wait {
                gap_ms: 8_400,
                allowance_ms: 10_000,
                phase: Phase::Idle,
            },
        ] {
            let sentence = miss.sentence();
            assert!(!sentence.is_empty(), "{miss:?}");
            assert!(
                sentence.chars().next().is_some_and(|c| c.is_lowercase()),
                "it reads as a clause, not a heading: {sentence}"
            );
        }
    }

    #[test]
    fn a_nearly_spilled_wait_reads_as_a_fraction_of_a_second() {
        let sentence = Miss::Wait {
            gap_ms: 8_400,
            allowance_ms: 10_000,
            phase: Phase::Idle,
        }
        .sentence();
        assert!(sentence.contains("8.4s"), "{sentence}");
        assert!(sentence.contains("10.0s"), "{sentence}");
        assert!(sentence.contains("idle"), "{sentence}");
    }

    // ---- triggers ----------------------------------------------------------

    #[test]
    fn each_reason_has_its_own_stable_token() {
        use crate::detect::StuckReason as R;
        let cases: Vec<(R, &str)> = vec![
            (
                R::Repetition {
                    sample: "x".into(),
                    repeats: 4,
                },
                "repetition",
            ),
            (
                R::RepeatedToolCall {
                    tool: "read_file".into(),
                    times: 4,
                },
                "identical_tool_call",
            ),
            (
                R::RepeatedToolFailure {
                    tool: "read_file".into(),
                    times: 4,
                },
                "tool_failures",
            ),
            (
                R::RepeatedToolError {
                    tool: "read_file".into(),
                    class: ErrorClass::NotFound,
                    times: 3,
                },
                "same_tool_error",
            ),
            (R::Stall { seconds: 30 }, "stalled"),
            (R::StepLimit { steps: 12 }, "step_limit"),
            (
                R::Failed {
                    detail: "reset".into(),
                },
                "failed",
            ),
            (R::Cancelled, "cancelled"),
        ];

        for (reason, expected) in cases {
            assert_eq!(trigger_of(&reason), expected, "{reason:?}");
        }
    }

    #[test]
    fn a_token_is_not_the_prose_it_summarises() {
        // The log is counted by the token, so it must not be the sentence that
        // happens to be printed in the transcript.
        let reason = StuckReason::Stall { seconds: 30 };
        assert_eq!(trigger_of(&reason), "stalled");
        assert_ne!(trigger_of(&reason), reason.summary());
    }

    // ---- the report --------------------------------------------------------

    #[test]
    fn the_report_shows_every_counter_not_only_the_one_that_fired() {
        // The reason says what tripped; the counters are what makes the call
        // arguable, so a report that only restated the reason would be useless.
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Looping Local".into(),
            reason: StuckReason::RepeatedToolError {
                tool: "read_file".into(),
                class: ErrorClass::NotFound,
                times: 3,
            },
            counters: Counters {
                steps_used: 6,
                progress: ProgressCounters {
                    same_run: 1,
                    failure_run: 3,
                    error_run: 3,
                    error_class: Some(ErrorClass::NotFound),
                    threshold: 4,
                },
                ..counters()
            },
        };

        let report = verdict.report();
        assert!(report.contains("Looping Local"), "{report}");
        assert!(
            report.contains("read_file failed 3 times"),
            "the verdict leads: {report}"
        );
        assert!(report.contains("6 of 12 used"), "{report}");
        assert!(report.contains("no such file x3"), "{report}");
        assert!(
            report.contains("budgets"),
            "the allowances are what a threshold is tuned against: {report}"
        );
        // The counters that did not fire are the point of the report.
        assert!(report.contains("identical line(s)"), "{report}");
    }

    #[test]
    fn the_report_of_a_realistic_stall_is_pinned() {
        // The whole block, because the README shows it and a format that drifts
        // from the documentation is worse than no documentation. If this fails,
        // the sample in the README needs the same change.
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Looping Local".into(),
            reason: StuckReason::RepeatedToolError {
                tool: "read_file".into(),
                class: ErrorClass::NotFound,
                times: 3,
            },
            counters: Counters {
                steps_used: 6,
                steps_allowed: 12,
                repetition: RepetitionCounters {
                    consecutive: 1,
                    span_repeats: 1,
                    threshold: 4,
                },
                progress: ProgressCounters {
                    same_run: 1,
                    failure_run: 3,
                    error_run: 3,
                    error_class: Some(ErrorClass::NotFound),
                    threshold: 4,
                },
                timing: Timing {
                    worst_gap_ms: 300,
                    worst_allowance_ms: 30_000,
                    first_token_ms: 120_000,
                    idle_ms: 30_000,
                    worst_phase: Phase::Idle,
                },
            },
        };

        let expected = "\
Looping Local was abandoned: read_file failed 3 times with the same error (no such file)

decided from:
  steps       6 of 12 used
  repeated    1 identical line(s) in a row, allowed 4
  spans       worst recurring span seen 1 time(s), allowed 4
  calls       1 identical call(s) in a row, allowed 4
  failures    3 in a row, allowed 4
  same error  no such file x3 (budget 3)
  silence     worst 0.3s of a 30.0s idle allowance
  budgets     120.0s first token, 30.0s idle, rate limited at 4 repeats";

        assert_eq!(verdict.report(), expected);
        // And the README is showing this, not an invented version of it.
        assert!(
            readme().contains(expected),
            "the README no longer shows the real report"
        );
    }

    #[test]
    fn the_record_the_readme_shows_is_the_record_that_is_written() {
        // The same guarantee for the log sample. Serialisation order is
        // whatever the JSON map does, which is not the order the fields are
        // written in the code — so a hand-copied sample would be wrong in a way
        // nobody would notice until they went looking for a key.
        let entry = SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 7,
            from: "local".into(),
            to: Some("frontier".into()),
            policy: Policy::Escalate,
            trigger: "same_tool_error",
            reason: "read_file failed 3 times with the same error (no such file)".into(),
            counters: Counters {
                steps_used: 6,
                steps_allowed: 12,
                progress: ProgressCounters {
                    same_run: 1,
                    failure_run: 3,
                    error_run: 3,
                    error_class: Some(ErrorClass::NotFound),
                    threshold: 4,
                },
                timing: Timing {
                    worst_gap_ms: 300,
                    worst_allowance_ms: 30_000,
                    first_token_ms: 120_000,
                    idle_ms: 30_000,
                    worst_phase: Phase::Idle,
                },
                ..counters()
            },
        };

        let written = entry.to_json().to_string();
        assert!(
            readme().contains(&written),
            "the README no longer shows the record that is actually written:\n{written}"
        );
    }

    #[test]
    fn the_silence_line_names_the_budget_the_wait_was_measured_against() {
        // Read off the screen: this said "0.0s of a 10.0s idle allowance" when
        // 10s was the first-token budget and the idle one was 20s. The number
        // was right and the word was wrong, which is the kind of error a reader
        // cannot see through.
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Local".into(),
            reason: StuckReason::Repetition {
                sample: "x".into(),
                repeats: 4,
            },
            counters: Counters {
                timing: Timing {
                    worst_gap_ms: 0,
                    worst_allowance_ms: 10_000,
                    first_token_ms: 10_000,
                    idle_ms: 20_000,
                    worst_phase: Phase::FirstToken,
                },
                ..counters()
            },
        };

        let report = verdict.report();
        assert!(
            report.contains("worst 0.0s of a 10.0s first token allowance"),
            "{report}"
        );
        assert!(
            !report.contains("10.0s idle"),
            "the idle budget is 20s: {report}"
        );
    }

    #[test]
    fn a_report_with_no_class_says_so_rather_than_printing_nothing() {
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Local".into(),
            reason: StuckReason::StepLimit { steps: 12 },
            counters: counters(),
        };
        let report = verdict.report();
        assert!(report.contains("same error  none"), "{report}");
    }

    #[test]
    fn the_trigger_of_a_verdict_matches_its_reason() {
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Local".into(),
            reason: StuckReason::StepLimit { steps: 12 },
            counters: counters(),
        };
        assert_eq!(verdict.trigger(), "step_limit");
    }

    // ---- the log -----------------------------------------------------------

    #[test]
    fn a_record_carries_what_the_log_is_for() {
        let entry = SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 3,
            from: "local".into(),
            to: Some("deepseek".into()),
            policy: Policy::Escalate,
            trigger: "same_tool_error",
            reason: "read_file failed 3 times with the same error (no such file)".into(),
            counters: counters(),
        };

        let json = entry.to_json();
        assert_eq!(json["from"], "local");
        assert_eq!(json["to"], "deepseek");
        assert_eq!(json["policy"], "escalate");
        assert_eq!(json["trigger"], "same_tool_error");
        assert_eq!(json["turn"], 3);
        assert!(json["reason"].as_str().unwrap().contains("read_file"));
    }

    #[test]
    fn a_record_carries_the_counters_a_threshold_is_tuned_from() {
        let entry = SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 1,
            from: "local".into(),
            to: Some("deepseek".into()),
            policy: Policy::Consult,
            trigger: "repetition",
            reason: "repeated the same output 4 times".into(),
            counters: Counters {
                steps_used: 2,
                steps_allowed: 12,
                repetition: RepetitionCounters {
                    consecutive: 4,
                    span_repeats: 3,
                    threshold: 4,
                },
                timing: Timing {
                    worst_gap_ms: 9_500,
                    worst_allowance_ms: 10_000,
                    ..counters().timing
                },
                ..counters()
            },
        };

        let json = entry.to_json();
        assert_eq!(json["steps"]["used"], 2);
        assert_eq!(json["steps"]["allowed"], 12);
        assert_eq!(json["repeats"]["lines"], 4);
        assert_eq!(json["repeats"]["allowed"], 4);
        assert_eq!(json["wait"]["worst_ms"], 9_500);
        assert_eq!(json["wait"]["allowance_ms"], 10_000);
        assert_eq!(json["policy"], "consult");
        // Exactly the numbers, so a threshold can be moved on evidence.
        assert_eq!(json["calls"]["allowed"], 4);
    }

    #[test]
    fn an_entry_built_from_a_verdict_cannot_misname_its_trigger() {
        // The constructor exists so the token and the prose cannot disagree;
        // hand-building an entry is what the field is for, and this is the
        // guarantee that the one path actually used gets it right.
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Loopy".into(),
            reason: StuckReason::RepeatedToolCall {
                tool: "list_dir".into(),
                times: 4,
            },
            counters: counters(),
        };

        let entry = SpillEntry::from_verdict(
            &verdict,
            "2026-09-12T10:00:00Z".into(),
            2,
            Some("deepseek".into()),
            Policy::Consult,
        );

        assert_eq!(entry.trigger, "identical_tool_call");
        assert_eq!(entry.from, "local");
        assert_eq!(entry.to.as_deref(), Some("deepseek"));
        assert_eq!(entry.policy, Policy::Consult);
        assert_eq!(entry.turn, 2);
        assert_eq!(entry.to_json()["trigger"], "identical_tool_call");
        assert!(entry.reason.contains("list_dir"), "{}", entry.reason);
    }

    #[test]
    fn a_turn_that_ended_for_want_of_a_tier_says_so_rather_than_naming_one() {
        // The single-tier case. Recording it as an escalate would name a tier
        // that never ran, which is the kind of log that teaches nothing.
        let verdict = Verdict {
            tier_id: "local".into(),
            tier_name: "Local".into(),
            reason: StuckReason::StepLimit { steps: 12 },
            counters: counters(),
        };
        let entry = SpillEntry::from_verdict(
            &verdict,
            "2026-09-12T10:00:00Z".into(),
            1,
            None,
            Policy::Ended,
        );

        assert_eq!(entry.policy.as_str(), "ended");
        let json = entry.to_json();
        assert!(json["to"].is_null(), "{json}");
        assert_eq!(json["trigger"], "step_limit");
    }

    #[test]
    fn an_absent_error_class_is_null_rather_than_a_guess() {
        let entry = SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 1,
            from: "a".into(),
            to: Some("b".into()),
            policy: Policy::Escalate,
            trigger: "stalled",
            reason: "went quiet for 30s".into(),
            counters: counters(),
        };
        assert!(entry.to_json()["calls"]["class"].is_null());
    }

    #[test]
    fn the_log_is_json_lines_one_record_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SpillLog::at(dir.path().join("nested/spills.jsonl"));

        for turn in 1..=3 {
            log.append(&SpillEntry {
                at: format!("2026-09-12T10:00:0{turn}Z"),
                turn,
                from: "local".into(),
                to: Some("deepseek".into()),
                policy: Policy::Escalate,
                trigger: "stalled",
                reason: "went quiet for 30s".into(),
                counters: counters(),
            })
            .expect("append");
        }

        let text = std::fs::read_to_string(log.path()).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one record per line: {text}");
        for line in lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}"));
            assert_eq!(parsed["from"], "local");
        }
        // Parent directories are made on the way, so a first spill on a fresh
        // machine is not the one that fails.
        assert!(log.path().parent().expect("parent").is_dir());
    }

    #[test]
    fn appending_keeps_what_was_there_before() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SpillLog::at(dir.path().join("spills.jsonl"));
        let entry = |reason: &str| SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 1,
            from: "a".into(),
            to: Some("b".into()),
            policy: Policy::Escalate,
            trigger: "stalled",
            reason: reason.to_string(),
            counters: counters(),
        };

        log.append(&entry("first")).expect("append");
        log.append(&entry("second")).expect("append");

        let text = std::fs::read_to_string(log.path()).expect("read");
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("second"), "{text}");
    }

    #[test]
    fn records_from_racing_writers_do_not_interleave() {
        // Every process on the machine shares one log file, and two that spill at
        // the same moment must not corrupt each other's lines. This is not
        // hypothetical: the suite itself did it, writing from several threads at
        // once, and left two records spliced together that no parser would read.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SpillLog::at(dir.path().join("spills.jsonl"));

        std::thread::scope(|scope| {
            for writer in 0..8 {
                let log = log.clone();
                scope.spawn(move || {
                    for turn in 0..25 {
                        log.append(&SpillEntry {
                            at: "2026-09-12T10:00:00Z".into(),
                            turn,
                            from: format!("writer-{writer}"),
                            to: Some("b".into()),
                            policy: Policy::Escalate,
                            trigger: "stalled",
                            reason: "went quiet for 30s".into(),
                            counters: counters(),
                        })
                        .expect("append");
                    }
                });
            }
        });

        let text = std::fs::read_to_string(log.path()).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 200, "every record should be its own line");
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("a torn record: {error}\n{line}"));
            assert!(parsed["from"].as_str().unwrap().starts_with("writer-"));
        }
    }

    #[test]
    fn an_unwritable_log_reports_the_failure_rather_than_panicking() {
        // The caller decides what to do about it; the point here is that the
        // result exists to be inspected instead of the write being unwrapped.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SpillLog::at(dir.path().join("spills.jsonl"));
        std::fs::create_dir_all(log.path()).expect("make a directory where the file goes");

        let failed = log.append(&SpillEntry {
            at: "2026-09-12T10:00:00Z".into(),
            turn: 1,
            from: "a".into(),
            to: Some("b".into()),
            policy: Policy::Escalate,
            trigger: "stalled",
            reason: "went quiet for 30s".into(),
            counters: counters(),
        });
        assert!(
            failed.is_err(),
            "a path that is a directory is not writable"
        );
    }

    // ---- timestamps --------------------------------------------------------

    #[test]
    fn the_epoch_is_1970() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_known_moment_is_rendered_correctly() {
        // 2026-09-12T10:00:00Z, which is what the log lines in these tests say.
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_789_207_200);
        assert_eq!(timestamp(at), "2026-09-12T10:00:00Z");
    }

    #[test]
    fn a_leap_day_is_rendered_correctly() {
        // 2024-02-29, the case a naive month table gets wrong.
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800);
        assert_eq!(timestamp(at), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn the_last_second_of_a_year_does_not_roll_over_early() {
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_735_689_599);
        assert_eq!(timestamp(at), "2024-12-31T23:59:59Z");
        let next = at + std::time::Duration::from_secs(1);
        assert_eq!(timestamp(next), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn a_timestamp_sorts_in_chronological_order() {
        // The reason for this format over a raw epoch: the file reads in order
        // without anything having to parse it.
        let earlier = timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_789_207_200));
        let later = timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_789_207_201));
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn a_time_before_the_epoch_does_not_panic_or_wrap() {
        // Clock skew is real; a negative duration must not become a huge year.
        let before = UNIX_EPOCH - std::time::Duration::from_secs(60);
        assert_eq!(timestamp(before), "1970-01-01T00:00:00Z");
    }
}
