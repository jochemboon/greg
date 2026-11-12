//! The text editing widget: renders only the visible part of a document and
//! translates keyboard/mouse input into document commands.

use crate::buffer::{Document, Severity, advance_col};
use crate::highlight::{Highlighter, HlCache, ThemeColors};
use eframe::egui::{
    self, Color32, CursorIcon, Event, EventFilter, FontId, Id, Key, Modifiers, Pos2, Rect, Sense,
    Shape, Stroke, TextFormat, Ui, Vec2, text::LayoutJob,
};

pub fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Error => Color32::from_rgb(0xf1, 0x4c, 0x4c),
        Severity::Warning => Color32::from_rgb(0xcc, 0xa7, 0x00),
        Severity::Info => Color32::from_rgb(0x37, 0x94, 0xff),
        Severity::Hint => Color32::from_rgb(0x8a, 0x8a, 0x8a),
    }
}

/// Paint a small severity icon (circle for errors, triangle for warnings…).
/// Painted rather than drawn from a font so no emoji font is needed.
pub fn severity_icon(ui: &mut Ui, severity: Severity) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let c = rect.center();
    let r = size * 0.32;
    let color = severity_color(severity);
    match severity {
        Severity::Warning => {
            let pts = vec![
                Pos2::new(c.x, c.y - r * 1.1),
                Pos2::new(c.x + r * 1.15, c.y + r * 0.9),
                Pos2::new(c.x - r * 1.15, c.y + r * 0.9),
            ];
            ui.painter()
                .add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Severity::Error => {
            ui.painter().circle_filled(c, r, color);
        }
        _ => {
            ui.painter()
                .circle_stroke(c, r * 0.9, Stroke::new(1.5, color));
        }
    }
    resp
}

pub struct EditorParams<'a> {
    pub hl: &'a Highlighter,
    pub colors: &'a ThemeColors,
    pub font_size: f32,
    /// Find-in-file matches (char ranges, sorted).
    pub matches: &'a [(usize, usize)],
}

struct Metrics {
    font: FontId,
    char_w: f32,
    line_h: f32,
    text_dy: f32,
}

