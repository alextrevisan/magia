//! What the agent is doing besides talking: Magia mode's round and review state, and the
//! subagents (`task` calls and the reviewer) with their live tool use. Built from
//! `AgentEvent`s, shown in the side panel above the task list. A subagent opens to every tool
//! call it made (input and output) and the report it sent back; rounds are links into the
//! transcript. Kept in memory for this window only, with outputs clipped.

use crate::theme;
use magia_core::{AgentEvent, MagiaPhase, REVIEW_PARENT};

#[derive(Default)]
pub struct Activity {
    /// Set while (and after) a Magia request runs.
    pub magia: Option<Magia>,
    pub subs: Vec<Sub>,
}

pub struct Magia {
    pub round: u32,
    pub phase: MagiaPhase,
    /// Problems from the last failed review, shown while the next round fixes them.
    pub issues: Vec<String>,
}

/// Outputs longer than this are cut (the transcript does the same).
const OUTPUT_MAX: usize = 4000;
/// Steps kept per subagent; older ones are dropped (the count stays right).
const STEPS_MAX: usize = 300;

pub struct Sub {
    id: String,
    pub label: String,
    /// Tool calls the subagent made.
    pub steps: u32,
    /// The latest one, summarized.
    pub last: String,
    pub state: SubState,
    /// The calls themselves, oldest first (at most `STEPS_MAX`).
    pub log: Vec<Step>,
    /// What it sent back to the main agent.
    pub report: Option<String>,
}

pub struct Step {
    id: String,
    pub name: String,
    pub summary: String,
    pub input: String,
    /// `(output, is_error)` once it finished.
    pub output: Option<(String, bool)>,
}

/// A place in the transcript the user asked to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    /// Where round `n` of the current Magia request began.
    Round(u32),
    /// Where this task began.
    Task(String),
}

