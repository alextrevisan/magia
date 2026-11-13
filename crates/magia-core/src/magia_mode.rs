//! The Magia agent: magia's own "ultrawork". A user request runs in rounds; when the model
//! stops while its task list still has open items, the agent adds a continuation message and
//! runs another round, until the list is closed and an independent reviewer agrees.
//!
//! This file holds the texts and the pure decisions; the loop itself is in `agent.rs`.

use magia_tools::Todo;
use magia_types::Message;
use serde::Deserialize;

/// Starts every continuation message (a `Message::User` the agent writes, not the person).
pub const CONTINUE_PREFIX: &str = "[magia: continue]";

/// Rounds in a row that change neither the task list nor any file before the loop gives up.
pub const MAX_IDLE_ROUNDS: u32 = 3;
/// Independent reviews per request.
pub const MAX_REVIEWS: u32 = 2;
const MAX_REQUEST_CHARS: usize = 2000;

/// `"magia": { "magia_mode": { … } }` in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MagiaMode {
    /// Rounds per request (each round has the usual step limit).
    pub max_rounds: u32,
    /// Ask a read-only reviewer to check the work before finishing.
    pub review: bool,
}

impl Default for MagiaMode {
    fn default() -> Self {
        Self {
            max_rounds: 30,
            review: true,
        }
    }
}

/// Added to the system prompt while the Magia agent is selected.
pub const INSTRUCTIONS: &str = "\
# Magia mode

You are in Magia mode: the user hands you a task and expects it finished and verified, not a \
plan or a progress report. Work autonomously until the job is done.

- Start by understanding: read the relevant code and find how the project builds and tests.
- For anything with more than a couple of steps, write the plan with `todo` right away and keep \
it current: exactly one item `in_progress`, mark items `completed` only when they are really done \
(not with failing tests, partial code or unresolved errors), add items you discover. The last \
item is always verification: build, run the tests and linters, run the thing you changed.
- When you stop while items are still `pending` or `in_progress`, magia sends you a \
continuation message and you keep going. Do not ask whether to continue; continue.
- Mark an item `cancelled` when you decide not to do it (out of scope, impossible, not needed) \
and say why in your reply. Never mark something `completed` that is not done.
- A denied tool call or a refusal by the user is final for what that call was meant to do: never \
reach the same result through another tool, command, path or host. If it blocks the task, cancel \
the affected items, explain, and stop.
- Use `question` only for decisions that are genuinely the user's (ambiguous scope that is \
expensive to redo, a trade-off they must choose, an irreversible step). Do not end a round with a \
question in plain text while work remains: either ask with `question` or decide, say what you \
assumed, and continue.
- Use `task` for broad read-only searches whose details you do not need, to keep your context \
small.
- When everything is done, an independent reviewer may check the changed files. If it reports \
problems, fix the real ones; if a point is wrong, say why in one line.
- Finish with a short report: what changed (paths), how it was verified, anything left open.";

/// Why the agent starts another round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continue {
    /// Items still open after the model stopped.
    OpenTasks,
    /// The round hit the step limit.
    StepLimit,
    /// The reviewer found problems.
    Review(Vec<String>),
}

impl Continue {
    /// One line for the transcript divider.
    pub fn label(&self, open: usize) -> String {
        match self {
            Continue::OpenTasks => format!("{open} task(s) left"),
            Continue::StepLimit => "step limit reached, continuing".into(),
            Continue::Review(issues) => format!("review found {} problem(s)", issues.len()),
        }
    }
}

/// Items not finished yet (`pending` or `in_progress`).
pub fn open_tasks(todos: &[Todo]) -> usize {
    todos
        .iter()
        .filter(|t| matches!(t.status.as_str(), "pending" | "in_progress"))
        .count()
}

/// The message that starts a new round. It repeats the request and the task list, so it still
/// makes sense after older history was compacted away.
pub fn continuation(request: &str, todos: &[Todo], why: &Continue) -> String {
    let mut s = format!("{CONTINUE_PREFIX}\n");
    match why {
        Continue::OpenTasks => s.push_str(
            "You stopped, but the task list still has open items. Continue with the next one now. \
If you believe the work is already complete, re-check each open item skeptically, verify it, and \
update the list (`completed` only if truly done, `cancelled` with a reason if you will not do it).",
        ),
        Continue::StepLimit => s.push_str(
            "The previous round reached its step limit. Continue where you left off; batch \
independent reads in one response and avoid redundant checks.",
        ),
        Continue::Review(issues) => {
            s.push_str(
                "An independent review of your changes reported these problems. Fix the real ones \
(add them to the task list), say in one line why any point is wrong, then finish.\n",
            );
            for i in issues {
                s.push_str(&format!("- {i}\n"));
            }
        }
    }
    s.push_str(&format!(
        "\n\n<original_request>\n{}\n</original_request>",
        clip(request.trim(), MAX_REQUEST_CHARS)
    ));
    if !todos.is_empty() {
        s.push_str("\n\n<task_list>\n");
        for t in todos {
            s.push_str(&format!("- [{}] {}\n", t.status, t.content));
        }
        s.push_str("</task_list>");
    }
    s
}

