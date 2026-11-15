//! Just enough Markdown for model replies: fenced code blocks, headings, bullet and numbered
//! lists, block quotes, tables, and inline `code` / **bold**. Parsed per frame for visible items only
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
    Code {
        lang: &'a str,
        body: String,
    },
    Rule,
    /// Header cells, column alignments, body rows.
    Table {
        head: Vec<String>,
        align: Vec<egui::Align>,
        rows: Vec<Vec<String>>,
    },
}

/// The cells of a `| a | b |` row (outer pipes optional, `\|` is a literal pipe).
fn cells(line: &str) -> Vec<String> {
    let l = line.trim();
    let l = l.strip_prefix('|').unwrap_or(l);
    let l = l.strip_suffix('|').unwrap_or(l);
    let mut out = vec![String::new()];
    let mut chars = l.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                out.last_mut().unwrap().push('|');
                chars.next();
            }
            '|' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out.into_iter().map(|c| c.trim().to_owned()).collect()
}

/// `|---|:--:|--:|` → the alignment of each column; None when it is not a separator row.
fn separator(line: &str) -> Option<Vec<egui::Align>> {
    if !line.contains('-') || !line.trim_start().starts_with(['|', '-', ':']) {
        return None;
    }
    cells(line)
        .iter()
        .map(|c| {
            let dashes = c.trim_matches(':');
            (!dashes.is_empty() && dashes.bytes().all(|b| b == b'-')).then(|| {
                match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => egui::Align::Center,
                    (false, true) => egui::Align::RIGHT,
                    _ => egui::Align::LEFT,
                }
            })
        })
        .collect()
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
        } else if trimmed.contains('|')
            && let Some(align) = lines.peek().and_then(|l| separator(l))
        {
            // A table: this line is the header, the next the separator, then the rows.
            flush(&mut para, &mut out);
            lines.next();
            let head = cells(trimmed);
            let mut rows = Vec::new();
            while let Some(row) = lines
                .peek()
                .filter(|l| l.contains('|') && !l.trim().is_empty())
            {
                let mut r = cells(row);
                r.resize(head.len(), String::new());
                rows.push(r);
                lines.next();
            }
            let mut align = align;
            align.resize(head.len(), egui::Align::LEFT);
            out.push(Block::Table { head, align, rows });
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
            Block::Table { head, align, rows } => table(ui, &head, &align, &rows, (salt, i)),
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

/// A table in a light frame: a bold header row, a line under it, striped body rows. Each
/// column gets its natural width when the table fits; otherwise narrow columns keep theirs and
/// the rest share what is left, wrapping. Every row is as tall as its tallest cell.
fn table(
    ui: &mut egui::Ui,
    head: &[String],
    align: &[egui::Align],
    rows: &[Vec<String>],
    salt: impl std::hash::Hash + std::fmt::Debug,
) {
    let t = theme::tokens(ui);
    let n = head.len().max(1);
    let gap = 16.0;
    let pad = 10.0;
    let head_font = theme::semibold(BODY);
    let body_font = FontId::proportional(BODY);
    // Natural (one-line) width of each column.
    let natural = |text: &str, font: &FontId| -> f32 {
        let job = text_job(ui, text, font.clone(), t.fg);
        ui.fonts_mut(|f| f.layout_job(job)).size().x
    };
    let mut want = vec![0.0f32; n];
    for (c, h) in head.iter().enumerate() {
        want[c] = want[c].max(natural(h, &head_font));
    }
    for r in rows {
        for (c, v) in r.iter().enumerate().take(n) {
            want[c] = want[c].max(natural(v, &body_font));
        }
    }
    let room = (ui.available_width() - 2.0 * pad - gap * (n as f32 - 1.0)).max(60.0 * n as f32);
    let widths = fit_columns(&want, room, 60.0);
    egui::Frame::new()
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(pad as i8, 6))
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt(("table", &salt))
                .auto_shrink([true, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(gap, 0.0);
                    let row = |ui: &mut egui::Ui, cells: &[String], font: &FontId, stripe: bool| {
                        let bg = ui.painter().add(egui::Shape::Noop);
                        let r = ui.horizontal_top(|ui| {
                            for (c, w) in widths.iter().enumerate() {
                                let text = cells.get(c).map(String::as_str).unwrap_or("");
                                ui.allocate_ui_with_layout(
                                    egui::vec2(*w, 0.0),
                                    egui::Layout::top_down(align[c]),
                                    |ui| {
                                        ui.set_width(*w);
                                        ui.add_space(5.0);
                                        ui.add(
                                            egui::Label::new(text_job(
                                                ui,
                                                text,
                                                font.clone(),
                                                t.fg,
                                            ))
                                            .wrap(),
                                        );
                                        ui.add_space(5.0);
                                    },
                                );
                            }
                        });
                        if stripe {
                            let rect = r.response.rect.expand2(egui::vec2(pad, 0.0));
                            ui.painter()
                                .set(bg, egui::Shape::rect_filled(rect, 0.0, t.raised));
                        }
                        r.response.rect
                    };
                    let h = row(ui, head, &head_font, false);
                    ui.painter().hline(
                        (h.left() - pad)..=(h.right() + pad),
                        h.bottom(),
                        Stroke::new(1.0, t.line),
                    );
                    for (i, r) in rows.iter().enumerate() {
                        row(ui, r, &body_font, i % 2 == 1);
                    }
                });
        });
}