pub fn editor_ui(ui: &mut Ui, doc: &mut Document, id: Id, p: &EditorParams) {
    let font = FontId::monospace(p.font_size);
    let (char_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let line_h = (row_h * 1.35).round();
    let m = Metrics {
        font,
        char_w,
        line_h,
        text_dy: ((line_h - row_h) / 2.0).round(),
    };

    let focused = ui.memory(|mem| mem.has_focus(id));
    if focused {
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(
                id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            )
        });
        let page = ((ui.available_height() / line_h) as isize - 2).max(1);
        handle_keyboard(ui, doc, page);
    }

    let digits = doc.len_lines().to_string().len().max(3);
    let gutter_w = (digits as f32 + 3.0) * char_w;
    let text_pad = char_w;
    let content_w = gutter_w + text_pad + (doc.max_line_len() as f32 + 4.0) * char_w;
    let content_h = (doc.len_lines() as f32 + 5.0) * line_h;

    let scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let content_w = content_w.max(viewport.width());
            let content_h = content_h.max(viewport.height());
            let (rect, _) = ui.allocate_exact_size(Vec2::new(content_w, content_h), Sense::hover());
            let resp = ui.interact(rect, id, Sense::click_and_drag());
            let text_x0 = rect.min.x + gutter_w + text_pad;

            let pos_at = |doc: &Document, pt: Pos2| -> usize {
                let line = (((pt.y - rect.min.y) / line_h).floor().max(0.0) as usize)
                    .min(doc.len_lines() - 1);
                let col = ((pt.x - text_x0) / char_w).round().max(0.0) as usize;
                doc.pos_at_visual_col(line, col)
            };

            // ---- mouse ----
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Text);
            }
            let pointer = ui.input(|i| i.pointer.interact_pos());
            let shift = ui.input(|i| i.modifiers.shift);
            if let Some(pt) = pointer {
                if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                    resp.request_focus();
                    let pos = pos_at(doc, pt);
                    doc.set_cursor(pos, shift);
                    doc.preferred_col = None;
                } else if resp.dragged_by(egui::PointerButton::Primary) {
                    let pos = pos_at(doc, pt);
                    doc.set_cursor(pos, true);
                }
                if resp.triple_clicked() {
                    doc.select_line();
                } else if resp.double_clicked() {
                    let (s, e) = doc.word_at(pos_at(doc, pt));
                    doc.sel = crate::buffer::Selection { anchor: s, head: e };
                }
            }

            // ---- scroll to cursor ----
            if doc.scroll_to_cursor {
                doc.scroll_to_cursor = false;
                let line = doc.line_of(doc.sel.head) as f32;
                let col = doc.visual_col(doc.sel.head) as f32;
                let x = text_x0 + col * char_w;
                let y = rect.min.y + line * line_h;
                let r = Rect::from_min_max(
                    Pos2::new(x - gutter_w - 4.0 * char_w, y - 2.0 * line_h),
                    Pos2::new(x + 4.0 * char_w, y + 3.0 * line_h),
                );
                ui.scroll_to_rect(r, None);
            }

            paint(ui, doc, p, &m, rect, viewport, gutter_w, text_x0, focused);

            // ---- diagnostic hover ----
            if let (true, Some(pt)) = (resp.hovered(), ui.input(|i| i.pointer.hover_pos()))
                && pt.x > text_x0
                && !resp.dragged()
            {
                let line = ((pt.y - rect.min.y) / line_h).floor().max(0.0) as usize;
                let pos = pos_at(doc, pt);
                let hits: Vec<_> = doc
                    .diagnostics
                    .iter()
                    .filter(|d| {
                        (d.start <= pos && pos <= d.end && d.start != d.end)
                            || (d.start == d.end
                                && doc.line_of(d.start) == line
                                && pos.abs_diff(d.start) <= 1)
                    })
                    .collect();
                if !hits.is_empty() {
                    resp.show_tooltip_ui(|ui| {
                        ui.set_max_width(600.0);
                        for d in hits {
                            ui.horizontal_wrapped(|ui| {
                                severity_icon(ui, d.severity);
                                ui.label(&d.message);
                                if !d.source.is_empty() {
                                    ui.weak(&d.source);
                                }
                            });
                        }
                    });
                }
            }
        });
    let _ = scroll;
}