fn new_sub(id: &str, label: String) -> Sub {
    Sub {
        id: id.into(),
        label,
        steps: 0,
        last: String::new(),
        state: SubState::Running,
        log: Vec::new(),
        report: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubState {
    Running,
    Done(String),
    Failed(String),
}

impl Activity {
    pub fn is_empty(&self) -> bool {
        self.magia.is_none() && self.subs.is_empty()
    }

    /// A new request starts with a clean slate.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn running(&mut self, id: &str) -> Option<&mut Sub> {
        self.subs
            .iter_mut()
            .rev()
            .find(|s| s.id == id && s.state == SubState::Running)
    }

    /// Follow one event. `summary` renders a tool call as one line (as in the transcript).
    pub fn on_event(
        &mut self,
        ev: &AgentEvent,
        summary: impl Fn(&str, &serde_json::Value) -> String,
    ) {
        match ev {
            AgentEvent::Magia(p) => {
                let m = self.magia.get_or_insert(Magia {
                    round: 1,
                    phase: p.clone(),
                    issues: Vec::new(),
                });
                match p {
                    MagiaPhase::Working { round } => m.round = *round,
                    MagiaPhase::Reviewing { n } => {
                        m.issues.clear();
                        self.subs
                            .push(new_sub(REVIEW_PARENT, format!("reviewer · review {n}")));
                    }
                    MagiaPhase::ReviewFailed { issues } => {
                        m.issues = issues.clone();
                        let note = format!("{} problem(s) found", issues.len());
                        if let Some(s) = self.running(REVIEW_PARENT) {
                            s.state = SubState::Failed(note);
                            s.report = Some(
                                issues
                                    .iter()
                                    .map(|i| format!("- {i}"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        }
                    }
                    MagiaPhase::Finished(why) => {
                        let note = if why.contains("review passed") {
                            "passed"
                        } else {
                            "done"
                        };
                        if let Some(s) = self.running(REVIEW_PARENT) {
                            s.state = SubState::Done(note.into());
                        }
                    }
                }
                if let Some(m) = &mut self.magia {
                    m.phase = p.clone();
                }
            }
            AgentEvent::ToolStart { id, name, input } if name == "task" => {
                let get = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::trim);
                let label = get("description")
                    .filter(|d| !d.is_empty())
                    .or(get("prompt"))
                    .unwrap_or("subagent");
                self.subs.push(new_sub(id, clip(label, 80)));
            }
            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
            } => {
                if let Some(s) = self.running(id) {
                    s.report = Some(magia_types::clip(output, OUTPUT_MAX * 2));
                    s.state = if *is_error {
                        SubState::Failed("failed".into())
                    } else {
                        SubState::Done("report sent back".into())
                    };
                }
            }
            AgentEvent::SubTool {
                parent,
                id,
                name,
                input,
            } => {
                if let Some(s) = self.running(parent) {
                    s.steps += 1;
                    s.last = summary(name, input);
                    if s.log.len() == STEPS_MAX {
                        s.log.remove(0);
                    }
                    s.log.push(Step {
                        id: id.clone(),
                        name: name.clone(),
                        summary: s.last.clone(),
                        input: serde_json::to_string_pretty(input).unwrap_or_default(),
                        output: None,
                    });
                }
            }
            AgentEvent::SubToolEnd {
                parent,
                id,
                output,
                is_error,
            } => {
                if let Some(step) = self
                    .running(parent)
                    .and_then(|s| s.log.iter_mut().rev().find(|st| st.id == *id))
                {
                    step.output = Some((magia_types::clip(output, OUTPUT_MAX), *is_error));
                }
            }
            AgentEvent::TurnDone => {
                for s in &mut self.subs {
                    if s.state == SubState::Running {
                        s.state = SubState::Failed("stopped".into());
                    }
                }
            }
            _ => {}
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= n && line.len() == s.len() {
        return line.to_owned();
    }
    let head: String = line.chars().take(n).collect();
    format!("{head}…")
}

fn heading(ui: &mut egui::Ui, text: &str) {
    let t = *theme::tokens(ui);
    ui.label(
        egui::RichText::new(text)
            .font(theme::semibold(theme::SMALL + 1.0))
            .color(t.fg),
    );
}

/// A small text link; true when clicked.
fn link(ui: &mut egui::Ui, text: &str) -> bool {
    let t = *theme::tokens(ui);
    let r = ui.add(
        egui::Label::new(egui::RichText::new(text).size(theme::SMALL).color(t.accent))
            .sense(egui::Sense::click()),
    );
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    if r.hovered() {
        ui.painter().hline(
            r.rect.x_range(),
            r.rect.bottom(),
            egui::Stroke::new(1.0, t.accent),
        );
    }
    r.clicked()
}

/// Magia and subagent sections (nothing when idle). Returns where the user asked to go.
pub fn show(ui: &mut egui::Ui, a: &Activity) -> Option<Jump> {
    let mut jump = None;
    let t = *theme::tokens(ui);
    let small = |s: &str, c| egui::RichText::new(s).size(theme::SMALL).color(c);
    if let Some(m) = &a.magia {
        heading(ui, &format!("Magia  round {}", m.round));
        let (mark, color, text) = match &m.phase {
            MagiaPhase::Working { .. } => ("▸", t.accent, "working through the tasks".to_owned()),
            MagiaPhase::Reviewing { n } => (
                "▸",
                t.accent,
                format!(
                    "reviewer is checking the changes ({n}/{})",
                    magia_core::magia_mode::MAX_REVIEWS
                ),
            ),
            MagiaPhase::ReviewFailed { issues } => (
                "!",
                t.danger,
                format!("review found {} problem(s); fixing", issues.len()),
            ),
            MagiaPhase::Finished(why) => ("✓", t.ok, why.clone()),
        };
        ui.horizontal_top(|ui| {
            ui.label(egui::RichText::new(mark).color(color));
            ui.add(egui::Label::new(small(&text, t.fg).size(theme::SMALL + 0.5)).wrap());
        });
        for i in &m.issues {
            ui.horizontal_top(|ui| {
                ui.add_space(14.0);
                ui.add(egui::Label::new(small(&format!("· {i}"), t.muted)).wrap());
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.add_space(14.0);
            ui.label(small("go to", t.muted));
            for n in 1..=m.round {
                if link(ui, &format!("round {n}")) {
                    jump = Some(Jump::Round(n));
                }
            }
        });
        ui.add_space(10.0);
    }
    if !a.subs.is_empty() {
        let running = a
            .subs
            .iter()
            .filter(|s| s.state == SubState::Running)
            .count();
        heading(
            ui,
            &if running > 0 {
                format!("Subagents  {running} running")
            } else {
                format!("Subagents  {}", a.subs.len())
            },
        );
        for (i, s) in a.subs.iter().enumerate() {
            let (mark, color, note) = match &s.state {
                SubState::Running => ("▸", t.accent, None),
                SubState::Done(n) => ("✓", t.ok, Some(n.as_str())),
                SubState::Failed(n) => ("×", t.danger, Some(n.as_str())),
            };
            // The whole row opens and closes the details (state kept by egui per row).
            let id = ui.make_persistent_id(("sub", i));
            let open = ui.data(|d| d.get_temp::<bool>(id).unwrap_or(false));
            let row = ui
                .horizontal_top(|ui| {
                    ui.label(egui::RichText::new(mark).color(color));
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&s.label)
                                    .size(theme::SMALL + 0.5)
                                    .color(t.fg),
                            )
                            .wrap()
                            .selectable(false),
                        );
                        let mut detail =
                            format!("{} {} step(s)", if open { "▾" } else { "▸" }, s.steps);
                        if let Some(n) = note {
                            detail.push_str(&format!(" · {n}"));
                        } else if !s.last.is_empty() {
                            detail.push_str(&format!(" · {}", s.last));
                        }
                        ui.add(
                            egui::Label::new(small(&detail, t.muted))
                                .truncate()
                                .selectable(false),
                        );
                    });
                })
                .response;
            let click = ui
                .interact(row.rect, id.with("row"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if click.clicked() {
                ui.data_mut(|d| d.insert_temp(id, !open));
            }
            if open {
                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 14,
                        ..Default::default()
                    })
                    .show(ui, |ui| sub_details(ui, i, s));
                ui.add_space(4.0);
            }
        }
        ui.add_space(10.0);
    }
    jump
}

