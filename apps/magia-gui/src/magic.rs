//! The Magia agent's mark: a gradient border that spins around the agent picker for a few
//! seconds when Magia is picked or starts a request, easing out and fading away. Otherwise the
//! picker looks like any other, and nothing repaints continuously.

use egui::{Color32, Pos2, Rect, Stroke};
use std::f32::consts::TAU;

/// How long the border spins.
pub const SPIN_SECONDS: f32 = 5.0;
/// Whole turns in one spin.
const TURNS: f32 = 3.0;
/// The border fades out over the last part of the spin.
const FADE_SECONDS: f32 = 1.2;

#[derive(Default)]
pub struct Spin {
    /// `ctx` time when the current spin started.
    started: Option<f64>,
    /// Asked to spin; the clock starts at the next painted frame, so a slow first frame (the
    /// app opening) does not eat the animation.
    pending: bool,
}

impl Spin {
    pub fn start(&mut self, ctx: &egui::Context) {
        self.pending = true;
        ctx.request_repaint();
    }

    /// Rotation now (radians) and opacity (0 when not spinning: nothing to draw).
    fn phase(&mut self, ctx: &egui::Context) -> (f32, f32) {
        if std::mem::take(&mut self.pending) {
            self.started = Some(ctx.input(|i| i.time));
        }
        let Some(t0) = self.started else {
            return (0.0, 0.0);
        };
        let elapsed = (ctx.input(|i| i.time) - t0) as f32;
        if elapsed >= SPIN_SECONDS {
            self.started = None;
            return (0.0, 0.0);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
        let p = elapsed / SPIN_SECONDS;
        let eased = 1.0 - (1.0 - p).powi(3); // fast start, slows down
        let alpha = ((SPIN_SECONDS - elapsed) / FADE_SECONDS).min(1.0);
        (TURNS * TAU * eased, alpha)
    }

    /// Paint the border over `rect` (drawn after the widget, so it sits on top).
    pub fn paint(&mut self, ui: &egui::Ui, rect: Rect, colors: &[Color32; 4]) {
        let (phase, alpha) = self.phase(ui.ctx());
        if alpha <= 0.0 {
            return; // not spinning: the picker keeps its normal look
        }
        let radius = ui.visuals().widgets.inactive.corner_radius.nw as f32;
        let points = rounded_rect_path(rect.shrink(0.5), radius);
        let center = rect.center();
        let color_at = |p: Pos2| {
            // Angle around the centre, with the box squashed to a square so the colours
            // sweep evenly instead of bunching on the short sides.
            let a = ((p.y - center.y) / rect.height()).atan2((p.x - center.x) / rect.width());
            gradient(colors, (a + phase).rem_euclid(TAU) / TAU)
        };
        let painter = ui.painter();
        for w in points.windows(2) {
            let c = color_at(w[0].lerp(w[1], 0.5));
            painter.line_segment(
                [w[0], w[1]],
                Stroke::new(5.0, c.gamma_multiply(0.3 * alpha)),
            );
            painter.line_segment([w[0], w[1]], Stroke::new(1.6, c.gamma_multiply(alpha)));
        }
    }
}

/// Colour at `x` (0..1, wrapping) on a loop through `colors`.
fn gradient(colors: &[Color32; 4], x: f32) -> Color32 {
    let n = colors.len() as f32;
    let f = x * n;
    let i = f.floor() as usize % colors.len();
    let j = (i + 1) % colors.len();
    let t = f - f.floor();
    let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    let (a, b) = (colors[i], colors[j]);
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Closed outline of a rounded rectangle, in short steps (so the gradient is smooth).
fn rounded_rect_path(r: Rect, radius: f32) -> Vec<Pos2> {
    let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
    // Corner centres, clockwise from top-right, with the angle each arc starts at.
    let corners = [
        (Pos2::new(r.right() - rad, r.top() + rad), -TAU / 4.0),
        (Pos2::new(r.right() - rad, r.bottom() - rad), 0.0),
        (Pos2::new(r.left() + rad, r.bottom() - rad), TAU / 4.0),
        (Pos2::new(r.left() + rad, r.top() + rad), TAU / 2.0),
    ];
    let mut outline = Vec::new();
    for (c, a0) in corners {
        for k in 0..=6 {
            let a = a0 + TAU / 4.0 * k as f32 / 6.0;
            outline.push(c + rad * egui::vec2(a.cos(), a.sin()));
        }
    }
    outline.push(outline[0]);
    let mut out = vec![outline[0]];
    for w in outline.windows(2) {
        let steps = ((w[1] - w[0]).length() / 4.0).ceil().max(1.0) as usize;
        for s in 1..=steps {
            out.push(w[0].lerp(w[1], s as f32 / steps as f32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_loops_through_the_colours() {
        let c = [Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE];
        assert_eq!(gradient(&c, 0.0), Color32::RED);
        assert_eq!(gradient(&c, 0.25), Color32::GREEN);
        assert_eq!(gradient(&c, 0.999), gradient(&c, 0.999)); // no panic at the wrap
        assert_eq!(gradient(&c, 1.0), Color32::RED);
    }

    #[test]
    fn outline_is_closed_and_hugs_the_box() {
        let r = Rect::from_min_size(Pos2::new(10.0, 10.0), egui::vec2(120.0, 26.0));
        let p = rounded_rect_path(r, 6.0);
        assert!(p.first().unwrap().distance(*p.last().unwrap()) < 0.01);
        assert!(p.iter().all(|q| r.expand(0.01).contains(*q)));
        assert!(p.windows(2).all(|w| w[0].distance(w[1]) <= 4.01));
    }
}

#[cfg(test)]
mod spin_tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    #[derive(Default)]
    struct S {
        spin: Spin,
        agent: String,
        phases: Vec<f32>,
    }

    #[test]
    fn picking_magia_again_spins() {
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                let mut picked = None;
                let r = egui::ComboBox::from_id_salt("agent")
                    .selected_text(s.agent.clone())
                    .show_ui(ui, |ui| {
                        for n in ["build", "magia"] {
                            if ui.selectable_label(s.agent == n, n).clicked() {
                                picked = Some(n.to_owned());
                            }
                        }
                    })
                    .response;
                if s.agent == "magia" {
                    let (p, _) = s.spin.phase(ui.ctx());
                    s.phases.push(p);
                    s.spin.paint(ui, r.rect, &[Color32::RED; 4]);
                }
                // Same rule as the app: picking Magia spins, even when it is already selected.
                if let Some(n) = picked {
                    if n == "magia" {
                        s.spin.start(ui.ctx());
                    }
                    s.agent = n;
                }
            },
            S {
                agent: "magia".into(),
                ..Default::default()
            },
        );
        h.run();
        assert!(
            h.state().phases.iter().all(|x| *x == 0.0),
            "still until asked"
        );
        h.state_mut().phases.clear();
        h.get_by_role(egui::accesskit::Role::ComboBox).click();
        h.run();
        h.get_by_label("magia").click();
        for _ in 0..30 {
            h.step();
        }
        let p = &h.state().phases;
        assert!(p.iter().any(|x| *x > 0.0), "spins: {p:?}");
        assert_eq!(p.last(), Some(&0.0), "and stops: {p:?}");
    }
}
