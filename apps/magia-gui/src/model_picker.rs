//! Model picker for the top bar: one button that opens a searchable list.
//!
//! Typing filters the provider's models (prefix > substring > letters in order); ↑↓ move the
//! highlight, Enter picks it. When nothing matches, Enter uses the typed text as the model id,
//! so models missing from `GET /models` can still be chosen. Esc or a click outside closes.

use crate::theme;
use egui::{Key, Modifiers};
use magia_core::filter_files;

/// Wider than the button so long OpenRouter ids fit.
const LIST_W: f32 = 320.0;
const LIST_H: f32 = 340.0;

#[derive(Default)]
pub struct ModelPicker {
    query: String,
    highlight: usize,
    /// The popup was showing last frame (to reset and focus the search on open).
    open: bool,
}

impl ModelPicker {
    /// Shows the button (and the list when open). Returns the model picked this frame.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        current: &str,
        models: &[String],
        width: f32,
    ) -> Option<String> {
        self.show_with(ui, current, models, width, "pick a model")
    }

    /// Like `show`, with `empty` on the button when no model is set.
    pub fn show_with(
        &mut self,
        ui: &mut egui::Ui,
        current: &str,
        models: &[String],
        width: f32,
        empty: &str,
    ) -> Option<String> {
        let label = if current.trim().is_empty() {
            egui::RichText::new(empty)
                .font(theme::mono(theme::MONO))
                .weak()
        } else {
            egui::RichText::new(current).font(theme::mono(theme::MONO))
        };
        let button = egui::Button::new(label)
            .right_text("▾")
            .truncate()
            .min_size(egui::vec2(width, ui.spacing().interact_size.y));
        let config = egui::containers::menu::MenuConfig::new()
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        let mut picked = None;
        let (response, inner) = egui::containers::menu::MenuButton::from_button(button)
            .config(config)
            .ui(ui, |ui| picked = self.list(ui, current, models));
        response.on_hover_text(if current.is_empty() { "Model" } else { current });
        if inner.is_none() {
            self.open = false;
        }
        picked
    }

    fn list(&mut self, ui: &mut egui::Ui, current: &str, models: &[String]) -> Option<String> {
        let just_opened = !self.open;
        if just_opened {
            self.open = true;
            self.query.clear();
        }
        ui.set_width(LIST_W);
        let shown = matches(models, &self.query);
        if just_opened {
            self.highlight = shown.iter().position(|m| *m == current).unwrap_or(0);
        }
        // Keys are taken before the search field sees them (↑↓ would move its caret).
        let (up, down, enter) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        let before = self.query.clone();
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.query)
                .font(theme::mono(theme::MONO))
                .hint_text("search or type a model id")
                .desired_width(f32::INFINITY),
        );
        if just_opened || !search.has_focus() {
            search.request_focus();
        }
        let shown = if self.query != before {
            self.highlight = 0;
            matches(models, &self.query)
        } else {
            shown
        };
        if !shown.is_empty() {
            if up {
                self.highlight = self.highlight.saturating_sub(1);
            }
            if down {
                self.highlight = (self.highlight + 1).min(shown.len() - 1);
            }
            self.highlight = self.highlight.min(shown.len() - 1);
        }
        let mut picked = None;
        if enter {
            picked = match shown.get(self.highlight) {
                Some(m) => Some((*m).clone()),
                None if !self.query.trim().is_empty() => Some(self.query.trim().to_owned()),
                None => None,
            };
        }
        ui.separator();
        if shown.is_empty() {
            ui.weak(if models.is_empty() {
                "No model list yet: check the provider key. Type an id and press Enter."
            } else {
                "No match: press Enter to use the typed id."
            });
        }
        egui::ScrollArea::vertical()
            .max_height(LIST_H)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, m) in shown.iter().enumerate() {
                    let text = egui::RichText::new(m.as_str()).font(theme::mono(theme::MONO));
                    let row = ui.add(
                        egui::Button::selectable(i == self.highlight || *m == current, text)
                            .frame_when_inactive(i == self.highlight),
                    );
                    if i == self.highlight && (up || down || just_opened) {
                        row.scroll_to_me(None);
                    }
                    if row.clicked() {
                        picked = Some((*m).clone());
                    }
                }
            });
        if picked.is_some() {
            ui.close();
        }
        picked
    }
}

/// The models to list for `query`: all of them in provider order when empty, else best first.
fn matches<'a>(models: &'a [String], query: &str) -> Vec<&'a String> {
    let q = query.trim();
    if q.is_empty() {
        models.iter().collect()
    } else {
        filter_files(models, q, usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    struct State {
        picker: ModelPicker,
        model: String,
        models: Vec<String>,
    }

    fn harness(models: &[&str]) -> Harness<'static, State> {
        let state = State {
            picker: ModelPicker::default(),
            model: "gpt-5".into(),
            models: models.iter().map(|s| s.to_string()).collect(),
        };
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| {
                if let Some(m) = s.picker.show(ui, &s.model, &s.models, 200.0) {
                    s.model = m;
                }
            },
            state,
        );
        h.run();
        h
    }

    fn open(h: &mut Harness<'_, State>) {
        h.get_by_label_contains("gpt-5").click();
        h.run();
    }

    #[test]
    fn typing_filters_and_enter_picks_the_best_match() {
        let mut h = harness(&[
            "gpt-5",
            "claude-sonnet-4-5",
            "anthropic/claude-opus-4",
            "qwen3-coder",
        ]);
        open(&mut h);
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("opus");
        h.run();
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(h.state().model, "anthropic/claude-opus-4");
    }

    #[test]
    fn arrows_move_the_highlight() {
        let mut h = harness(&["gpt-5", "claude-sonnet-4-5", "qwen3-coder"]);
        open(&mut h);
        // Opens on the current model (gpt-5, first); ↓ ↓ ↑ lands on the second.
        for k in [Key::ArrowDown, Key::ArrowDown, Key::ArrowUp] {
            h.key_press(k);
            h.run();
        }
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(h.state().model, "claude-sonnet-4-5");
    }

    #[test]
    fn enter_with_no_match_uses_the_typed_id() {
        let mut h = harness(&[]);
        open(&mut h);
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("my-local-model");
        h.run();
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(h.state().model, "my-local-model");
    }

    #[test]
    fn clicking_a_row_picks_it() {
        let mut h = harness(&["gpt-5", "qwen3-coder"]);
        open(&mut h);
        h.get_by_label("qwen3-coder").click();
        h.run();
        assert_eq!(h.state().model, "qwen3-coder");
    }

    #[test]
    fn empty_query_keeps_provider_order() {
        let models = ["z-model", "a-model", "m/deep"].map(String::from);
        let got: Vec<_> = matches(&models, " ").into_iter().cloned().collect();
        assert_eq!(got, models);
    }
}
