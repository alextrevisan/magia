//! The Magia agent: magia's own "ultrawork". A user request runs in rounds; when the model
//! stops while its task list still has open items, the agent adds a continuation message and
//! runs another round, until the list is closed and an independent reviewer agrees.
//!
//! This file holds the texts and the pure decisions; the loop itself is in `agent.rs`.

use magia_tools::Todo;
use magia_types::Message;
use magia_types::clip;
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

/// The read-only subagents: three the model calls through `task` (`agent` field), and the
/// reviewer magia itself calls at the end. Each can run on its own model
/// (`"magia_mode": {"models": {"explore": "provider/model", …}}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Explore,
    Plan,
    Oracle,
    /// Implements a well-defined part of the work; the only role that edits files.
    Worker,
    Review,
}

impl Role {
    pub const ALL: [Role; 5] = [
        Role::Explore,
        Role::Plan,
        Role::Oracle,
        Role::Worker,
        Role::Review,
    ];
    /// The ones the model can pick in `task`.
    pub const DELEGATED: [Role; 4] = [Role::Explore, Role::Plan, Role::Oracle, Role::Worker];

    pub fn name(self) -> &'static str {
        match self {
            Role::Explore => "explore",
            Role::Plan => "plan",
            Role::Oracle => "oracle",
            Role::Worker => "worker",
            Role::Review => "review",
        }
    }

    /// A config key or a `task` argument (`explorer`, `planner`, `reviewer` also work).
    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "explore" | "explorer" => Some(Role::Explore),
            "plan" | "planner" => Some(Role::Plan),
            "oracle" => Some(Role::Oracle),
            "worker" | "implementer" => Some(Role::Worker),
            "review" | "reviewer" => Some(Role::Review),
            _ => None,
        }
    }

    /// Added to the subagent's system prompt.
    pub fn prompt(self) -> &'static str {
        match self {
            Role::Explore => EXPLORE,
            Role::Plan => PLANNER,
            Role::Oracle => ORACLE,
            Role::Worker => WORKER,
            Role::Review => REVIEWER,
        }
    }
}

const EXPLORE: &str = "\
You are an explore subagent: a fast, read-only searcher. Find what the request asks for in the \
project (or the web, when asked) and stop. Batch independent searches and reads in one response. \
Reply with a concise report, no preamble: the concrete file paths (with line numbers when useful), \
what each one does, and the conventions you saw. Say what you could not find.";

const PLANNER: &str = "\
You are a planning subagent. You get a request and the context gathered so far; read whatever \
else you need (you cannot change anything). Reply with a work plan another agent will follow:
- the steps in order, each with the files to change, the change itself, and how to verify it \
(the exact command, test or observable result);
- which steps are independent of each other;
- 1-2 scenarios for a small change, 3+ for a risky or multi-part one, each with a pass condition \
that can fail, checked on the real thing (command output, a running program, a screenshot), not \
only \"tests pass\";
- open questions only the user can answer, and risks.
No code beyond short snippets. Do not pad the plan: the smallest one that fully does the request.";

const ORACLE: &str = "\
You are the oracle: a senior engineer consulted on a hard problem (architecture, a bug that \
resists, a plan that needs a second opinion). You can read and search but not change anything. \
Think it through carefully, check your assumptions against the code, and reply with: your \
diagnosis or recommendation, the reasoning in a few lines, the risks or cases the asker is \
missing, and a better alternative if there is one. Be direct; say when the asker is wrong.";

const WORKER: &str = "\
You are a worker subagent: another agent split a larger job and gave you one well-defined part. \
You can read, edit and run commands. Change only the files your instructions give you (other \
workers may be editing other files at the same time); if the part cannot be done without touching \
something else, stop and say so instead. Make the smallest complete change: no stubs, \
placeholders or simplified versions. Verify it the way the instructions say (build, tests, \
running it) and do not report success without that evidence. Reply with a short report: the files \
you changed, what you did, how you verified it (commands and results), and anything left open.";

