//! The message box: Enter sends, Shift+Enter is a newline, and typing `/` opens a list of
//! commands and skills that filters as you type (↑↓ to move, Tab/Enter to complete, Esc to close).
//! Images are attached with Ctrl+V or by dropping files on the window.

use std::path::PathBuf;
use ufrh_core::{SlashEntry, SlashKind, filter_slash, media};
use ufrh_types::Image;

const VISIBLE: usize = 8;

#[derive(Default)]
pub struct Composer {
    pub input: String,
    sel: usize,
    was_open: bool,
    last_query: Option<String>,
    /// Input value for which the user pressed Esc (the list stays closed until it changes).
    dismissed: Option<String>,
    /// Focus as of the previous frame: egui drops focus on Esc before widgets run.
    had_focus: bool,
    pub attachments: Vec<Attachment>,
    /// A `V` key press reached egui (so it was typed, not a paste shortcut).
    v_down: bool,
    /// egui delivered the clipboard as text during the current paste.
    pasted_text: bool,
    /// Messages sent before, oldest first; ↑/↓ walk through them like a shell.
    history: Vec<String>,
    /// Index into `history` while browsing it; `None` while editing the draft.
    hist_pos: Option<usize>,
    /// What was typed when ↑ started browsing; ↓ past the newest message brings it back.
    draft: String,
    /// Changes made to recalled messages while browsing (kept until the next send).
    edits: std::collections::HashMap<usize, String>,
}

/// Most messages kept for ↑/↓.
const HISTORY_MAX: usize = 200;

pub struct Attachment {
    pub image: Image,
    pub label: String,
    thumb: Option<egui::TextureHandle>,
}

/// Longest edge of the preview next to the box.
pub const THUMB: u32 = 56;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// The user asked to send the current input.
    pub send: bool,
    /// The `/` list just opened: the caller should refresh the catalog.
    pub opened: bool,
    /// Ctrl+V with something other than text on the clipboard: the caller should read the
    /// clipboard image (off the UI thread) and call [`Composer::attach`].
    pub paste_image: bool,
    /// Image files pasted as paths or dropped on the window, to load and attach.
    pub files: Vec<PathBuf>,
}

