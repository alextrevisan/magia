//! What the agent is doing besides talking: Magia mode's round and review state, and the
//! subagents (`task` calls and the reviewer) with their live tool use. Built from
//! `AgentEvent`s, shown in the side panel above the task list.

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

pub struct Sub {
    id: String,
    pub label: String,
    /// Tool calls the subagent made.
    pub steps: u32,
    /// The latest one, summarized.
    pub last: String,
    pub state: SubState,
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
                        self.subs.push(Sub {
                            id: REVIEW_PARENT.into(),
                            label: format!("reviewer · review {n}"),
                            steps: 0,
                            last: String::new(),
                            state: SubState::Running,
                        });
                    }
                    MagiaPhase::ReviewFailed { issues } => {
                        m.issues = issues.clone();
                        let note = format!("{} problem(s) found", issues.len());
                        if let Some(s) = self.running(REVIEW_PARENT) {
                            s.state = SubState::Failed(note);
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
                self.subs.push(Sub {
                    id: id.clone(),
                    label: clip(label, 80),
                    steps: 0,
                    last: String::new(),
                    state: SubState::Running,
                });
            }
            AgentEvent::ToolEnd { id, is_error, .. } => {
                if let Some(s) = self.running(id) {
                    s.state = if *is_error {
                        SubState::Failed("failed".into())
                    } else {
                        SubState::Done("report sent back".into())
                    };
                }
            }
            AgentEvent::SubTool {
                parent,
                name,
                input,
            } => {
                if let Some(s) = self.running(parent) {
                    s.steps += 1;
                    s.last = summary(name, input);
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

/// Magia and subagent sections (nothing when idle).
pub fn show(ui: &mut egui::Ui, a: &Activity) {
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
        for s in &a.subs {
            let (mark, color, note) = match &s.state {
                SubState::Running => ("▸", t.accent, None),
                SubState::Done(n) => ("✓", t.ok, Some(n.as_str())),
                SubState::Failed(n) => ("×", t.danger, Some(n.as_str())),
            };
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new(mark).color(color));
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&s.label)
                                .size(theme::SMALL + 0.5)
                                .color(t.fg),
                        )
                        .wrap(),
                    );
                    let mut detail = format!("{} step(s)", s.steps);
                    if let Some(n) = note {
                        detail.push_str(&format!(" · {n}"));
                    } else if !s.last.is_empty() {
                        detail.push_str(&format!(" · {}", s.last));
                    }
                    ui.add(egui::Label::new(small(&detail, t.muted)).truncate());
                });
            });
        }
        ui.add_space(10.0);
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
                    name: tool.into(),
                    input: json!({}),
                },
            );
        }
        assert_eq!((a.subs[0].steps, a.subs[0].last.as_str()), (2, "read"));
        feed(
            &mut a,
            AgentEvent::ToolEnd {
                id: "c1".into(),
                output: "report".into(),
                is_error: false,
            },
        );
        assert!(matches!(a.subs[0].state, SubState::Done(_)));

        feed(&mut a, AgentEvent::Magia(MagiaPhase::Reviewing { n: 1 }));
        feed(
            &mut a,
            AgentEvent::SubTool {
                parent: REVIEW_PARENT.into(),
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