/// Column widths for `room`: all natural widths when they fit; otherwise columns narrower than
/// an even share keep theirs and the others split the rest evenly (never below `min`).
fn fit_columns(want: &[f32], room: f32, min: f32) -> Vec<f32> {
    if want.iter().sum::<f32>() <= room {
        return want.to_vec();
    }
    let mut fixed = vec![false; want.len()];
    loop {
        let open = fixed.iter().filter(|f| !**f).count().max(1) as f32;
        let left = room
            - want
                .iter()
                .zip(&fixed)
                .filter(|(_, f)| **f)
                .map(|(w, _)| w)
                .sum::<f32>();
        let share = (left / open).max(min);
        let mut changed = false;
        for (c, w) in want.iter().enumerate() {
            if !fixed[c] && *w <= share {
                fixed[c] = true;
                changed = true;
            }
        }
        if !changed {
            return want
                .iter()
                .zip(&fixed)
                .map(|(w, f)| if *f { *w } else { share })
                .collect();
        }
    }
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
    fn tables() {
        let md = "Structure:\n| Section | Status | Key \\| Content |\n|:--|:-:|--:|\n| Summary | ✓ | `ref` |\n| Overview | ✓ |\n\nafter\n| not | a table |";
        let mut blocks = parse(md).into_iter();
        assert_eq!(blocks.next(), Some(Block::Para("Structure:".into())));
        let Some(Block::Table { head, align, rows }) = blocks.next() else {
            panic!("no table");
        };
        assert_eq!(head, ["Section", "Status", "Key | Content"]);
        assert_eq!(
            align,
            [egui::Align::LEFT, egui::Align::Center, egui::Align::RIGHT]
        );
        assert_eq!(
            rows,
            [
                vec!["Summary".to_owned(), "✓".into(), "`ref`".into()],
                vec!["Overview".to_owned(), "✓".into(), String::new()],
            ]
        );
        // Pipes without a separator row under them stay text.
        assert_eq!(
            blocks.collect::<Vec<_>>(),
            [Block::Para("after | not | a table |".into())]
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

#[cfg(test)]
mod table_layout {
    use egui_kittest::kittest::Queryable;

    #[test]
    fn rows_are_as_tall_as_their_wrapped_cells() {
        let md = "| A | B |\n|--|--|\n| x | one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen |\n| y | short |";
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 600.0))
            .build_ui_state(
                |ui, fonts: &mut bool| {
                    if !*fonts {
                        ui.ctx()
                            .set_fonts(fastframe_fonts::FontSetup::default().definitions());
                        *fonts = true;
                        return;
                    }
                    super::show(ui, md, 1);
                },
                false,
            );
        h.run();
        let long = h.get_by_label_contains("sixteen").rect();
        let next = h.get_by_label("y").rect();
        assert!(long.height() > 40.0, "it wraps: {long:?}");
        assert!(
            next.top() >= long.bottom(),
            "the next row starts below it: {long:?} {next:?}"
        );
        assert!(long.right() <= 420.0, "fits the width: {long:?}");
    }

    #[test]
    fn narrow_columns_keep_their_width() {
        let w = super::fit_columns(&[80.0, 30.0, 900.0], 400.0, 60.0);
        assert_eq!(w, [80.0, 30.0, 290.0]);
        assert_eq!(super::fit_columns(&[50.0, 60.0], 400.0, 60.0), [50.0, 60.0]);
    }
}