/// The command name being typed: `/rev` → `rev`. `None` once a space/newline follows (the
/// arguments are being typed) or when the input is not a command.
pub fn slash_query(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('/')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

impl Composer {
    pub fn id() -> egui::Id {
        egui::Id::new("composer")
    }

    /// Entries the list would show right now (empty = closed).
    pub fn suggestions(&self, catalog: &[SlashEntry]) -> Vec<SlashEntry> {
        match slash_query(&self.input) {
            Some(q) if self.dismissed.as_deref() != Some(self.input.as_str()) => {
                filter_slash(catalog, q).into_iter().cloned().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Add an image; `thumb` is its RGBA preview (computed off the UI thread).
    pub fn attach(
        &mut self,
        ctx: &egui::Context,
        image: Image,
        thumb: Option<(u32, u32, Vec<u8>)>,
    ) {
        let thumb = thumb.map(|(w, h, px)| {
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
            ctx.load_texture("attachment", img, egui::TextureOptions::LINEAR)
        });
        self.attachments.push(Attachment {
            label: image_label(&image),
            image,
            thumb,
        });
    }

    /// Record a sent message for ↑/↓ and stop browsing.
    pub fn remember(&mut self, text: &str) {
        let text = text.trim();
        self.hist_pos = None;
        self.draft.clear();
        self.edits.clear();
        if text.is_empty() || self.history.last().is_some_and(|l| l == text) {
            return;
        }
        self.history.push(text.to_owned());
        let extra = self.history.len().saturating_sub(HISTORY_MAX);
        self.history.drain(..extra);
    }

    /// Add older messages (e.g. from a resumed session) before the ones sent in this window.
    pub fn load_history(&mut self, older: Vec<String>) {
        let mut merged: Vec<String> = Vec::with_capacity(older.len() + self.history.len());
        for m in older.into_iter().chain(std::mem::take(&mut self.history)) {
            if !m.trim().is_empty() && merged.last() != Some(&m) {
                merged.push(m);
            }
        }
        let extra = merged.len().saturating_sub(HISTORY_MAX);
        merged.drain(..extra);
        self.history = merged;
        self.hist_pos = None;
        self.edits.clear();
    }

    /// History entry `p` as last seen: with the user's edits, if any.
    fn entry(&self, p: usize) -> &str {
        self.edits.get(&p).unwrap_or(&self.history[p])
    }

    /// ↑: the previous sent message (saving the draft the first time).
    fn history_up(&mut self, ctx: &egui::Context) {
        let pos = match self.hist_pos {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(p) => p - 1,
        };
        self.show_entry(ctx, Some(pos), false);
    }

    /// ↓: the next sent message, or back to the draft after the newest one.
    fn history_down(&mut self, ctx: &egui::Context) {
        let next = self
            .hist_pos
            .and_then(|p| (p + 1 < self.history.len()).then_some(p + 1));
        self.show_entry(ctx, next, true);
    }

    fn show_entry(&mut self, ctx: &egui::Context, pos: Option<usize>, caret_at_end: bool) {
        self.hist_pos = pos;
        self.input = match pos {
            Some(p) => self.entry(p).to_owned(),
            None => std::mem::take(&mut self.draft),
        };
        // A recalled `/command` should not pop the command list over the history walk.
        if pos.is_some() {
            self.dismissed = Some(self.input.clone());
        }
        // ↑ leaves the caret at the start, so another ↑ keeps going back; ↓ at the end.
        let at = if caret_at_end {
            self.input.chars().count()
        } else {
            0
        };
        set_cursor(ctx, Self::id(), at);
    }

    /// Images to send with the message; clears the attachments.
    pub fn take_images(&mut self) -> Vec<Image> {
        self.attachments.drain(..).map(|a| a.image).collect()
    }

    #[cfg(test)]
    pub fn selected(&self) -> usize {
        self.sel
    }

    pub fn show(&mut self, ui: &mut egui::Ui, catalog: &[SlashEntry]) -> Output {
        let ctx = ui.ctx().clone();
        let id = Self::id();
        let focused = ui.memory(|m| m.has_focus(id));
        let query = slash_query(&self.input).map(str::to_owned);
        let mut out = Output {
            opened: query.is_some() && !self.was_open,
            ..Default::default()
        };
        self.was_open = query.is_some();
        if query != self.last_query {
            self.sel = 0;
            self.last_query = query.clone();
        }
        let matches = self.suggestions(catalog);
        let popup = !matches.is_empty();
        self.sel = self.sel.min(matches.len().saturating_sub(1));

        // Keys are removed from the event queue before the TextEdit sees them. egui's
        // `consume_key` is not used for Enter because it ignores extra Shift, which would also
        // swallow Shift+Enter (the newline key below).
        let mut complete: Option<String> = None;
        let mut refocus = false;
        // Editing a recalled message keeps the edit at that position, so walking away and back
        // loses nothing; the draft stays untouched too.
        if let Some(p) = self.hist_pos
            && self.entry(p) != self.input
        {
            self.edits.insert(p, self.input.clone());
        }
        let (at_start, on_last_line) = caret_position(&ctx, id, &self.input);
        let can_go_up = !self.history.is_empty() && self.hist_pos != Some(0) && at_start;
        let can_go_down = self.hist_pos.is_some() && on_last_line;
        let (mut hist_up, mut hist_down) = (false, false);
        let nobody_focused = ui.memory(|m| m.focused().is_none());
        if focused || self.had_focus || nobody_focused {
            self.paste_events(&ctx, &mut out);
        }
        if focused || self.had_focus {
            let (mut up, mut down, mut tab, mut enter, mut esc) =
                (0usize, 0usize, false, false, false);
            ctx.input_mut(|i| {
                i.events.retain(|e| {
                    let egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } = e
                    else {
                        return true;
                    };
                    let plain = modifiers.is_none();
                    match key {
                        egui::Key::Escape if popup => esc = true,
                        _ if !focused => return true, // only Esc counts once focus is gone
                        egui::Key::ArrowUp if popup && plain => up += 1,
                        egui::Key::ArrowDown if popup && plain => down += 1,
                        egui::Key::Tab if popup && plain => tab = true,
                        egui::Key::Enter if plain => enter = true,
                        egui::Key::ArrowUp if plain && can_go_up => hist_up = true,
                        egui::Key::ArrowDown if plain && can_go_down => hist_down = true,
                        egui::Key::Backspace
                            if plain && self.input.is_empty() && !self.attachments.is_empty() =>
                        {
                            self.attachments.pop();
                        }
                        _ => return true,
                    }
                    false
                });
            });
            if popup {
                let n = matches.len();
                self.sel = (self.sel + n * VISIBLE + down - up) % n;
                let sel = &matches[self.sel];
                if esc {
                    self.dismissed = Some(self.input.clone());
                    refocus = true; // Esc closes the list, not the text box
                } else if tab || (enter && query.as_deref() != Some(sel.name.as_str())) {
                    complete = Some(sel.name.clone());
                } else if enter {
                    out.send = true; // name already complete: run it
                }
            } else if enter {
                out.send = true;
            }
        }
        if hist_up {
            self.history_up(&ctx);
        } else if hist_down {
            self.history_down(&ctx);
        }

        self.attachment_row(ui);
        let resp = ui.add_sized(
            [ui.available_width(), 64.0],
            egui::TextEdit::multiline(&mut self.input)
                .id(id)
                .return_key(egui::KeyboardShortcut::new(
                    egui::Modifiers::SHIFT,
                    egui::Key::Enter,
                ))
                .hint_text("Ask ufrh to read, change or explain code…")
                .margin(egui::Margin::symmetric(10, 8)),
        );

        if popup
            && complete.is_none()
            && !out.send
            && self.dismissed.as_deref() != Some(self.input.as_str())
        {
            complete = self.popup(&ctx, resp.rect, &matches);
        }
        if let Some(name) = complete {
            self.input = format!("/{name} ");
            set_cursor_to_end(&ctx, id, &self.input);
            refocus = true;
        }
        if refocus {
            resp.request_focus();
        }
        self.had_focus = resp.has_focus() || refocus;
        out
    }

    /// Ctrl+V and dropped files. egui-winit turns Ctrl+V into `Event::Paste(text)` and emits
    /// nothing at all when the clipboard holds no text (an image), but the *release* of `V`
    /// still arrives as a key event. A `V` release without a `V` press and without pasted text
    /// therefore means "Ctrl+V on a non-text clipboard".
    fn paste_events(&mut self, ctx: &egui::Context, out: &mut Output) {
        ctx.input_mut(|i| {
            for f in std::mem::take(&mut i.raw.dropped_files) {
                if media::is_image_path(f.path()) && f.path().is_file() {
                    out.files.push(f.path().to_path_buf());
                } else {
                    i.raw.dropped_files.push(f);
                }
            }
            i.events.retain(|e| match e {
                egui::Event::Paste(t) => {
                    if let Some(paths) = crate::clipboard::pasted_image_paths(t) {
                        out.files.extend(paths); // copied image files: attach, don't type
                        return false;
                    }
                    self.pasted_text = true;
                    true
                }
                egui::Event::Key {
                    key: egui::Key::V,
                    pressed,
                    ..
                } => {
                    if *pressed {
                        self.v_down = true;
                    } else {
                        out.paste_image |= !self.v_down && !self.pasted_text;
                        self.v_down = false;
                        self.pasted_text = false;
                    }
                    true
                }
                _ => true,
            });
        });
    }

    fn attachment_row(&mut self, ui: &mut egui::Ui) {
        if self.attachments.is_empty() {
            return;
        }
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (n, a) in self.attachments.iter().enumerate() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(t) = &a.thumb {
                            ui.image((t.id(), t.size_vec2()));
                        }
                        ui.vertical(|ui| {
                            ui.weak(&a.label);
                            if ui
                                .small_button("× remove")
                                .on_hover_text("Backspace in an empty box removes the last one")
                                .clicked()
                            {
                                remove = Some(n);
                            }
                        });
                    });
                });
            }
        });
        if let Some(n) = remove {
            self.attachments.remove(n);
        }
    }

    /// Draw the suggestion list above the box. Returns the clicked entry, if any.
    fn popup(
        &self,
        ctx: &egui::Context,
        anchor: egui::Rect,
        matches: &[SlashEntry],
    ) -> Option<String> {
        let start = self.sel.saturating_sub(VISIBLE - 1);
        let mut picked = None;
        egui::Area::new(egui::Id::new("slash_popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor.left_top())
            .pivot(egui::Align2::LEFT_BOTTOM)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width((anchor.width() - 16.0).max(200.0));
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let (strong, weak, normal) = (
                        ui.visuals().strong_text_color(),
                        ui.visuals().weak_text_color(),
                        ui.visuals().text_color(),
                    );
                    for (i, e) in matches.iter().enumerate().skip(start).take(VISIBLE) {
                        let fmt = |color| egui::TextFormat {
                            font_id: font.clone(),
                            color,
                            ..Default::default()
                        };
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("/{}", e.name), 0.0, fmt(strong));
                        if !e.args.is_empty() {
                            job.append(&format!(" {}", e.args), 0.0, fmt(weak));
                        }
                        job.append(&format!("   {}", e.description), 0.0, fmt(normal));
                        let tag = match e.kind {
                            SlashKind::Builtin => "",
                            SlashKind::Command => "   command",
                            SlashKind::Skill => "   skill",
                        };
                        job.append(tag, 0.0, fmt(weak));
                        if ui.selectable_label(i == self.sel, job).clicked() {
                            picked = Some(e.name.clone());
                        }
                    }
                    let more = if matches.len() > VISIBLE {
                        format!("{} of {} · ", self.sel + 1, matches.len())
                    } else {
                        String::new()
                    };
                    ui.weak(format!("{more}↑↓ move · Tab/Enter complete · Esc close"));
                });
            });
        picked
    }
}

