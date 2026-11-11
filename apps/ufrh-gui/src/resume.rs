//! `/resume` picker: lists this folder's saved sessions (newest first), filters as you type, and
//! with "All folders" searches every session. Only sessions of the current folder can be
//! resumed; the others are shown (with their folder) but cannot be opened here.
//!
//! The picker never touches the disk: it asks the caller to run searches ([`Action::Search`],
//! done off the UI thread) and receives the results through [`Picker::set_results`].

use std::path::{Path, PathBuf};
use ufrh_store::Hit;

pub struct Picker {
    pub query: String,
    pub all: bool,
    cwd: PathBuf,
    /// Session file currently open in the app (marked, and not reopened).
    current: Option<PathBuf>,
    hits: Vec<Hit>,
    sel: usize,
    /// Generation of the last search requested; older results are dropped.
    generation: u64,
    loading: bool,
    /// (query, all) of the last search requested.
    searched: Option<(String, bool)>,
    note: String,
    opened_frame: bool,
    scroll_to_sel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Search {
        generation: u64,
        query: String,
        all: bool,
    },
    Open(PathBuf),
    Close,
}

impl Picker {
    pub fn new(cwd: &Path, query: &str, current: Option<PathBuf>) -> Self {
        Self {
            query: query.trim().to_owned(),
            all: false,
            cwd: cwd.to_path_buf(),
            current,
            hits: vec![],
            sel: 0,
            generation: 0,
            loading: false,
            searched: None,
            note: String::new(),
            opened_frame: true,
            scroll_to_sel: false,
        }
    }

    pub fn set_results(&mut self, generation: u64, hits: Vec<Hit>) {
        if generation == self.generation {
            self.hits = hits;
            self.sel = 0;
            self.loading = false;
        }
    }

    #[cfg(test)]
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    fn resumable(&self, h: &Hit) -> bool {
        h.session.resumable_in(&self.cwd)
    }