#[allow(clippy::too_many_arguments)]
fn paint(
    ui: &Ui,
    doc: &mut Document,
    p: &EditorParams,
    m: &Metrics,
    rect: Rect,
    viewport: Rect,
    gutter_w: f32,
    text_x0: f32,
    focused: bool,
) {
    let painter = ui.painter_at(ui.clip_rect());
    let c = p.colors;
    let line_h = m.line_h;
    let char_w = m.char_w;

    painter.rect_filled(ui.clip_rect(), 0.0, c.background);

    let total = doc.len_lines();
    let first = ((viewport.min.y / line_h).floor().max(0.0) as usize).min(total.saturating_sub(1));
    let last = (((viewport.max.y / line_h).ceil() as usize) + 1).min(total.saturating_sub(1));
    let first_col = (viewport.min.x / char_w).floor().max(0.0) as usize;
    let last_col = first_col + (viewport.width() / char_w).ceil() as usize + 2;

    // Syntax highlighting up to the last visible line.
    let first_line_text = doc.line_text(0);
    p.hl.attach(&mut doc.hl, doc.path.as_deref(), &first_line_text);
    {
        let rope = &doc.rope;
        let hl = &mut doc.hl;
        // Keep frames responsive: highlight for at most ~12ms per frame and
        // continue on the next one when jumping deep into a large file.
        let done =
            p.hl.ensure(hl, last, total, std::time::Duration::from_millis(12), |i| {
                rope.line(i).to_string()
            });
        if !done {
            ui.ctx().request_repaint();
        }
    }

    let (sel_s, sel_e) = doc.sel.range();
    let cursor_line = doc.line_of(doc.sel.head);
    let line_y = |l: usize| rect.min.y + l as f32 * line_h;
    let col_x = |col: usize| text_x0 + col as f32 * char_w;

    // Find matches starting from the first visible char.
    let first_char = doc.line_start(first);
    let last_char = doc.line_end(last);
    let match_start = p.matches.partition_point(|&(_, e)| e < first_char);

    for line in first..=last {
        let y = line_y(line);
        let ls = doc.line_start(line);
        let le = doc.line_end(line);

        if line == cursor_line && doc.sel.is_empty() {
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(rect.min.x, y), Vec2::new(rect.width(), line_h)),
                0.0,
                c.line_highlight,
            );
        }

        // Find matches.
        for &(s, e) in p.matches[match_start..]
            .iter()
            .take_while(|&&(s, _)| s <= last_char)
        {
            if e > ls && s <= le && s < e {
                let x1 = col_x(doc.visual_col(s.max(ls)));
                let x2 = col_x(doc.visual_col(e.min(le)));
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x1, y), Pos2::new(x2.max(x1 + 2.0), y + line_h)),
                    2.0,
                    Color32::from_rgba_unmultiplied(234, 92, 0, 70),
                );
            }
        }

        // Selection.
        if sel_s != sel_e && sel_e >= ls && sel_s <= le {
            let x1 = col_x(doc.visual_col(sel_s.max(ls)));
            let mut x2 = col_x(doc.visual_col(sel_e.min(le)));
            if sel_e > le {
                x2 += char_w * 0.6;
            }
            if x2 > x1 {
                painter.rect_filled(
                    Rect::from_min_max(Pos2::new(x1, y), Pos2::new(x2, y + line_h)),
                    0.0,
                    c.selection,
                );
            }
        }

        // Text.
        let text = doc.line_text(line);
        let spans = doc.hl.line(line);
        let (job, start_col) = layout_line(
            &text,
            spans,
            doc.tab_size,
            first_col,
            last_col,
            &m.font,
            c.foreground,
        );
        if !job.text.is_empty() {
            let galley = painter.layout_job(job);
            painter.galley(
                Pos2::new(col_x(start_col), y + m.text_dy),
                galley,
                c.foreground,
            );
        }
    }

    // Diagnostics: squiggly underlines.
    for d in doc.diagnostics.iter().rev() {
        let (dl1, dl2) = (doc.line_of(d.start), doc.line_of(d.end));
        if dl2 < first || dl1 > last {
            continue;
        }
        let color = severity_color(d.severity);
        for line in dl1.max(first)..=dl2.min(last) {
            let ls = doc.line_start(line);
            let le = doc.line_end(line);
            let s = d.start.max(ls);
            let e = d.end.min(le);
            let mut x1 = col_x(doc.visual_col(s));
            let mut x2 = col_x(doc.visual_col(e));
            if x2 - x1 < char_w {
                // Empty range: mark one char (or a small bit after line end).
                if s >= le && le > ls {
                    x1 -= char_w;
                }
                x2 = x1 + char_w;
            }
            let y = line_y(line) + line_h - 3.0;
            if d.severity == Severity::Hint {
                painter.line_segment(
                    [Pos2::new(x1, y + 1.0), Pos2::new(x2, y + 1.0)],
                    Stroke::new(1.0, color),
                );
            } else {
                painter.add(squiggle(x1, x2, y, color));
            }
        }
    }

    // Cursor.
    let cursor_col = doc.visual_col(doc.sel.head);
    let cx = col_x(cursor_col);
    let cy = line_y(cursor_line);
    let caret = if focused {
        c.caret
    } else {
        c.caret.gamma_multiply(0.4)
    };
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(cx - 1.0, cy + 1.0), Vec2::new(2.0, line_h - 2.0)),
        0.0,
        caret,
    );

    // Gutter (painted last so it covers horizontally scrolled text).
    let gx = rect.min.x + viewport.min.x;
    painter.rect_filled(
        Rect::from_min_size(
            Pos2::new(gx, ui.clip_rect().min.y),
            Vec2::new(gutter_w, ui.clip_rect().height()),
        ),
        0.0,
        c.background,
    );
    let mut worst: Vec<Option<Severity>> = vec![None; last + 1 - first];
    for d in &doc.diagnostics {
        let l = doc.line_of(d.start);
        if (first..=last).contains(&l) {
            let w = &mut worst[l - first];
            if w.is_none_or(|s| (d.severity as u8) < (s as u8)) {
                *w = Some(d.severity);
            }
        }
    }
    for line in first..=last {
        let y = line_y(line);
        let color = if line == cursor_line {
            c.foreground
        } else {
            c.gutter_fg
        };
        painter.text(
            Pos2::new(gx + gutter_w - 1.5 * char_w, y + m.text_dy),
            egui::Align2::RIGHT_TOP,
            (line + 1).to_string(),
            m.font.clone(),
            color,
        );
        if let Some(sev) = worst[line - first] {
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(gx + 3.0, y + 3.0), Vec2::new(3.0, line_h - 6.0)),
                1.0,
                severity_color(sev),
            );
        }
    }
}