/// `PNG 1280×720 · 240 KB`
pub fn image_label(img: &Image) -> String {
    let kind = img.media_type.trim_start_matches("image/").to_uppercase();
    let kb = img.byte_len().div_ceil(1024);
    match media::dimensions(img) {
        Some((w, h)) => format!("{kind} {w}×{h} · {kb} KB"),
        None => format!("{kind} · {kb} KB"),
    }
}

/// Put the caret after the last character (after an autocomplete).
fn set_cursor_to_end(ctx: &egui::Context, id: egui::Id, text: &str) {
    set_cursor(ctx, id, text.chars().count());
}

fn set_cursor(ctx: &egui::Context, id: egui::Id, char_index: usize) {
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let at = egui::text::CCursor::new(char_index);
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(at)));
    state.store(ctx, id);
}

/// (caret at the very start, caret on the last line), with no selection. Before the box has
/// ever been focused there is no caret: an empty box counts as both.
fn caret_position(ctx: &egui::Context, id: egui::Id, text: &str) -> (bool, bool) {
    let range = egui::text_edit::TextEditState::load(ctx, id).and_then(|s| s.cursor.char_range());
    match range {
        Some(r) if r.primary.index == r.secondary.index => {
            let i = r.primary.index.0;
            (i == 0, !text.chars().skip(i).any(|c| c == '\n'))
        }
        Some(_) => (false, false), // a selection: arrows only move it
        None => (text.is_empty(), !text.contains('\n')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn entry(name: &str, desc: &str, kind: SlashKind) -> SlashEntry {
        SlashEntry {
            name: name.into(),
            description: desc.into(),
            kind,
            args: String::new(),
        }
    }

    fn catalog() -> Vec<SlashEntry> {
        vec![
            entry("clear", "Start a new session", SlashKind::Builtin),
            entry("compact", "Summarize older history", SlashKind::Builtin),
            entry("review", "Review the diff", SlashKind::Command),
            entry("deploy", "Ship to prod", SlashKind::Skill),
        ]
    }

    struct State {
        composer: Composer,
        catalog: Vec<SlashEntry>,
        sent: Vec<String>,
        opened: usize,
        paste_image: usize,
        files: Vec<PathBuf>,
    }

    fn harness() -> Harness<'static, State> {
        let state = State {
            composer: Composer::default(),
            catalog: catalog(),
            sent: vec![],
            opened: 0,
            paste_image: 0,
            files: vec![],
        };
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| {
                ui.add_space(300.0); // room for the list above the box
                let out = s.composer.show(ui, &s.catalog);
                s.opened += out.opened as usize;
                s.paste_image += out.paste_image as usize;
                s.files.extend(out.files);
                if out.send {
                    let n = s.composer.take_images().len();
                    let text = std::mem::take(&mut s.composer.input);
                    s.composer.remember(&text);
                    s.sent.push(if n > 0 {
                        format!("{text}+{n}img")
                    } else {
                        text
                    });
                }
            },
            state,
        );
        h.run();
        h.get_by_role(egui::accesskit::Role::MultilineTextInput)
            .focus();
        h.run();
        h
    }

    fn type_text(h: &mut Harness<'_, State>, t: &str) {
        h.get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text(t);
        h.run();
    }

    fn press(h: &mut Harness<'_, State>, key: Key, modifiers: Modifiers) {
        h.key_press_modifiers(modifiers, key);
        h.run();
    }

    #[test]
    fn enter_sends_and_shift_enter_inserts_newline() {
        let mut h = harness();
        type_text(&mut h, "hello");
        press(&mut h, Key::Enter, Modifiers::SHIFT);
        type_text(&mut h, "world");
        assert_eq!(h.state().composer.input, "hello\nworld");
        assert!(h.state().sent.is_empty());
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().sent, ["hello\nworld"]);
        assert_eq!(
            h.state().composer.input,
            "",
            "no stray newline after sending"
        );
    }

    #[test]
    fn slash_opens_full_list_and_typing_filters() {
        let mut h = harness();
        type_text(&mut h, "/");
        assert_eq!(h.state().opened, 1, "catalog refresh requested once");
        assert_eq!(h.state().composer.suggestions(&catalog()).len(), 4);
        assert!(
            h.query_by_label_contains("/deploy").is_some(),
            "list is drawn"
        );
        type_text(&mut h, "c");
        let names: Vec<String> = h
            .state()
            .composer
            .suggestions(&catalog())
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["clear", "compact"]);
        assert!(
            h.query_by_label_contains("/deploy").is_none(),
            "filtered out"
        );
        type_text(&mut h, "o");
        let names: Vec<String> = h
            .state()
            .composer
            .suggestions(&catalog())
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["compact"]);
    }

    #[test]
    fn arrows_select_and_tab_completes_then_enter_runs() {
        let mut h = harness();
        type_text(&mut h, "/");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(h.state().composer.selected(), 2);
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(h.state().composer.selected(), 1);
        press(&mut h, Key::Tab, Modifiers::NONE);
        assert_eq!(
            h.state().composer.input,
            "/compact ",
            "completed with a trailing space"
        );
        assert!(
            h.state().composer.suggestions(&catalog()).is_empty(),
            "list closes after completion"
        );
        type_text(&mut h, "now");
        assert_eq!(
            h.state().composer.input,
            "/compact now",
            "caret moved to the end"
        );
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().sent, ["/compact now"]);
    }

    #[test]
    fn enter_completes_partial_name_but_runs_exact_one() {
        let mut h = harness();
        type_text(&mut h, "/dep");
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().composer.input, "/deploy ");
        assert!(h.state().sent.is_empty(), "first Enter only completes");
        let mut h = harness();
        type_text(&mut h, "/clear");
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().sent, ["/clear"], "exact name runs immediately");
    }

    #[test]
    fn escape_closes_until_input_changes_and_click_completes() {
        let mut h = harness();
        type_text(&mut h, "/re");
        press(&mut h, Key::Escape, Modifiers::NONE);
        assert!(h.state().composer.suggestions(&catalog()).is_empty());
        assert!(h.query_by_label_contains("/review").is_none());
        type_text(&mut h, "v");
        assert!(
            !h.state().composer.suggestions(&catalog()).is_empty(),
            "reopens on typing"
        );
        h.get_by_label_contains("/review").click();
        h.run();
        assert_eq!(h.state().composer.input, "/review ");
    }

    fn tiny_image() -> Image {
        media::image_from_rgba(3, 2, vec![200; 3 * 2 * 4]).unwrap()
    }

    #[test]
    fn ctrl_v_on_a_non_text_clipboard_asks_for_the_image() {
        let mut h = harness();
        // egui-winit swallows the Ctrl+V press and, with no text on the clipboard, sends
        // nothing; only the release of V arrives.
        h.key_up_modifiers(Modifiers::CTRL, Key::V);
        h.run();
        assert_eq!(h.state().paste_image, 1);

        // A text paste is a normal paste.
        h.event(egui::Event::Paste("some text".into()));
        h.run();
        h.key_up_modifiers(Modifiers::CTRL, Key::V);
        h.run();
        assert_eq!(h.state().paste_image, 1);
        assert_eq!(h.state().composer.input, "some text");

        // Typing the letter v (press reaches egui) is not a paste.
        h.key_down(Key::V);
        h.run();
        h.key_up(Key::V);
        h.run();
        assert_eq!(h.state().paste_image, 1);
    }

    #[test]
    fn pasted_image_paths_are_attached_not_typed() {
        let d = std::env::temp_dir().join(format!("ufrh-composer-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let img = d.join("shot.png");
        std::fs::write(&img, b"x").unwrap();
        let mut h = harness();
        h.event(egui::Event::Paste(format!("file://{}", img.display())));
        h.run();
        assert_eq!(h.state().files, [img]);
        assert_eq!(h.state().composer.input, "");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn attachments_show_send_and_backspace_removes() {
        let mut h = harness();
        let ctx = h.ctx.clone();
        let img = tiny_image();
        let thumb = media::thumbnail(&img, THUMB);
        h.state_mut().composer.attach(&ctx, img.clone(), thumb);
        h.state_mut().composer.attach(&ctx, img, None);
        h.run();
        assert_eq!(
            h.query_all_by_label_contains("PNG 3×2").count(),
            2,
            "labels drawn"
        );
        press(&mut h, Key::Backspace, Modifiers::NONE);
        assert_eq!(
            h.state().composer.attachments.len(),
            1,
            "Backspace on empty box"
        );
        // An image alone can be sent.
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().sent, ["+1img"]);
        assert!(h.state().composer.attachments.is_empty());
        // Backspace with text only edits the text.
        h.state_mut().composer.attach(&ctx, tiny_image(), None);
        type_text(&mut h, "ab");
        press(&mut h, Key::Backspace, Modifiers::NONE);
        assert_eq!(h.state().composer.input, "a");
        assert_eq!(h.state().composer.attachments.len(), 1);
    }

    fn input(h: &Harness<'_, State>) -> String {
        h.state().composer.input.clone()
    }

    fn send(h: &mut Harness<'_, State>, text: &str) {
        type_text(h, text);
        press(h, Key::Enter, Modifiers::NONE);
    }

    #[test]
    fn up_recalls_sent_messages_and_down_returns_to_the_draft() {
        let mut h = harness();
        send(&mut h, "first message");
        send(&mut h, "second message");
        // The example: type, go to the start, ↑ shows the previous message, ↓ restores the text.
        type_text(&mut h, "oi, pode implementar isso");
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(
            input(&h),
            "oi, pode implementar isso",
            "caret not at the start yet"
        );
        press(&mut h, Key::Home, Modifiers::NONE);
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "second message");
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "first message");
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "first message", "stops at the oldest");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "second message");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "oi, pode implementar isso", "draft is back");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(
            input(&h),
            "oi, pode implementar isso",
            "nothing after the draft"
        );
        // The caret is at the end of the restored draft: typing continues it.
        type_text(&mut h, "!");
        assert_eq!(input(&h), "oi, pode implementar isso!");
    }

    #[test]
    fn empty_box_goes_straight_to_history_and_edits_are_kept() {
        let mut h = harness();
        send(&mut h, "/compact");
        send(&mut h, "fix the parser");
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "fix the parser");
        type_text(&mut h, "please "); // caret is at the start after ↑
        assert_eq!(input(&h), "please fix the parser");
        press(&mut h, Key::Home, Modifiers::NONE);
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "/compact");
        assert!(
            h.state().composer.suggestions(&catalog()).is_empty(),
            "a recalled command does not open the list"
        );
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "please fix the parser", "the edit survived");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "", "empty draft");
        // Sending the edited copy adds it; the original stays in history.
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        press(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(h.state().sent.last().unwrap(), "please fix the parser");
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "fix the parser");
    }

    #[test]
    fn arrows_move_inside_multiline_text_before_walking_history() {
        let mut h = harness();
        send(&mut h, "older");
        type_text(&mut h, "line one");
        press(&mut h, Key::Enter, Modifiers::SHIFT);
        type_text(&mut h, "line two");
        // ↓ on the last line while not browsing: nothing to go to.
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "line one\nline two");
        // ↑ from the second line only moves the caret up; at the start it recalls.
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "line one\nline two");
        press(&mut h, Key::Home, Modifiers::NONE);
        press(&mut h, Key::ArrowUp, Modifiers::NONE);
        assert_eq!(input(&h), "older");
        press(&mut h, Key::ArrowDown, Modifiers::NONE);
        assert_eq!(input(&h), "line one\nline two");
    }

    #[test]
    fn older_sessions_are_loaded_before_this_windows_messages() {
        let mut h = harness();
        send(&mut h, "new");
        h.state_mut()
            .composer
            .load_history(vec!["old 1".into(), "old 2".into(), "new".into()]);
        for want in ["new", "old 2", "old 1"] {
            press(&mut h, Key::Home, Modifiers::NONE);
            press(&mut h, Key::ArrowUp, Modifiers::NONE);
            assert_eq!(input(&h), want);
        }
    }
}
