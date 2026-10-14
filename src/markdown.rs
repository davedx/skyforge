use eframe::egui::{self, Align, Layout, RichText, Ui};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Alignment, Event, Parser, Tag, TagEnd};
use std::borrow::Cow;
use std::ops::Range;

struct ListBlock {
    range: Range<usize>,
    start: Option<u64>,
    items: Vec<Range<usize>>,
}

// Keep the existing renderer for rich content, but give list markers and bodies
// separate columns. Its inline marker widgets cannot align or indent wrapped text.
pub fn show(ui: &mut Ui, cache: &mut CommonMarkCache, text: &str) {
    let mut cursor = 0;
    for list in lists(text) {
        render(ui, cache, &text[cursor..list.range.start]);
        let last_number = list.start.unwrap_or(1) + list.items.len().saturating_sub(1) as u64;
        let marker_width = ui
            .fonts(|fonts| {
                fonts
                    .layout_no_wrap(
                        format!("{last_number}."),
                        egui::TextStyle::Body.resolve(ui.style()),
                        ui.visuals().text_color(),
                    )
                    .size()
                    .x
            })
            .max(24.0);
        for (index, range) in list.items.iter().enumerate() {
            let body = item_body(&text[range.clone()]);
            let task = if body.starts_with("[ ] ") {
                Some(false)
            } else if body.starts_with("[x] ") || body.starts_with("[X] ") {
                Some(true)
            } else {
                None
            };
            let marker = list
                .start
                .map_or_else(|| "•".into(), |start| format!("{}.", start + index as u64));
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(marker_width, 0.0),
                    Layout::right_to_left(Align::Min),
                    |ui| {
                        if let Some(mut checked) = task {
                            ui.add_enabled(false, egui::Checkbox::without_text(&mut checked));
                        } else {
                            ui.label(RichText::new(marker).color(ui.visuals().strong_text_color()));
                        }
                    },
                );
                let width = ui.available_width();
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    show(ui, cache, if task.is_some() { &body[4..] } else { &body });
                });
            });
        }
        cursor = list.range.end;
    }
    render(ui, cache, &text[cursor..]);
}

fn render(ui: &mut Ui, cache: &mut CommonMarkCache, text: &str) {
    let mut cursor = 0;
    for table in tables(text) {
        render_markdown(ui, cache, &text[cursor..table.range.start]);
        render_table(ui, cache, text, &table);
        cursor = table.range.end;
    }
    render_markdown(ui, cache, &text[cursor..]);
}

fn render_markdown(ui: &mut Ui, cache: &mut CommonMarkCache, text: &str) {
    if !text.trim().is_empty() {
        CommonMarkViewer::new().show(ui, cache, &autolink_urls(text));
    }
}

// Only rewrite prose: Markdown links, image labels, and code must retain their
// original meaning. Offset ranges let us preserve all other source formatting.
fn autolink_urls(text: &str) -> Cow<'_, str> {
    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    let mut protected_depth = 0;
    let mut output = String::new();
    let mut cursor = 0;
    let events = Parser::new_ext(text, pulldown_cmark::Options::all()).into_offset_iter();
    for (event, range) in pulldown_cmark::TextMergeWithOffset::new(events) {
        match event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. } | Tag::CodeBlock(_)) => {
                protected_depth += 1;
            }
            Event::End(TagEnd::Link | TagEnd::Image | TagEnd::CodeBlock) => {
                protected_depth -= 1;
            }
            Event::Text(_) if protected_depth == 0 => {
                for link in finder.links(&text[range.clone()]) {
                    let url = link.as_str();
                    if !(url.starts_with("https://") || url.starts_with("http://")) {
                        continue;
                    }
                    let start = range.start + link.start();
                    let end = range.start + link.end();
                    output.push_str(&text[cursor..start]);
                    output.push('<');
                    output.push_str(url);
                    output.push('>');
                    cursor = end;
                }
            }
            _ => {}
        }
    }
    if cursor == 0 {
        Cow::Borrowed(text)
    } else {
        output.push_str(&text[cursor..]);
        Cow::Owned(output)
    }
}

