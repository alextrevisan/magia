//! Just enough Markdown for model replies: fenced code blocks, headings, bullet and numbered
//! lists, block quotes, and inline `code` / **bold**. Parsed per frame for visible items only
//! (the transcript is virtualized), so there is no cache and no extra memory.

use crate::theme::{self, BODY, MONO, SMALL};
use egui::text::{LayoutJob, TextFormat};
use egui::{CornerRadius, FontId, Margin, Stroke};

#[derive(Debug, PartialEq, Eq)]
pub enum Block<'a> {
    Para(String),
    Heading(usize, &'a str),
    Bullet(&'a str),
    Numbered(&'a str, &'a str),
    Quote(String),
    Code { lang: &'a str, body: String },
    Rule,
}

/// Split into blocks. An unterminated fence (a reply still streaming) is a code block so far.
pub fn parse(text: &str) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Block>| {
        if !para.is_empty() {
            out.push(Block::Para(para.join(" ")));
            para.clear();
        }
    };
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(lang) = trimmed.strip_prefix("```") {
            flush(&mut para, &mut out);
            let mut body = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                body.push(l);
            }
            out.push(Block::Code {
                lang: lang.trim(),
                body: body.join("\n"),
            });
        } else if trimmed.is_empty() {
            flush(&mut para, &mut out);
        } else if let Some((level, rest)) = heading(trimmed) {
            flush(&mut para, &mut out);
            out.push(Block::Heading(level, rest));
        } else if matches!(trimmed, "---" | "***" | "___") {
            flush(&mut para, &mut out);
            out.push(Block::Rule);
        } else if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            flush(&mut para, &mut out);
            out.push(Block::Bullet(rest));
        } else if let Some((num, rest)) = numbered(trimmed) {
            flush(&mut para, &mut out);
            out.push(Block::Numbered(num, rest));
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            flush(&mut para, &mut out);
            let mut q = vec![rest.trim_start()];
            while let Some(next) = lines.peek().and_then(|l| l.trim_start().strip_prefix('>')) {
                q.push(next.trim_start());
                lines.next();
            }
            out.push(Block::Quote(q.join(" ")));
        } else {
            para.push(trimmed);
        }
    }
    flush(&mut para, &mut out);
    out
}

fn heading(l: &str) -> Option<(usize, &str)> {
    let level = l.bytes().take_while(|b| *b == b'#').count();
    let rest = l[level..].strip_prefix(' ')?;
    (1..=6).contains(&level).then_some((level, rest))
}

fn numbered(l: &str) -> Option<(&str, &str)> {
    let digits = l.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = l[digits..].strip_prefix(". ")?;
    Some((&l[..digits], rest))
}

/// Inline spans: `code` and **bold**; everything else plain.
pub fn inline(job: &mut LayoutJob, text: &str, base: &TextFormat, ui: &egui::Ui) {
    let t = theme::tokens(ui);
    let code = TextFormat {
        font_id: FontId::monospace(MONO),
        color: t.fg,
        background: t.code,
        ..base.clone()
    };
    let bold = TextFormat {
        font_id: theme::semibold(base.font_id.size),
        ..base.clone()
    };
    // Thin gaps around inline code so its background does not touch the words.
    const GAP: f32 = 3.0;
    let mut gap = 0.0; // leading space of the next plain segment
    let plain = |job: &mut LayoutJob, s: &str, gap: &mut f32| {
        job.append(s, *gap, base.clone());
        *gap = 0.0;
    };
    let mut rest = text;
    while !rest.is_empty() {
        let next_code = rest.find('`');
        let next_bold = rest.find("**");
        match (next_code, next_bold) {
            (Some(c), b) if b.is_none_or(|b| c < b) => {
                let after = &rest[c + 1..];
                let Some(end) = after.find('`') else {
                    plain(job, rest, &mut gap);
                    break;
                };
                plain(job, &rest[..c], &mut gap);
                job.append(&after[..end], GAP, code.clone());
                gap = GAP;
                rest = &after[end + 1..];
            }
            (_, Some(b)) => {
                let after = &rest[b + 2..];
                let Some(end) = after.find("**") else {
                    plain(job, rest, &mut gap);
                    break;
                };
                plain(job, &rest[..b], &mut gap);
                job.append(&after[..end], gap, bold.clone());
                gap = 0.0;
                rest = &after[end + 2..];
            }
            _ => {
                plain(job, rest, &mut gap);
                break;
            }
        }
    }
}