/// Added to the system prompt while the Magia agent is selected. Adapted from the "ultrawork"
/// mode of oh-my-opencode: understand, plan, implement, then prove it works.
pub const INSTRUCTIONS: &str = "\
# Magia mode

You are in Magia mode: the user hands you a task and expects it finished and proven to work, \
not a plan, a demo or a progress report. Work autonomously until the job is done.

## 1. Understand before changing anything
- Find out what the user actually wants and how the project is built and tested. Do not start \
implementing while you are guessing which files to change or how the existing code works.
- Delegate searching: send `task` with `agent: \"explore\"` for each independent question, all in \
the same response (they run in parallel), and keep reading yourself for what you must see in full.
- Stuck on something hard (architecture, a bug that resists two attempts, a risky design)? Ask \
`task` with `agent: \"oracle\"`, giving it your plan and your doubts.
- If ambiguity remains after looking, and getting it wrong is expensive to redo, ask with \
`question`. Otherwise decide, say what you assumed, and go on.

## 2. Plan
- For anything with more than a couple of steps, ask `task` with `agent: \"plan\"` (pass the \
request and what you found), then write the plan with `todo`.
- Each item says what to do and how it will be verified: `path: change — verify by <check>`. \
Not \"implement feature\".
- Before coding, decide the scenarios that prove the work: 1-2 for a small change, 3+ for a risky \
or multi-part one. Each one has a pass condition that can fail, observed on the real thing.
- Keep the list current: exactly one item `in_progress`, mark `completed` right after it is done \
and verified, add what you discover. The last items are verification and cleanup.

## 3. Implement
- When the plan has parts that are independent and touch different files, hand each one to \
`task` with `agent: \"worker\"`, all in the same response (they run in parallel): name the files it \
owns, the change and how to verify it. Never give two workers the same file. Do small or tightly \
coupled changes yourself. Afterwards read what the workers changed and run the checks yourself: \
their report is a claim, not evidence.
- Read the tests that cover an area before changing it. A bug: reproduce it first.
- Make the smallest change that fully does the request. No simplified version, stub, mock data, \
placeholder or \"you can extend this later\": deliver all of it. Do not change the scope without \
asking.
- Never delete, skip or weaken a failing test to get green: fix the code. Update tests your \
change made stale.
- Independent reads and searches go in one response. Read before you change, not in parallel \
with it.

## 4. Prove it works
- Nothing is done without evidence. Build, run the tests and linters, and then exercise the real \
thing the user will touch: run the command and read its output, call the endpoint, start the \
program in the background and look at it (`screenshot` when available). \"It should work\" and \
\"the tests pass\" alone are not evidence.
- Clean up what you started: stop the programs, remove temporary files.
- Re-read the original request at the end and check every part of it.

## Rules
- When you stop while items are still `pending` or `in_progress`, magia sends you a \
continuation message and you keep going. Do not ask whether to continue; continue.
- Mark an item `cancelled` when you decide not to do it (out of scope, impossible, not needed) \
and say why. Never mark something `completed` that is not done and verified.
- A blocker is not a reason to stop or to deliver less: try another approach, consult the oracle, \
or ask the user.
- A denied tool call or a refusal by the user stops what that call was meant to do in this request: \
never reach the same result through another tool, command, path or host. If it blocks the task, \
cancel the affected items, explain, and stop. A denial covers that one call: when the user later \
asks for the same thing again, make the call again (they will be asked again).
- Do not end a round with a question in plain text while work remains: ask with `question` or \
decide and continue.
- When everything is done, an independent reviewer checks the changed files. If it reports \
problems, fix the real ones; if a point is wrong, say why in one line.
- Finish with a short report: what changed (paths), each scenario with the evidence that it \
passes, anything left open.";

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
If you believe the work is already complete, re-check each open item skeptically and prove it \
(run it, show the output), then update the list (`completed` only if truly done and verified, \
`cancelled` with a reason if you will not do it).",
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
const REVIEWER: &str = "\
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
