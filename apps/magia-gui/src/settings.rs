//! Settings window (⚙ in the top bar, `/settings`): the theme and the Magia mode (rounds,
//! review, the model of each subagent role), so none of it needs the JSON config. Saved in the
//! state file, which wins over the config file; the config file itself is never rewritten.
//!
//! Pure UI over a draft: it never touches the disk or the network. Saving and fetching another
//! provider's model list are returned as actions for the app to do.

use crate::model_picker::ModelPicker;
use crate::theme;
use magia_core::Config;
use magia_core::magia_mode::Role;
use std::collections::HashMap;

pub const THEMES: [(&str, &str); 3] = [("system", "System"), ("light", "Light"), ("dark", "Dark")];

/// What the window edits.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub theme: String,
    pub max_rounds: u32,
    pub review: bool,
    /// Compact at this share of the model's context window, in %.
    pub compact_percent: u8,
    /// Per role (in `Role::ALL` order): provider ("" = the chat's) and model ("" = the chat's).
    pub roles: [(String, String); 5],
}

impl Draft {
    pub fn of(cfg: &Config) -> Self {
        let mut roles: [(String, String); 5] = Default::default();
        for (role, spec) in &cfg.role_models {
            if let Some(i) = Role::ALL.iter().position(|r| r == role) {
                roles[i] = split_spec(cfg, spec);
            }
        }
        Self {
            theme: cfg.theme.clone(),
            max_rounds: cfg.magia_mode.max_rounds,
            review: cfg.magia_mode.review,
            compact_percent: cfg.compact_percent,
            roles,
        }
    }

    /// The roles that name a model, as config specs (`provider/model` or a bare id). A bare id
    /// that starts like `provider/` (`anthropic/claude-…` on OpenRouter) would be read as that
    /// provider, so it is saved with the chat's provider named.
    pub fn role_models(&self, cfg: &Config) -> Vec<(Role, String)> {
        Role::ALL
            .iter()
            .zip(&self.roles)
            .filter(|(_, (_, m))| !m.trim().is_empty())
            .map(|(r, (p, m))| {
                let m = m.trim();
                let misread = m.split_once('/').is_some_and(|(v, _)| cfg.is_provider(v));
                let spec = match (p.is_empty(), misread) {
                    (true, false) => m.to_owned(),
                    (true, true) => format!("{}/{m}", cfg.provider),
                    (false, _) => format!("{p}/{m}"),
                };
                (*r, spec)
            })
            .collect()
    }
}

/// `provider/model` → (provider, model); anything else → ("", id). Same rule as
/// `Config::role_model`.
fn split_spec(cfg: &Config, spec: &str) -> (String, String) {
    match spec.split_once('/') {
        Some((p, m)) if cfg.is_provider(p) => (p.to_owned(), m.to_owned()),
        _ => (String::new(), spec.to_owned()),
    }
}

pub enum Action {
    Save(Box<Draft>),
    /// The model list of this provider is needed (a role picked it).
    Models(String),
}

#[derive(Default)]
pub struct Settings {
    pub open: bool,
    draft: Option<Draft>,
    pickers: [ModelPicker; 5],
    error: Option<String>,
}

fn role_help(r: Role) -> &'static str {
    match r {
        Role::Explore => "Fast searches through the code; several run at once",
        Role::Plan => "Turns the request into an ordered plan with checks",
        Role::Oracle => "Senior consultant for hard problems",
        Role::Worker => "Implements one part of the work; can edit files",
        Role::Review => "Checks the finished work before Magia stops",
    }
}

impl Settings {
    /// Open on the current values.
    pub fn open(&mut self, cfg: &Config) {
        self.open = true;
        self.draft = Some(Draft::of(cfg));
        self.error = None;
    }

    /// Draws the window when open. `models` holds the lists fetched so far, by provider.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        cfg: &Config,
        providers: &[String],
        models: &HashMap<String, Vec<String>>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        let Some(mut draft) = self.draft.take() else {
            self.open = false;
            return actions;
        };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            let t = *theme::tokens(ui);
            ui.set_width(520.0);
            ui.label(
                egui::RichText::new("Settings")
                    .font(theme::semibold(theme::HEADING))
                    .color(t.fg),
            );
            ui.add_space(10.0);

            section(ui, &t, "Appearance");
            ui.horizontal(|ui| {
                ui.label("Theme");
                for (key, label) in THEMES {
                    ui.radio_value(&mut draft.theme, key.to_owned(), label);
                }
            });
            ui.add_space(14.0);

            section(ui, &t, "Context");
            ui.horizontal(|ui| {
                ui.label("Compact at");
                ui.add(
                    egui::DragValue::new(&mut draft.compact_percent)
                        .range(50..=99)
                        .suffix("%"),
                );
                ui.label("of the model's context window");
            })
            .response
            .on_hover_text(
                "The older messages are summarized when the conversation reaches this share of \
                 the window. Higher keeps more history but leaves less room for the answer. \
                 Unknown window: 100k tokens. A compact_at_tokens in the JSON config wins.",
            );
            ui.add_space(14.0);