struct TableBlock {
    range: Range<usize>,
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Range<usize>>>,
}

fn tables(text: &str) -> Vec<TableBlock> {
    let mut blocks = Vec::new();
    let mut depth = 0;
    let mut current: Option<TableBlock> = None;
    for (event, range) in Parser::new_ext(text, pulldown_cmark::Options::all()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    if let Tag::Table(alignments) = &tag {
                        current = Some(TableBlock {
                            range: range.clone(),
                            alignments: alignments.clone(),
                            rows: Vec::new(),
                        });
                    }
                }
                if let Some(table) = &mut current {
                    match tag {
                        Tag::TableHead | Tag::TableRow => table.rows.push(Vec::new()),
                        Tag::TableCell => table.rows.last_mut().unwrap().push(range),
                        _ => {}
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0 && tag == TagEnd::Table {
                    if let Some(table) = current.take() {
                        blocks.push(table);
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

fn render_table(ui: &mut Ui, cache: &mut CommonMarkCache, text: &str, table: &TableBlock) {
    let columns = table.alignments.len();
    if columns == 0 {
        return;
    }
    let id = ui.next_auto_id();
    egui::ScrollArea::horizontal()
        .id_salt(id)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let spacing = ui.spacing().item_spacing.x;
                let width = ((ui.available_width() - spacing * (columns - 1) as f32)
                    / columns as f32)
                    .max(160.0);
                for (row_index, row) in table.rows.iter().enumerate() {
                    let background = if row_index % 2 == 1 {
                        ui.visuals().faint_bg_color
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    egui::Frame::new().fill(background).show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            for (column, alignment) in table.alignments.iter().enumerate() {
                                let align = match alignment {
                                    Alignment::Center => Align::Center,
                                    Alignment::Right => Align::Max,
                                    _ => Align::Min,
                                };
                                ui.allocate_ui_with_layout(
                                    egui::vec2(width, 0.0),
                                    Layout::top_down(align),
                                    |ui| {
                                        ui.set_width(width);
                                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                                        if row_index == 0 {
                                            ui.visuals_mut().override_text_color =
                                                Some(ui.visuals().strong_text_color());
                                        }
                                        if let Some(cell) = row.get(column) {
                                            render_markdown(ui, cache, text[cell.clone()].trim());
                                        }
                                    },
                                );
                            }
                        });
                    });
                }
            });
        });
}