/// Every call of one subagent, each opening to its input and output, then its report.
fn sub_details(ui: &mut egui::Ui, i: usize, s: &Sub) {
    let t = *theme::tokens(ui);
    if s.log.is_empty() && s.report.is_none() {
        ui.label(
            egui::RichText::new("no tool calls yet")
                .size(theme::SMALL)
                .color(t.muted),
        );
    }
    if s.steps as usize > s.log.len() {
        ui.label(
            egui::RichText::new(format!(
                "{} older step(s) not kept",
                s.steps as usize - s.log.len()
            ))
            .size(theme::SMALL)
            .color(t.muted),
        );
    }
    for (k, st) in s.log.iter().enumerate() {
        let (mark, color) = match &st.output {
            None => ("…", t.accent),
            Some((_, true)) => ("✗", t.danger),
            Some(_) => ("✓", t.ok),
        };
        let mut job = egui::text::LayoutJob::default();
        let fmt = |color| egui::TextFormat {
            font_id: theme::mono(theme::SMALL),
            color,
            ..Default::default()
        };
        job.append(mark, 0.0, fmt(color));
        job.append(&st.name, 6.0, fmt(t.fg));
        if !st.summary.is_empty() && st.summary != st.name {
            job.append(&st.summary, 8.0, fmt(t.muted));
        }
        job.wrap = egui::text::TextWrapping {
            max_width: ui.available_width() - 8.0,
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        egui::CollapsingHeader::new(job)
            .id_salt(("sub_step", i, k))
            .icon(|_, _, _| {})
            .show(ui, |ui| {
                crate::markdown::code_block(ui, "input", &st.input, ("sub_in", i, k));
                if let Some((o, err)) = &st.output {
                    crate::markdown::code_block(
                        ui,
                        if *err { "error" } else { "output" },
                        o,
                        ("sub_out", i, k),
                    );
                }
            });
    }
    if let Some(r) = &s.report {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Report")
                .font(theme::semibold(theme::SMALL))
                .color(t.muted),
        );
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, t.line))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                crate::markdown::show(ui, r, 1_000_000 + i);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feed(a: &mut Activity, ev: AgentEvent) {
        a.on_event(&ev, |n, _| n.to_owned());
    }

    #[test]
    fn follows_rounds_reviews_and_subagents() {
        let mut a = Activity::default();
        assert!(a.is_empty());
        feed(&mut a, AgentEvent::Magia(MagiaPhase::Working { round: 1 }));
        feed(
            &mut a,
            AgentEvent::ToolStart {
                id: "c1".into(),
                name: "task".into(),
                input: json!({"description": "find callers", "prompt": "…"}),
            },
        );
        for tool in ["grep", "read"] {
            feed(
                &mut a,
                AgentEvent::SubTool {
                    parent: "c1".into(),
                    id: format!("s-{tool}"),
                    name: tool.into(),
                    input: json!({}),
                },
            );
        }
        assert_eq!((a.subs[0].steps, a.subs[0].last.as_str()), (2, "read"));
        feed(
            &mut a,
            AgentEvent::SubToolEnd {
                parent: "c1".into(),
                id: "s-grep".into(),
                output: "x".repeat(OUTPUT_MAX + 50),
                is_error: false,
            },
        );
        let log = &a.subs[0].log;
        assert_eq!(log.len(), 2);
        assert!(matches!(&log[0].output, Some((o, false)) if o.len() < OUTPUT_MAX + 10));
        assert!(log[1].output.is_none());
        feed(
            &mut a,
            AgentEvent::ToolEnd {
                id: "c1".into(),
                output: "report".into(),
                is_error: false,
            },
        );
        assert!(matches!(a.subs[0].state, SubState::Done(_)));
        assert_eq!(a.subs[0].report.as_deref(), Some("report"));

        feed(&mut a, AgentEvent::Magia(MagiaPhase::Reviewing { n: 1 }));
        feed(
            &mut a,
            AgentEvent::SubTool {
                parent: REVIEW_PARENT.into(),
                id: "r1".into(),
                name: "read".into(),
                input: json!({}),
            },
        );
        feed(
            &mut a,
            AgentEvent::Magia(MagiaPhase::ReviewFailed {
                issues: vec!["a.rs: stub".into()],
            }),
        );
        assert_eq!(a.subs[1].steps, 1);
        assert!(matches!(a.subs[1].state, SubState::Failed(_)));
        feed(&mut a, AgentEvent::Magia(MagiaPhase::Working { round: 2 }));
        let m = a.magia.as_ref().unwrap();
        assert_eq!((m.round, m.issues.len()), (2, 1));

        feed(&mut a, AgentEvent::Magia(MagiaPhase::Reviewing { n: 2 }));
        feed(
            &mut a,
            AgentEvent::Magia(MagiaPhase::Finished("finished · review passed".into())),
        );
        assert_eq!(a.subs[2].state, SubState::Done("passed".into()));
        assert!(a.magia.as_ref().unwrap().issues.is_empty());
    }

    #[test]
    fn a_subagent_opens_to_its_steps_and_report_and_rounds_are_links() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        let mut a = Activity::default();
        feed(&mut a, AgentEvent::Magia(MagiaPhase::Working { round: 2 }));
        feed(
            &mut a,
            AgentEvent::ToolStart {
                id: "c1".into(),
                name: "task".into(),
                input: json!({"description": "find callers"}),
            },
        );
        a.on_event(
            &AgentEvent::SubTool {
                parent: "c1".into(),
                id: "s1".into(),
                name: "grep".into(),
                input: json!({"pattern": "fn main"}),
            },
            |_, _| "grep fn main".into(),
        );
        feed(
            &mut a,
            AgentEvent::SubToolEnd {
                parent: "c1".into(),
                id: "s1".into(),
                output: "src/main.rs:1".into(),
                is_error: false,
            },
        );
        feed(
            &mut a,
            AgentEvent::ToolEnd {
                id: "c1".into(),
                output: "FOUND-IT in src/main.rs".into(),
                is_error: false,
            },
        );
        struct S {
            a: Activity,
            jumps: Vec<Jump>,
            fonts: bool,
        }
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                // The app's fonts (semibold) take effect on the next frame.
                if !s.fonts {
                    ui.ctx()
                        .set_fonts(fastframe_fonts::FontSetup::default().definitions());
                    s.fonts = true;
                    return;
                }
                if let Some(j) = show(ui, &s.a) {
                    s.jumps.push(j);
                }
            },
            S {
                a,
                jumps: vec![],
                fonts: false,
            },
        );
        h.run();
        assert!(
            h.query_by_label_contains("FOUND-IT").is_none(),
            "closed at first"
        );
        h.get_by_label_contains("step(s)").click();
        h.run();
        assert!(
            h.query_by_label_contains("FOUND-IT").is_some(),
            "report shown"
        );
        h.get_by_label_contains("fn main").click();
        h.run();
        assert!(
            h.query_by_label_contains("src/main.rs:1").is_some(),
            "step output"
        );
        h.get_by_label("round 2").click();
        h.run();
        assert_eq!(h.state().jumps, [Jump::Round(2)]);
    }

    #[test]
    fn a_turn_that_ends_stops_running_subagents() {
        let mut a = Activity::default();
        feed(
            &mut a,
            AgentEvent::ToolStart {
                id: "c1".into(),
                name: "task".into(),
                input: json!({"prompt": "look around\nmore"}),
            },
        );
        assert_eq!(a.subs[0].label, "look around…");
        feed(&mut a, AgentEvent::TurnDone);
        assert!(matches!(a.subs[0].state, SubState::Failed(_)));
    }
}