pub fn is_continuation(m: &Message) -> bool {
    matches!(m, Message::User { text, .. } if text.starts_with(CONTINUE_PREFIX))
}

/// Shown to the reviewer subagent (with the usual read-only tools).
pub const REVIEWER: &str = "\
You are an independent reviewer. Another agent says it finished the request below. You did not \
write the code. Check skeptically whether the request is really done: read the changed files and \
whatever they touch. Look for missing parts of the request, placeholder or stub code, TODOs left \
behind, code that cannot compile or obviously fails, broken callers, and changes the request did \
not ask for. Do not report style preferences or optional improvements. You cannot run commands.

Reply with JSON only, as the last thing you write:
{\"verdict\": \"pass\" | \"fail\", \"issues\": [\"<one concrete problem per entry, with the file>\"]}";

pub fn review_prompt(request: &str, todos: &[Todo], changed: &[String], claim: &str) -> String {
    let mut p = format!(
        "<original_request>\n{}\n</original_request>\n\n<changed_files>\n",
        clip(request.trim(), MAX_REQUEST_CHARS)
    );
    for f in changed {
        p.push_str(&format!("{f}\n"));
    }
    p.push_str("</changed_files>\n");
    if !todos.is_empty() {
        p.push_str("\n<task_list>\n");
        for t in todos {
            p.push_str(&format!("- [{}] {}\n", t.status, t.content));
        }
        p.push_str("</task_list>\n");
    }
    p.push_str(&format!(
        "\n<agent_final_message>\n{}\n</agent_final_message>\n\nReview the work and reply with the JSON verdict.",
        clip(claim.trim(), 4000)
    ));
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(Vec<String>),
    /// No JSON verdict in the reply. Treated as a pass (with a notice), so a confused reviewer
    /// cannot keep the loop going.
    Unreadable,
}

/// Read the reviewer's reply. A `fail` without concrete issues is not worth another round.
pub fn parse_review(reply: &str) -> Verdict {
    let Some(v) = reply
        .rfind('}')
        .and_then(|b| {
            reply[..b]
                .rmatch_indices('{')
                .find_map(|(a, _)| serde_json::from_str::<serde_json::Value>(&reply[a..=b]).ok())
        })
        .filter(|v| v.get("verdict").is_some())
    else {
        return Verdict::Unreadable;
    };
    let fail = v
        .get("verdict")
        .and_then(|d| d.as_str())
        .is_some_and(|d| d.trim().eq_ignore_ascii_case("fail"));
    let issues: Vec<String> = v
        .get("issues")
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| i.as_str())
                .map(|i| clip(i.trim(), 500))
                .filter(|i| !i.is_empty())
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    if fail && !issues.is_empty() {
        Verdict::Fail(issues)
    } else {
        Verdict::Pass
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head} […]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(c: &str, s: &str) -> Todo {
        Todo {
            content: c.into(),
            status: s.into(),
        }
    }

    #[test]
    fn only_pending_and_in_progress_are_open() {
        let t = [
            todo("a", "completed"),
            todo("b", "in_progress"),
            todo("c", "pending"),
            todo("d", "cancelled"),
        ];
        assert_eq!(open_tasks(&t), 2);
    }

    #[test]
    fn continuation_restates_the_request_and_the_list() {
        let m = continuation(
            "add a --json flag",
            &[todo("parse flag", "completed"), todo("tests", "pending")],
            &Continue::OpenTasks,
        );
        assert!(m.starts_with(CONTINUE_PREFIX));
        assert!(m.contains("add a --json flag"));
        assert!(m.contains("- [pending] tests"));
        assert!(is_continuation(&Message::user(m)));
        assert!(!is_continuation(&Message::user("hi")));
    }

    #[test]
    fn review_verdicts() {
        let fail = "Looked at it.\n{\"verdict\":\"fail\",\"issues\":[\"src/a.rs: stub\"]}";
        assert_eq!(
            parse_review(fail),
            Verdict::Fail(vec!["src/a.rs: stub".into()])
        );
        assert_eq!(
            parse_review("{\"verdict\":\"pass\",\"issues\":[]}"),
            Verdict::Pass
        );
        // A fail with nothing concrete is not worth another round.
        assert_eq!(
            parse_review("{\"verdict\":\"fail\",\"issues\":[]}"),
            Verdict::Pass
        );
        assert_eq!(parse_review("no idea"), Verdict::Unreadable);
        // Braces inside the issues do not confuse the parser.
        let nested = "x {\"verdict\":\"fail\",\"issues\":[\"fn f() {} is empty\"]}";
        assert_eq!(
            parse_review(nested),
            Verdict::Fail(vec!["fn f() {} is empty".into()])
        );
    }
}