            section(ui, &t, "Magia mode");
            ui.horizontal(|ui| {
                ui.label("Rounds per request");
                ui.add(egui::DragValue::new(&mut draft.max_rounds).range(1..=500));
            })
            .response
            .on_hover_text("Magia stops after this many rounds even with tasks left");
            ui.checkbox(
                &mut draft.review,
                "Have the changes reviewed before finishing",
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Model of each subagent (empty: the chat's model)")
                    .size(theme::SMALL)
                    .color(t.muted),
            );
            ui.add_space(4.0);
            egui::Grid::new("roles")
                .num_columns(4)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    for (i, role) in Role::ALL.iter().enumerate() {
                        let (provider, model) = &mut draft.roles[i];
                        ui.label(role.name()).on_hover_text(role_help(*role));
                        let before = provider.clone();
                        egui::ComboBox::from_id_salt(("role-provider", i))
                            .selected_text(if provider.is_empty() {
                                "chat's provider"
                            } else {
                                provider.as_str()
                            })
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(provider, String::new(), "chat's provider");
                                for p in providers {
                                    ui.selectable_value(provider, p.clone(), p.as_str());
                                }
                            });
                        if *provider != before {
                            model.clear(); // another provider has other ids
                        }
                        let list_of = if provider.is_empty() {
                            cfg.provider.as_str()
                        } else {
                            provider.as_str()
                        };
                        if !models.contains_key(list_of) {
                            actions.push(Action::Models(list_of.to_owned()));
                        }
                        let list = models.get(list_of).map(Vec::as_slice).unwrap_or(&[]);
                        if let Some(m) =
                            self.pickers[i].show_with(ui, model, list, 230.0, "chat's model")
                        {
                            *model = m;
                        }
                        let clear = ui.add_enabled(
                            !model.is_empty() || !provider.is_empty(),
                            egui::Button::new("×").frame(false),
                        );
                        if clear.on_hover_text("Use the chat's model").clicked() {
                            provider.clear();
                            model.clear();
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Saved in magia's state file; these win over the JSON config, which is \
                     not changed.",
                )
                .size(theme::SMALL)
                .color(t.muted),
            );
            if let Some(e) = &self.error {
                ui.add_space(6.0);
                ui.colored_label(t.danger, e);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::primary(ui, "Save", true).clicked() {
                    let missing = Role::ALL
                        .iter()
                        .zip(&draft.roles)
                        .find(|(_, (p, m))| !p.is_empty() && m.trim().is_empty());
                    let bad = draft
                        .role_models(cfg)
                        .into_iter()
                        .find_map(|(r, s)| cfg.role_model(&s).err().map(|e| (r, e)));
                    if let Some((r, _)) = missing {
                        self.error = Some(format!("{}: pick a model for the provider", r.name()));
                    } else if let Some((r, e)) = bad {
                        self.error = Some(format!("{}: {e:#}", r.name()));
                    } else {
                        actions.push(Action::Save(Box::new(draft.clone())));
                        close = true;
                    }
                }
                if theme::secondary(ui, "Cancel", None).clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        // One list request per provider is enough.
        actions.dedup_by(|a, b| matches!((a, b), (Action::Models(x), Action::Models(y)) if x == y));
        if close {
            self.open = false;
        } else {
            self.draft = Some(draft);
        }
        actions
    }
}

fn section(ui: &mut egui::Ui, t: &theme::Tokens, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .font(theme::semibold(theme::BODY))
            .color(t.accent),
    );
    ui.add_space(4.0);
}

/// Theme key → egui preference.
pub fn preference(theme: &str) -> egui::ThemePreference {
    match theme {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_through_the_draft() {
        let cfg = Config {
            provider: "openrouter".into(),
            role_models: vec![
                (Role::Explore, "zen/fast".into()),
                (Role::Oracle, "big".into()),
                (Role::Worker, "moonshotai/kimi-k2".into()),
            ],
            ..Default::default()
        };
        let mut d = Draft::of(&cfg);
        assert_eq!(d.roles[0], ("zen".into(), "fast".into()));
        assert_eq!(d.roles[2], (String::new(), "big".into()));
        assert_eq!(d.roles[3], (String::new(), "moonshotai/kimi-k2".into()));
        // A provider without a model is not a role model.
        d.roles[1] = ("zen".into(), String::new());
        assert_eq!(d.role_models(&cfg), cfg.role_models);
        // The chat's model `zen/…` (an OpenRouter id) must not turn into the zen provider.
        d.roles[4] = (String::new(), "zen/x".into());
        assert_eq!(
            d.role_models(&cfg)[3],
            (Role::Review, "openrouter/zen/x".into())
        );
    }

    #[test]
    fn save_hands_back_what_was_changed() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct S {
            settings: Settings,
            cfg: Config,
            saved: Vec<Draft>,
            fonts: bool,
        }
        let mut s = S {
            settings: Settings::default(),
            cfg: Config::default(),
            saved: vec![],
            fonts: false,
        };
        s.settings.open(&s.cfg);
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                let ctx = &ui.ctx().clone();
                // The app's fonts (semibold) take effect on the next frame.
                if !s.fonts {
                    ctx.set_fonts(fastframe_fonts::FontSetup::default().definitions());
                    s.fonts = true;
                    return;
                }
                let models = HashMap::from([("zen".to_owned(), vec![])]);
                for a in s.settings.ui(ctx, &s.cfg, &["zen".into()], &models) {
                    if let Action::Save(d) = a {
                        s.saved.push(*d);
                    }
                }
            },
            s,
        );
        h.run();
        h.get_by_label("Dark").click();
        h.run();
        h.get_by_label("Have the changes reviewed before finishing")
            .click();
        h.run();
        h.get_by_label("Save").click();
        h.run();
        let saved = &h.state().saved;
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].theme.as_str(), saved[0].review), ("dark", false));
        assert!(!h.state().settings.open, "saving closes the window");
    }
}
