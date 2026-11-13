//! The left bar: projects with their conversations, and standalone chats, like the Claude and
//! Codex apps. Built from the session sidecars (`magia_store::list`), which the app loads off
//! the UI thread; this module only groups and draws them.

use crate::theme;
use magia_store::SessionInfo;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Conversations listed per project before "Show more".
const PER_PROJECT: usize = 5;
/// Standalone chats listed before "Show more".
const CHATS: usize = 10;

pub enum Action {
    /// A standalone chat, in a scratch folder of its own.
    NewChat,
    /// A new conversation in this project.
    NewIn(PathBuf),
    /// Open this session (switching to its folder).
    Open { path: PathBuf },
    /// Pick a folder to work in.
    AddProject,
    /// Search every conversation (`/resume` over all folders).
    Search,
}

#[derive(Default)]
pub struct Sidebar {
    /// Every saved session, most recent first (`None` until the first load arrives).
    sessions: Option<Vec<SessionInfo>>,
    /// Projects folded or unfolded by hand (the open project starts unfolded, others folded).
    toggled: HashSet<PathBuf>,
    /// Projects (and `chats_root` for the chat list) showing every conversation.
    expanded: HashSet<PathBuf>,
}

/// A project folder and its conversations, most recent first.
pub struct Project<'a> {
    pub dir: PathBuf,
    pub sessions: Vec<&'a SessionInfo>,
}

/// Split sessions into projects (by folder, the most recently used first) and standalone chats
/// (sessions whose folder is inside `chats_root`). `current` is listed even with no sessions.
pub fn group<'a>(
    sessions: &'a [SessionInfo],
    chats_root: &Path,
    current: &Path,
) -> (Vec<Project<'a>>, Vec<&'a SessionInfo>) {
    let mut projects: Vec<Project<'a>> = Vec::new();
    let mut chats = Vec::new();
    for s in sessions {
        let Some(dir) = &s.meta.cwd else { continue }; // saved before folders existed
        if dir.starts_with(chats_root) {
            chats.push(s);
        } else if let Some(p) = projects.iter_mut().find(|p| p.dir == *dir) {
            p.sessions.push(s);
        } else {
            projects.push(Project {
                dir: dir.clone(),
                sessions: vec![s],
            });
        }
    }
    let current = magia_store::canonical(current);
    if !current.starts_with(chats_root) && !projects.iter().any(|p| p.dir == current) {
        projects.insert(
            0,
            Project {
                dir: current,
                sessions: vec![],
            },
        );
    }
    (projects, chats)
}

/// Last path component, or the whole path.
pub fn name_of(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

fn title_of(s: &SessionInfo) -> &str {
    if s.meta.title.trim().is_empty() {
        "(untitled)"
    } else {
        s.meta.title.as_str()
    }
}

impl Sidebar {
    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>) {
        self.sessions = Some(sessions);
    }

    /// Draw the bar. `current` = the open session file.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        cwd: &Path,
        current: Option<&Path>,
        chats_root: &Path,
    ) -> Option<Action> {
        let t = *theme::tokens(ui);
        let mut act = None;
        let (new, search) = row(ui, "✎  New chat", 0.0, false, t.fg, Some("⌕"));
        if search {
            act = Some(Action::Search);
        } else if new.on_hover_text("A chat outside any project").clicked() {
            act = Some(Action::NewChat);
        }
        ui.add_space(10.0);
        let Some(sessions) = &self.sessions else {
            ui.label(
                egui::RichText::new("loading…")
                    .color(t.muted)
                    .size(theme::SMALL),
            );
            return act;
        };
        let (projects, chats) = group(sessions, chats_root, cwd);
        let here = magia_store::canonical(cwd);
        egui::ScrollArea::vertical()
            .id_salt("sidebar")
            .auto_shrink(false)
            .show(ui, |ui| {
                // Leave room for the scroll bar on the right.
                ui.set_width(ui.available_width() - 6.0);
                let (head, add) = row(ui, "Projects", 0.0, false, t.muted, Some("＋"));
                if add {
                    act = Some(Action::AddProject);
                }
                head.on_hover_text("＋ opens a folder as a project");
                for p in &projects {
                    // The open project starts unfolded, the others folded; a click flips it.
                    let open = (p.dir == here) != self.toggled.contains(&p.dir);
                    let label = format!("{}  {}", if open { "▾" } else { "▸" }, name_of(&p.dir));
                    let (r, plus) = row(ui, &label, 0.0, false, t.fg, Some("＋"));
                    if plus {
                        act = Some(Action::NewIn(p.dir.clone()));
                    } else if r
                        .on_hover_text(format!(
                            "{}\n＋ starts a new conversation here",
                            p.dir.display()
                        ))
                        .clicked()
                    {
                        toggle(&mut self.toggled, &p.dir);
                    }
                    if !open {
                        continue;
                    }
                    if p.sessions.is_empty() {
                        ui.label(
                            egui::RichText::new("      no conversations yet")
                                .color(t.muted)
                                .size(theme::SMALL),
                        );
                    }
                    let all = self.expanded.contains(&p.dir);
                    let shown = if all { p.sessions.len() } else { PER_PROJECT };
                    for s in p.sessions.iter().take(shown) {
                        if let Some(a) = session_row(ui, s, current) {
                            act = Some(a);
                        }
                    }
                    if p.sessions.len() > PER_PROJECT {
                        let more = if all {
                            "Show less".to_owned()
                        } else {
                            format!("Show {} more", p.sessions.len() - PER_PROJECT)
                        };
                        if row(ui, &more, INDENT, false, t.muted, None).0.clicked() {
                            toggle(&mut self.expanded, &p.dir);
                        }
                    }
                    ui.add_space(4.0);
                }
                ui.add_space(12.0);
                row(ui, "Chats", 0.0, false, t.muted, None);
                if chats.is_empty() {
                    ui.label(
                        egui::RichText::new("  chats outside a project show here")
                            .color(t.muted)
                            .size(theme::SMALL),
                    );
                }
                let all = self.expanded.contains(chats_root);
                for s in chats.iter().take(if all { chats.len() } else { CHATS }) {
                    if let Some(a) = session_row(ui, s, current) {
                        act = Some(a);
                    }
                }
                if chats.len() > CHATS {
                    let more = if all {
                        "Show less".to_owned()
                    } else {
                        format!("Show {} more", chats.len() - CHATS)
                    };
                    if row(ui, &more, INDENT, false, t.muted, None).0.clicked() {
                        toggle(&mut self.expanded, chats_root);
                    }
                }
            });
        act
    }
}