    /// Enter / click on row `i`.
    fn pick(&mut self, i: usize, actions: &mut Vec<Action>) {
        let Some(h) = self.hits.get(i) else { return };
        self.sel = i;
        if self.current.as_deref() == Some(h.session.path.as_path()) {
            self.note = "that is the session already open".into();
        } else if self.resumable(h) {
            actions.push(Action::Open(h.session.path.clone()));
        } else {
            let owner = h.session.meta.cwd.as_deref().unwrap_or(Path::new("?"));
            self.note = format!(
                "this session belongs to {} — open ufrh in that folder to resume it",
                owner.display()
            );
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Vec<Action> {
        let mut actions = Vec::new();
        let want = (self.query.trim().to_owned(), self.all);
        if self.searched.as_ref() != Some(&want) {
            self.generation += 1;
            self.loading = true;
            self.searched = Some(want.clone());
            actions.push(Action::Search {
                generation: self.generation,
                query: want.0,
                all: want.1,
            });
        }

        // Navigation keys are taken before any widget sees them (the composer included).
        let (mut up, mut down, mut enter, mut esc) = (0usize, 0usize, false, false);
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                let egui::Event::Key {
                    key, pressed: true, ..
                } = e
                else {
                    return true;
                };
                match key {
                    egui::Key::ArrowUp => up += 1,
                    egui::Key::ArrowDown => down += 1,
                    egui::Key::Enter => enter = true,
                    egui::Key::Escape => esc = true,
                    _ => return true,
                }
                false
            })
        });
        if esc {
            actions.push(Action::Close);
            return actions;
        }
        let n = self.hits.len();
        if n > 0 && (up > 0 || down > 0) {
            self.sel = (self.sel + n * 8 + down - up) % n;
            self.scroll_to_sel = true;
            self.note.clear();
        }
        if enter {
            self.pick(self.sel, &mut actions);
        }

        let mut open = true;
        let mut clicked = None;
        egui::Window::new("Resume a session")
            .id(egui::Id::new("resume_picker"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 48.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .id(egui::Id::new("resume_query"))
                            .hint_text("Search sessions…")
                            .desired_width(ui.available_width() - 110.0),
                    );
                    if self.opened_frame {
                        r.request_focus();
                        self.opened_frame = false;
                    }
                    if r.changed() {
                        self.note.clear();
                    }
                    ui.checkbox(&mut self.all, "All folders")
                        .on_hover_text("Search sessions of every folder (only this folder's can be resumed)");
                });
                ui.weak(if self.all {
                    "every folder".to_owned()
                } else {
                    format!("sessions of {}", self.cwd.display())
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.hits.is_empty() {
                            ui.weak(if self.loading {
                                "searching…"
                            } else if self.query.trim().is_empty() && !self.all {
                                "no saved sessions in this folder yet (tick All folders to see others)"
                            } else {
                                "no session matches"
                            });
                        }
                        for (i, h) in self.hits.iter().enumerate() {
                            let r = ui.selectable_label(i == self.sel, self.row(ui, h));
                            if i == self.sel && self.scroll_to_sel {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
                self.scroll_to_sel = false;
                ui.separator();
                if !self.note.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(220, 150, 60), &self.note);
                }
                ui.weak("↑↓ select · Enter resume · Esc close");
            });
        if let Some(i) = clicked {
            self.pick(i, &mut actions);
        }
        if !open {
            actions.push(Action::Close);
        }
        actions
    }

    fn row(&self, ui: &egui::Ui, h: &Hit) -> egui::text::LayoutJob {
        let s = &h.session;
        let ok = self.resumable(h);
        let v = ui.visuals();
        let (strong, weak) = if ok {
            (v.strong_text_color(), v.weak_text_color())
        } else {
            (v.weak_text_color(), v.weak_text_color())
        };
        let font = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::TextStyle::Small.resolve(ui.style());
        let fmt = |font_id: &egui::FontId, color| egui::TextFormat {
            font_id: font_id.clone(),
            color,
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        let title = if s.meta.title.is_empty() {
            "(no prompt)"
        } else {
            &s.meta.title
        };
        job.append(title, 0.0, fmt(&font, strong));
        let mut info = format!(
            "\n{} · {} messages",
            ufrh_store::age(s.meta.updated),
            s.meta.messages
        );
        if self.current.as_deref() == Some(s.path.as_path()) {
            info.push_str(" · open now");
        }
        match &s.meta.cwd {
            _ if ok && s.meta.cwd.is_some() => {}
            Some(c) => info.push_str(&format!(" · {} (other folder)", c.display())),
            None => info.push_str(" · folder unknown (older session; resuming moves it here)"),
        }
        job.append(&info, 0.0, fmt(&small, weak));
        for snip in &h.snippets {
            job.append(&format!("\n  {snip}"), 0.0, fmt(&small, weak));
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use ufrh_store::{SessionInfo, SessionMeta};

    fn hit(path: &str, cwd: Option<&str>, title: &str) -> Hit {
        Hit {
            session: SessionInfo {
                path: PathBuf::from(path),
                meta: SessionMeta {
                    cwd: cwd.map(PathBuf::from),
                    created: 0,
                    updated: 0,
                    title: title.into(),
                    messages: 3,
                },
            },
            snippets: vec![],
        }
    }

    struct State {
        picker: Picker,
        actions: Vec<Action>,
    }

    fn harness() -> Harness<'static, State> {
        // `/` canonicalizes to itself, so it stands for "this folder".
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| {
                let a = s.picker.show(ui.ctx());
                s.actions.extend(a);
            },
            State {
                picker: Picker::new(Path::new("/"), "", Some(PathBuf::from("/s/now.jsonl"))),
                actions: vec![],
            },
        );
        h.run();
        h
    }

    fn results(h: &mut Harness<'_, State>) {
        let generation = h.state().picker.generation;
        h.state_mut().picker.set_results(
            generation,
            vec![
                hit("/s/a.jsonl", Some("/"), "fix the parser"),
                hit("/s/b.jsonl", Some("/elsewhere"), "other project"),
                hit("/s/now.jsonl", Some("/"), "current one"),
                hit("/s/old.jsonl", None, "legacy"),
            ],
        );
        h.run();
    }

    fn press(h: &mut Harness<'_, State>, key: Key) {
        h.key_press(key);
        h.run();
    }

    #[test]
    fn opens_with_a_search_and_lists_results() {
        let mut h = harness();
        assert_eq!(
            h.state().actions,
            [Action::Search {
                generation: 1,
                query: String::new(),
                all: false
            }]
        );
        results(&mut h);
        assert_eq!(h.state().picker.hits().len(), 4);
        assert!(h.query_by_label_contains("fix the parser").is_some());
        assert!(h.query_by_label_contains("(other folder)").is_some());
        // Typing searches again; stale results are ignored.
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("pars");
        h.run();
        assert!(matches!(
            h.state().actions.last(),
            Some(Action::Search { generation: 2, query, .. }) if query == "pars"
        ));
        h.state_mut().picker.set_results(1, vec![]);
        assert_eq!(h.state().picker.hits().len(), 4, "old generation dropped");
    }

    #[test]
    fn enter_resumes_only_sessions_of_this_folder() {
        let mut h = harness();
        results(&mut h);
        press(&mut h, Key::ArrowDown); // other folder
        press(&mut h, Key::Enter);
        assert!(
            !h.state()
                .actions
                .iter()
                .any(|a| matches!(a, Action::Open(_)))
        );
        assert!(h.query_by_label_contains("belongs to /elsewhere").is_some());
        press(&mut h, Key::ArrowDown); // the one already open
        press(&mut h, Key::Enter);
        assert!(
            !h.state()
                .actions
                .iter()
                .any(|a| matches!(a, Action::Open(_)))
        );
        press(&mut h, Key::ArrowDown); // legacy: allowed, it gets adopted
        press(&mut h, Key::Enter);
        assert_eq!(
            h.state().actions.last(),
            Some(&Action::Open("/s/old.jsonl".into()))
        );
        press(&mut h, Key::ArrowDown); // wraps to the first
        press(&mut h, Key::Enter);
        assert_eq!(
            h.state().actions.last(),
            Some(&Action::Open("/s/a.jsonl".into()))
        );
        h.get_by_label_contains("fix the parser").click();
        h.run();
        assert_eq!(
            h.state().actions.last(),
            Some(&Action::Open("/s/a.jsonl".into()))
        );
    }

    #[test]
    fn escape_closes_and_all_folders_searches_again() {
        let mut h = harness();
        results(&mut h);
        h.get_by_label("All folders").click();
        h.run();
        assert!(matches!(
            h.state().actions.last(),
            Some(Action::Search { all: true, .. })
        ));
        press(&mut h, Key::Escape);
        assert_eq!(h.state().actions.last(), Some(&Action::Close));
    }
}