fn text_job(ui: &egui::Ui, text: &str, font: FontId, color: egui::Color32) -> LayoutJob {
    let base = TextFormat {
        font_id: font,
        color,
        line_height: Some(BODY * 1.45),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    inline(&mut job, text, &base, ui);
    job
}

/// Draw `text`. `salt` keeps the code blocks' scroll areas apart between messages.
pub fn show(ui: &mut egui::Ui, text: &str, salt: usize) {
    let t = theme::tokens(ui);
    ui.spacing_mut().item_spacing.y = 8.0;
    for (i, block) in parse(text).into_iter().enumerate() {
        match block {
            Block::Para(p) => {
                ui.add(egui::Label::new(text_job(ui, &p, FontId::proportional(BODY), t.fg)).wrap());
            }
            Block::Heading(level, h) => {
                let size = match level {
                    1 => theme::HEADING,
                    2 => 16.0,
                    _ => BODY,
                };
                ui.add_space(2.0);
                ui.add(egui::Label::new(text_job(ui, h, theme::semibold(size), t.fg)).wrap());
            }
            Block::Bullet(b) => list_item(ui, "•", b),
            Block::Numbered(n, b) => list_item(ui, &format!("{n}."), b),
            Block::Quote(q) => {
                egui::Frame::new()
                    .inner_margin(Margin {
                        left: 10,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        let r = ui.add(
                            egui::Label::new(text_job(ui, &q, FontId::proportional(BODY), t.muted))
                                .wrap(),
                        );
                        let x = r.rect.left() - 10.0;
                        ui.painter()
                            .vline(x, r.rect.y_range(), Stroke::new(2.0, t.line));
                    });
            }
            Block::Rule => {
                ui.separator();
            }
            Block::Code { lang, body } => code_block(ui, lang, &body, (salt, i)),
        }
    }
}

fn list_item(ui: &mut egui::Ui, marker: &str, text: &str) {
    let t = theme::tokens(ui);
    ui.horizontal_top(|ui| {
        ui.add_sized(
            [18.0, BODY * 1.45],
            egui::Label::new(egui::RichText::new(marker).color(t.muted)),
        );
        ui.add(egui::Label::new(text_job(ui, text, FontId::proportional(BODY), t.fg)).wrap());
    });
}

/// Monospace block on the code ground; long lines scroll sideways instead of wrapping.
pub fn code_block(
    ui: &mut egui::Ui,
    lang: &str,
    body: &str,
    salt: impl std::hash::Hash + std::fmt::Debug,
) {
    let t = theme::tokens(ui);
    egui::Frame::new()
        .fill(t.code)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !lang.is_empty() {
                ui.label(
                    egui::RichText::new(lang)
                        .font(FontId::monospace(SMALL))
                        .color(t.muted),
                );
            }
            egui::ScrollArea::horizontal()
                .id_salt(("code", salt))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(body)
                                .font(FontId::monospace(MONO))
                                .color(t.fg),
                        )
                        .extend(),
                    );
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        let md = "# Plan\nFirst line\ncontinues here.\n\n- one\n* two\n1. first\n> quoted\n> more\n\n```rust\nfn main() {}\n```\n---\ntail";
        assert_eq!(
            parse(md),
            vec![
                Block::Heading(1, "Plan"),
                Block::Para("First line continues here.".into()),
                Block::Bullet("one"),
                Block::Bullet("two"),
                Block::Numbered("1", "first"),
                Block::Quote("quoted more".into()),
                Block::Code {
                    lang: "rust",
                    body: "fn main() {}".into()
                },
                Block::Rule,
                Block::Para("tail".into()),
            ]
        );
    }

    #[test]
    fn streaming_fence_is_code_so_far() {
        assert_eq!(
            parse("see:\n```\nlet x = 1;"),
            vec![
                Block::Para("see:".into()),
                Block::Code {
                    lang: "",
                    body: "let x = 1;".into()
                }
            ]
        );
        assert_eq!(
            parse("#notaheading"),
            vec![Block::Para("#notaheading".into())]
        );
    }

    #[test]
    fn inline_spans() {
        let mut parts = Vec::new();
        let mut h = egui_kittest::Harness::new_ui(|ui| {
            let mut job = LayoutJob::default();
            inline(
                &mut job,
                "run `cargo test` **now** or `never",
                &TextFormat::default(),
                ui,
            );
            parts = job
                .sections
                .iter()
                .map(|s| job.text[s.byte_range.start.0..s.byte_range.end.0].to_owned())
                .collect::<Vec<_>>();
        });
        h.run();
        drop(h);
        assert_eq!(parts, ["run ", "cargo test", " ", "now", " or `never"]);
    }
}