/// Indent of a conversation under its project.
const INDENT: f32 = 18.0;

fn session_row(ui: &mut egui::Ui, s: &SessionInfo, current: Option<&Path>) -> Option<Action> {
    let t = *theme::tokens(ui);
    let selected = current == Some(s.path.as_path());
    let (r, _) = row(ui, title_of(s), INDENT, selected, t.fg, None);
    let clicked = r
        .on_hover_text(format!(
            "{} · {}",
            s.meta.title,
            magia_store::age(s.meta.updated)
        ))
        .clicked();
    (clicked && !selected).then(|| Action::Open {
        path: s.path.clone(),
    })
}

fn toggle(set: &mut HashSet<PathBuf>, p: &Path) {
    if !set.remove(p) {
        set.insert(p.to_path_buf());
    }
}

/// One full-width line: text from the left (cut with "…"), a hover/selected background, and
/// an optional icon at the right that shows on hover. Returns the row and whether the icon
/// was the part clicked.
fn row(
    ui: &mut egui::Ui,
    text: &str,
    indent: f32,
    selected: bool,
    color: egui::Color32,
    icon: Option<&str>,
) -> (egui::Response, bool) {
    let t = *theme::tokens(ui);
    let h = 26.0;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    let mut icon_clicked = false;
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        let hovered = resp.hovered();
        if selected || hovered {
            painter.rect_filled(rect, 6.0, if selected { t.surface } else { t.raised });
        }
        let icon_rect = icon.map(|_| {
            egui::Rect::from_min_max(
                egui::pos2(rect.right() - 26.0, rect.top()),
                rect.right_bottom(),
            )
        });
        let right = icon_rect.map_or(rect.right() - 8.0, |r| r.left());
        let mut job = egui::text::LayoutJob::simple_singleline(
            text.to_owned(),
            egui::FontId::proportional(theme::SMALL + 1.5),
            color,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(
            (right - rect.left() - 8.0 - indent).max(10.0),
        );
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let y = rect.center().y - galley.size().y / 2.0;
        painter.galley(egui::pos2(rect.left() + 8.0 + indent, y), galley, color);
        if let (Some(icon), Some(ir)) = (icon, icon_rect) {
            let over = resp.hover_pos().is_some_and(|p| ir.contains(p));
            if hovered {
                painter.text(
                    ir.center(),
                    egui::Align2::CENTER_CENTER,
                    icon,
                    egui::FontId::proportional(15.0),
                    if over { t.fg } else { t.muted },
                );
            }
            icon_clicked =
                resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| ir.contains(p));
        }
    }
    (
        resp.on_hover_cursor(egui::CursorIcon::PointingHand),
        icon_clicked,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use magia_store::SessionMeta;

    fn s(path: &str, cwd: Option<&str>, title: &str) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(path),
            meta: SessionMeta {
                cwd: cwd.map(PathBuf::from),
                title: title.into(),
                messages: 1,
                ..Default::default()
            },
        }
    }

    #[test]
    fn sessions_group_into_projects_and_chats() {
        let chats = Path::new("/data/magia/chats");
        let list = vec![
            s("1", Some("/src/a"), "a newest"),
            s("2", Some("/data/magia/chats/9"), "a chat"),
            s("3", Some("/src/b"), "b"),
            s("4", Some("/src/a"), "a older"),
            s("5", None, "no folder"),
        ];
        let (projects, chat_list) = group(&list, chats, Path::new("/nonexistent/here"));
        let names: Vec<_> = projects
            .iter()
            .map(|p| p.dir.display().to_string())
            .collect();
        // The open folder comes first even without conversations; then by most recent use.
        assert_eq!(names, ["/nonexistent/here", "/src/a", "/src/b"]);
        let titles: Vec<_> = projects[1]
            .sessions
            .iter()
            .map(|s| s.meta.title.as_str())
            .collect();
        assert_eq!(titles, ["a newest", "a older"]);
        assert_eq!(chat_list.len(), 1);
        assert_eq!(chat_list[0].meta.title, "a chat");
        // A chat's scratch folder is never listed as a project.
        let (projects, _) = group(&list, chats, Path::new("/data/magia/chats/9"));
        assert_eq!(projects.len(), 2);
    }
}