/// Build a layout job for the visible columns of a line. Returns the job and
/// the visual column at which it starts.
fn layout_line(
    text: &str,
    spans: Option<&[crate::highlight::Span]>,
    tab_size: usize,
    first_col: usize,
    last_col: usize,
    font: &FontId,
    default: Color32,
) -> (LayoutJob, usize) {
    let mut job = LayoutJob::default();
    let mut start_col = None;
    let mut col = 0;
    let mut current = String::new();
    let mut cur_fmt: Option<(Color32, bool)> = None;

    let flush = |job: &mut LayoutJob, s: &mut String, fmt: Option<(Color32, bool)>| {
        if let (false, Some((color, italics))) = (s.is_empty(), fmt) {
            job.append(
                s,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color,
                    italics,
                    ..Default::default()
                },
            );
            s.clear();
        }
    };

    let mut span_iter = spans
        .map(|s| HlCache::ranges(s).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter();
    let mut span = span_iter.next();
    for (bi, ch) in text.char_indices() {
        while let Some((r, _)) = &span {
            if bi >= r.end {
                span = span_iter.next()
            } else {
                break;
            }
        }
        let fmt = span
            .as_ref()
            .map(|(_, s)| (s.color, s.italic))
            .unwrap_or((default, false));
        let next = advance_col(col, ch, tab_size);
        if col > last_col {
            break;
        }
        if next > first_col {
            if start_col.is_none() {
                start_col = Some(col);
            }
            if cur_fmt != Some(fmt) {
                flush(&mut job, &mut current, cur_fmt);
                cur_fmt = Some(fmt);
            }
            if ch == '\t' {
                current.extend(std::iter::repeat_n(' ', next - col));
            } else if ch.is_control() {
                current.push('·');
            } else {
                current.push(ch);
            }
        }
        col = next;
    }
    flush(&mut job, &mut current, cur_fmt);
    (job, start_col.unwrap_or(0))
}

fn squiggle(x1: f32, x2: f32, y: f32, color: Color32) -> Shape {
    let mut pts = Vec::new();
    let step = 2.0;
    let mut x = x1;
    let mut up = true;
    while x <= x2 {
        pts.push(Pos2::new(x, if up { y } else { y + 2.0 }));
        up = !up;
        x += step;
    }
    if pts.len() < 2 {
        pts.push(Pos2::new(x2, y));
    }
    Shape::line(pts, Stroke::new(1.0, color))
}