fn lists(text: &str) -> Vec<ListBlock> {
    let mut blocks = Vec::new();
    let mut depth = 0;
    let mut current: Option<ListBlock> = None;
    for (event, range) in Parser::new_ext(text, pulldown_cmark::Options::all()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    if let Tag::List(start) = tag {
                        current = Some(ListBlock {
                            range,
                            start,
                            items: Vec::new(),
                        });
                    }
                } else if depth == 1 && matches!(tag, Tag::Item) {
                    if let Some(list) = &mut current {
                        list.items.push(range);
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth -= 1;
                if depth == 0 && matches!(tag, TagEnd::List(_)) {
                    if let Some(list) = current.take() {
                        blocks.push(list);
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

fn item_body(text: &str) -> String {
    let marker_end = text.find([' ', '\t']).unwrap_or(text.len());
    let content_start = text[marker_end..]
        .find(|c: char| c != ' ' && c != '\t')
        .map_or(marker_end, |offset| marker_end + offset);
    let mut lines = text[content_start..].split('\n');
    let mut body = lines.next().unwrap_or_default().to_owned();
    for line in lines {
        body.push('\n');
        let indent = line
            .bytes()
            .take_while(|b| *b == b' ')
            .count()
            .min(content_start);
        body.push_str(&line[indent..]);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_urls_become_links_without_sentence_punctuation() {
        let pr = "https://github.com/Covariant-Group-LLC/acelero-ai-assessment/pull/270";
        let source =
            format!("Verification: the branch was pushed as new PR {pr} which fixes #267.");
        assert_eq!(
            autolink_urls(&source),
            format!("Verification: the branch was pushed as new PR <{pr}> which fixes #267.")
        );
        assert_eq!(
            autolink_urls("See (https://example.com/a_(b)), then http://example.com/path?q=1&x=2."),
            "See (<https://example.com/a_(b)>), then <http://example.com/path?q=1&x=2>."
        );
        assert_eq!(
            autolink_urls("- **PR:** https://example.com/pr\n\n| Link |\n| --- |\n| https://example.com/table |"),
            "- **PR:** <https://example.com/pr>\n\n| Link |\n| --- |\n| <https://example.com/table> |"
        );
    }

    #[test]
    fn autolinking_preserves_code_images_and_existing_links() {
        let source = "[https://example.com/label](https://example.com/target)\n\n<https://example.com/auto>\n\n![https://example.com/alt](https://example.com/image)\n\n`https://example.com/inline`\n\n```text\nhttps://example.com/fenced\n```\n\n    https://example.com/indented\n\n[https://example.com/reference][ref]\n\n[ref]: https://example.com/destination";
        assert!(matches!(autolink_urls(source), Cow::Borrowed(_)));
        assert_eq!(autolink_urls(source), source);
    }

    #[test]
    fn clicking_a_bare_pr_url_opens_the_browser() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut cache = CommonMarkCache::default();
        let url = "https://github.com/example/project/pull/270";
        let source = format!("New PR {url} which fixes #267.");
        let draw = |ui: &mut Ui, cache: &mut CommonMarkCache| show(ui, cache, &source);
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| draw(ui, &mut cache));
        });
        let position = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    if text.galley.job.text == url {
                        return Some(text.pos + text.galley.rect.center().to_vec2());
                    }
                }
                None
            })
            .expect("URL rendered as a hyperlink widget");
        let output = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| draw(ui, &mut cache));
            },
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::OpenUrl(open) if open.url == url)
        }));
    }

    #[test]
    fn tables_preserve_cell_markdown_alignment_and_surrounding_content() {
        let source = "Before\n\n| Requested change | How we can populate it | Remaining question |\n| :--- | :---: | ---: |\n| **Teacher** | Use `HS` and [docs](https://example.com) | |\n\nAfter";
        let blocks = tables(source);
        assert_eq!(blocks.len(), 1);
        let table = &blocks[0];
        assert_eq!(
            table.alignments,
            vec![Alignment::Left, Alignment::Center, Alignment::Right]
        );
        assert_eq!(table.rows.len(), 2);
        assert_eq!(source[table.rows[0][0].clone()].trim(), "Requested change");
        assert_eq!(source[table.rows[1][0].clone()].trim(), "**Teacher**");
        assert_eq!(
            source[table.rows[1][1].clone()].trim(),
            "Use `HS` and [docs](https://example.com)"
        );
        assert!(source[table.rows[1][2].clone()].trim().is_empty());
        assert_eq!(source[..table.range.start].trim(), "Before");
        assert_eq!(source[table.range.end..].trim(), "After");
        assert!(tables("```\n| A | B |\n| --- | --- |\n```").is_empty());
        let list = "- Item\n\n  | A | B |\n  | --- | --- |\n  | one | two |";
        let list_blocks = lists(list);
        assert_eq!(
            tables(&item_body(&list[list_blocks[0].items[0].clone()])).len(),
            1
        );
    }

    #[test]
    fn table_columns_wrap_readably_and_rows_align_on_first_and_later_frames() {
        let source = "| Requested change | How we can populate it | Remaining question |\n| --- | --- | --- |\n| Teacher seniority | Existing teachers already have seniority; adding an enum value needs no backfill. Future records could use the new value. | Which rule should apply? |";
        for viewport in [900.0, 300.0] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx);
            let mut cache = CommonMarkCache::default();
            for _ in 0..3 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(viewport, 600.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let top = ui.cursor().top();
                            show(ui, &mut cache, source);
                            assert!(ui.cursor().top() - top < 450.0);
                            assert!(ui.min_rect().width() <= viewport);
                        });
                    },
                );
                let texts: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Text(text) = &shape.shape {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .collect();
                let find = |prefix: &str| {
                    *texts
                        .iter()
                        .find(|text| text.galley.job.text.starts_with(prefix))
                        .unwrap_or_else(|| panic!("Missing {prefix} at width {viewport}"))
                };
                let left = find("Requested change");
                let middle = find("How we can populate it");
                assert!(middle.pos.x - left.pos.x >= 160.0);
                assert!((left.pos.y - middle.pos.y).abs() < 0.5);
                assert!(left.galley.rows.len() <= 2);
                let body = find("Existing teachers");
                assert!(body.galley.rows.len() > 1);
                assert!(body.galley.rect.width() <= (middle.pos.x - left.pos.x));
                assert!((find("Teacher seniority").pos.y - body.pos.y).abs() < 0.5);
                if viewport > 500.0 {
                    let right = find("Remaining question");
                    assert!((middle.pos.x - left.pos.x - (right.pos.x - middle.pos.x)).abs() < 1.0);
                    assert!((right.pos.y - middle.pos.y).abs() < 0.5);
                    assert!(right.galley.rows.len() <= 2);
                    assert!((find("Which rule").pos.y - body.pos.y).abs() < 0.5);
                }
            }
        }
    }

    #[test]
    fn markers_align_with_first_lines_and_wrapping_keeps_the_body_column() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut cache = CommonMarkCache::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(276.0, 600.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(260.0);
                show(
                    ui,
                    &mut cache,
                    "9. First item has enough words to wrap across several lines.\n10. Second item",
                );
            });
        });
        let text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    Some(text)
                } else {
                    None
                }
            })
            .collect();
        let marker = |value: &str| {
            text.iter()
                .find(|shape| shape.galley.job.text == value)
                .unwrap()
        };
        let first = text
            .iter()
            .find(|shape| shape.galley.job.text.starts_with("First item"))
            .unwrap();
        let second = text
            .iter()
            .find(|shape| shape.galley.job.text == "Second item")
            .unwrap();
        assert!((marker("9.").pos.y - first.pos.y).abs() < 0.5);
        assert!((marker("10.").pos.y - second.pos.y).abs() < 0.5);
        assert!((first.pos.x - second.pos.x).abs() < 0.5);
        assert!(
            first.galley.rows.len() > 1,
            "{:?} {:?}",
            first.galley.job.wrap,
            first.galley.rect
        );
        for row in &first.galley.rows {
            assert!(row.rect.left().abs() < 0.5);
        }
    }

    #[test]
    fn list_items_preserve_rich_content_and_nested_indentation() {
        let source = "Intro\n\n9. **First**\n   wrapped line\n   - nested\n\n10. Second\n\nAfter";
        let blocks = lists(source);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start, Some(9));
        assert_eq!(blocks[0].items.len(), 2);
        let body = item_body(&source[blocks[0].items[0].clone()]);
        assert_eq!(body.trim_end(), "**First**\nwrapped line\n- nested");
        assert_eq!(lists(&body).len(), 1);
        assert_eq!(
            item_body(&source[blocks[0].items[1].clone()]).trim(),
            "Second"
        );
        assert_eq!(source[blocks[0].range.end..].trim(), "After");
    }

    #[test]
    fn code_and_quoted_lists_stay_with_the_markdown_renderer() {
        assert!(lists("```\n- code\n```\n\n> - quoted").is_empty());
        assert_eq!(item_body("- item\n\n      code"), "item\n\n    code");
    }
}