fn handle_keyboard(ui: &Ui, doc: &mut Document, page: isize) {
    let events = ui.input(|i| i.events.clone());
    for ev in events {
        match ev {
            Event::Text(t) => {
                if !t.chars().any(|c| c.is_control()) {
                    doc.insert(&t);
                }
            }
            Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => doc.insert(&t),
            Event::Copy => {
                let text = if doc.sel.is_empty() {
                    let (s, e) = doc.current_line_range();
                    doc.rope.slice(s..e).to_string()
                } else {
                    doc.selected_text()
                };
                ui.ctx().copy_text(text);
            }
            Event::Cut => {
                if doc.sel.is_empty() {
                    let (s, e) = doc.current_line_range();
                    ui.ctx().copy_text(doc.rope.slice(s..e).to_string());
                    doc.delete_range(s, e, crate::buffer::EditKind::Other);
                } else {
                    ui.ctx().copy_text(doc.selected_text());
                    let (s, e) = doc.sel.range();
                    doc.delete_range(s, e, crate::buffer::EditKind::Other);
                }
            }
            Event::Paste(t) => {
                let t = t.replace("\r\n", "\n");
                let t = if doc.line_ending == "\r\n" {
                    t.replace('\n', "\r\n")
                } else {
                    t
                };
                doc.insert(&t);
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => handle_key(doc, key, modifiers, page),
            _ => {}
        }
    }
}

fn handle_key(doc: &mut Document, key: Key, m: Modifiers, page: isize) {
    let shift = m.shift;
    let cmd = m.command;
    match key {
        Key::ArrowLeft => doc.move_horizontal(-1, cmd || m.alt, shift),
        Key::ArrowRight => doc.move_horizontal(1, cmd || m.alt, shift),
        Key::ArrowUp if m.alt && shift => {
            doc.duplicate_line();
            doc.move_vertical(-1, false);
        }
        Key::ArrowDown if m.alt && shift => doc.duplicate_line(),
        Key::ArrowUp if m.alt => doc.move_lines(-1),
        Key::ArrowDown if m.alt => doc.move_lines(1),
        Key::ArrowUp => doc.move_vertical(-1, shift),
        Key::ArrowDown => doc.move_vertical(1, shift),
        Key::PageUp => doc.move_vertical(-page, shift),
        Key::PageDown => doc.move_vertical(page, shift),
        Key::Home if cmd => doc.set_cursor(0, shift),
        Key::End if cmd => doc.set_cursor(doc.len_chars(), shift),
        Key::Home => doc.home(shift),
        Key::End => doc.end(shift),
        Key::Backspace if cmd => doc.delete_word_back(),
        Key::Backspace => doc.backspace(),
        Key::Delete if cmd => doc.delete_word_forward(),
        Key::Delete => doc.delete_forward(),
        Key::Enter if !cmd => doc.newline(),
        Key::Tab if shift => doc.dedent_lines(),
        Key::Tab if !cmd => doc.tab(),
        Key::Escape => {
            let h = doc.sel.head;
            doc.sel = crate::buffer::Selection::cursor(h);
        }
        Key::A if cmd => doc.select_all(),
        Key::Z if cmd && shift => doc.redo(),
        Key::Z if cmd => doc.undo(),
        Key::Y if cmd => doc.redo(),
        Key::Slash if cmd => doc.toggle_comment(),
        Key::L if cmd => doc.select_line(),
        Key::K if cmd && shift => {
            let (s, e) = doc.current_line_range();
            doc.delete_range(s, e, crate::buffer::EditKind::Other);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_window_and_tabs() {
        let font = FontId::monospace(12.0);
        let (job, start) = layout_line("\tabcdef", None, 4, 0, 100, &font, Color32::WHITE);
        assert_eq!(job.text, "    abcdef");
        assert_eq!(start, 0);
        let (job, start) = layout_line("\tabcdef", None, 4, 6, 7, &font, Color32::WHITE);
        assert_eq!(start, 6);
        assert_eq!(job.text, "cd");
    }
}
