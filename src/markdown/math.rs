use eframe::egui::{self, Color32, RichText, Ui};
use latex_rust::{Color, Dim, MathFont, SvgOptions};
use pulldown_cmark::{Event, Parser, Tag};
use std::{
    borrow::Cow,
    cell::{OnceCell, RefCell},
    collections::VecDeque,
    ops::Range,
    sync::Arc,
};

const CACHE_ENTRIES: usize = 128;
const CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Cache {
    font: OnceCell<Result<MathFont, String>>,
    entries: RefCell<VecDeque<(Key, Result<Asset, String>)>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    source: String,
    size: u32,
    color: Color32,
    inline: bool,
}

#[derive(Clone)]
struct Asset {
    uri: String,
    svg: Arc<[u8]>,
}

impl Cache {
    fn get(&self, ui: &Ui, source: &str, inline: bool) -> Result<Asset, String> {
        let size = egui::TextStyle::Body.resolve(ui.style()).size;
        let key = Key {
            source: source.into(),
            size: size.to_bits(),
            color: ui.visuals().text_color(),
            inline,
        };
        let mut entries = self.entries.borrow_mut();
        if let Some(index) = entries.iter().position(|(cached, _)| cached == &key) {
            let entry = entries.remove(index).unwrap();
            let result = entry.1.clone();
            entries.push_back(entry);
            return result;
        }
        let result = self.render(&key).map(|svg| Asset {
            uri: format!("skyforge-math-{}.svg", egui::Id::new(&key).value()),
            svg: svg.into_bytes().into(),
        });
        entries.push_back((key, result.clone()));
        // Evict loader textures as well as source SVGs when history or style changes.
        let mut bytes: usize = entries
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|asset| asset.svg.len())
            .sum();
        while entries.len() > CACHE_ENTRIES || bytes > CACHE_BYTES {
            if let Some((_, Ok(asset))) = entries.pop_front() {
                bytes -= asset.svg.len();
                ui.ctx().forget_image(&asset.uri);
            }
        }
        result
    }

    fn render(&self, key: &Key) -> Result<String, String> {
        if key.source.len() > 8192 {
            return Err("Formula is too long to typeset".into());
        }
        let font = self
            .font
            .get_or_init(|| MathFont::stix_two_math().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(Clone::clone)?;
        let options = SvgOptions {
            // SVG pt converts to 4/3 pixels. egui font sizes are logical pixels.
            font_size_pt: Dim::from_ieee32_bits((f32::from_bits(key.size) * 0.75).to_bits()),
            color: Color::rgb(key.color.r(), key.color.g(), key.color.b()),
            display: !key.inline,
        };
        let svg = latex_rust::latex_to_svg(&key.source, font, &options)
            .map_err(|error| error.to_string())?;
        if svg.len() > CACHE_BYTES {
            return Err("Formula is too large to typeset".into());
        }
        Ok(svg)
    }

    pub(super) fn show(&self, ui: &mut Ui, source: &str, inline: bool) {
        if !inline {
            ui.end_row();
        }
        let response = match self.get(ui, source, inline) {
            Ok(asset) => {
                let image = egui::Image::from_bytes(asset.uri, asset.svg)
                    .fit_to_original_size(1.0)
                    .max_width(ui.max_rect().width().max(1.0))
                    .sense(egui::Sense::click());
                ui.add(image).on_hover_text(source)
            }
            Err(error) => ui
                .label(RichText::new(source).monospace())
                .on_hover_text(format!("Could not typeset formula: {error}")),
        };
        response.context_menu(|ui| {
            if ui.button("Copy LaTeX").clicked() {
                ui.ctx().copy_text(source.to_owned());
                ui.close_menu();
            }
        });
        if !inline {
            ui.end_row();
        }
    }
}

// Normalize before Markdown consumes backslash escapes. Offset ranges protect
// code, links, images, HTML and existing dollar-delimited math verbatim.
pub(super) fn normalize(text: &str) -> Cow<'_, str> {
    let parser = Parser::new_ext(text, pulldown_cmark::Options::all());
    let mut protected: Vec<_> = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect();
    for (event, range) in parser.into_offset_iter() {
        if matches!(
            event,
            Event::Start(Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. })
                | Event::Code(_)
                | Event::Html(_)
                | Event::InlineHtml(_)
                | Event::InlineMath(_)
                | Event::DisplayMath(_)
        ) {
            protected.push(range);
        }
    }
    protected.sort_by_key(|range| range.start);
    let mut ranges = Vec::<Range<usize>>::new();
    for range in protected {
        if let Some(last) = ranges.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            ranges.push(range);
        }
    }
    let protected_at = |pos| {
        ranges
            .get(ranges.partition_point(|range| range.end <= pos))
            .filter(|range| range.start <= pos)
    };
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut cursor = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        if let Some(range) = protected_at(pos) {
            pos = range.end;
            continue;
        }
        if bytes[pos] == b'\\' {
            let start = pos;
            let delimiter = bytes.get(pos + 1);
            if matches!(delimiter, Some(b'[' | b'(')) {
                let close = if delimiter == Some(&b'[') { b']' } else { b')' };
                let mut end = pos + 2;
                while end + 1 < bytes.len() {
                    if protected_at(end).is_some() {
                        // An unmatched opener must not swallow a later code block
                        // or link when its closing delimiter eventually arrives.
                        break;
                    }
                    if bytes[end] == b'\\' {
                        if bytes[end + 1] == close {
                            let marker = if close == b']' { "$$" } else { "$" };
                            output.push_str(&text[cursor..pos]);
                            output.push_str(marker);
                            output.push_str(&text[pos + 2..end]);
                            output.push_str(marker);
                            pos = end + 2;
                            cursor = pos;
                            break;
                        }
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                if pos != start {
                    continue;
                }
            }
            // Escaped backslashes must not become math delimiters.
            pos += 2;
        } else if bytes[pos] == b'[' {
            // Some responses omit the backslashes around a standalone equation.
            // Only accept a whole line with an equality and a known TeX command.
            let line_start = text[..pos].rfind('\n').map_or(0, |index| index + 1);
            let line_end = text[pos..].find('\n').map_or(text.len(), |end| pos + end);
            let line = text[pos..line_end].trim_end();
            if text[line_start..pos].trim().is_empty()
                && line.ends_with(']')
                && line.contains('=')
                && ["\\frac", "\\text", "\\sqrt", "\\times", "\\sum"]
                    .iter()
                    .any(|command| line.contains(command))
                && !ranges
                    .iter()
                    .any(|range| range.start < line_end && range.end > pos)
            {
                output.push_str(&text[cursor..pos]);
                output.push_str("$$");
                output.push_str(&line[1..line.len() - 1]);
                output.push_str("$$");
                pos += line.len();
                cursor = pos;
            } else {
                pos += 1;
            }
        } else {
            pos += 1;
        }
    }
    if cursor == 0 {
        Cow::Borrowed(text)
    } else {
        output.push_str(&text[cursor..]);
        Cow::Owned(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENARIO: &str = r"P_{\text{scenario}}=P_{\text{before}}\times \frac{EPS_{\text{scenario}}}{EPS_{\text{baseline}}}";

    #[test]
    fn normalizes_llm_delimiters_and_standalone_bracket_equations() {
        assert_eq!(
            normalize(r"Before \(x^2\) and \[\frac{1}{2}\] after"),
            r"Before $x^2$ and $$\frac{1}{2}$$ after"
        );
        assert_eq!(
            normalize(&format!("[ {SCENARIO} ]")),
            format!("$$ {SCENARIO} $$")
        );
        assert_eq!(
            normalize("[ordinary brackets]\n[price = 20]\n[link](https://example.com)"),
            "[ordinary brackets]\n[price = 20]\n[link](https://example.com)"
        );
    }

    #[test]
    fn preserves_code_links_images_html_and_existing_math() {
        let source = r#"`\(code\)`

```latex
\[not math\]
```

    \[indented code\]

[\(label\)](https://example.com/\(path\))
![\[alt\]](image.png)
[reference][ref]

[ref]: https://example.com/\(reference\)

<span title="\(attribute\)">html</span>
$\text{\(existing\)}$ and $$\text{\[existing\]}$$
\\(escaped\\)
"#;
        assert_eq!(normalize(source), source);
        assert!(matches!(normalize(source), Cow::Borrowed(_)));
        let spanning_code = "\\[ before\n\n```text\ncode\n```\n\n\\]";
        assert_eq!(normalize(spanning_code), spanning_code);
    }

    #[test]
    fn streaming_prefixes_never_lose_source_or_panic() {
        let source = format!("Before \\[ {SCENARIO} \\] after λ");
        for end in source
            .char_indices()
            .map(|(end, _)| end)
            .chain([source.len()])
        {
            let prefix = &source[..end];
            let normalized = normalize(prefix);
            if !prefix.contains(r"\]") {
                assert_eq!(normalized, prefix);
            }
        }
        assert_eq!(normalize(r"\[unfinished"), r"\[unfinished");
        assert_eq!(normalize(r"text \(unfinished"), r"text \(unfinished");
        assert_eq!(
            normalize(r"\[x\] then \(unfinished"),
            r"$$x$$ then \(unfinished"
        );
    }

    #[test]
    fn typesets_the_scenario_and_caches_successes_and_failures_by_style() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        egui_extras::install_image_loaders(&ctx);
        let cache = Cache::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let asset = cache.get(ui, SCENARIO, false).expect("scenario typesets");
                let svg = std::str::from_utf8(&asset.svg).unwrap();
                assert!(svg.contains("<path"));
                assert!(svg.contains("<rect"), "fraction rule");
                assert!(svg.contains("#cccccc"), "dark theme foreground");
                cache.show(ui, SCENARIO, false);
                assert!(ctx
                    .try_load_image(&asset.uri, egui::load::SizeHint::default())
                    .is_ok());
                assert_eq!(cache.get(ui, SCENARIO, false).unwrap().uri, asset.uri);
                assert_eq!(cache.entries.borrow().len(), 1);
                assert_ne!(cache.get(ui, SCENARIO, true).unwrap().uri, asset.uri);
                ui.visuals_mut().override_text_color = Some(Color32::BLACK);
                assert_ne!(cache.get(ui, SCENARIO, false).unwrap().uri, asset.uri);
                assert!(cache.get(ui, r"\unknowncommand{x}", true).is_err());
                cache.show(ui, r"\frac{", false);
                assert!(
                    cache.get(ui, SCENARIO, false).is_ok(),
                    "recovers after malformed input"
                );
            });
        });
    }

    #[test]
    fn chat_renders_math_in_prose_lists_and_tables_at_narrow_widths() {
        for width in [300.0, 900.0] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx);
            egui_extras::install_image_loaders(&ctx);
            let mut cache = super::super::Cache::default();
            let source = format!(
                "Before \\(x^2\\) after\n\n\\[ {SCENARIO} \\]\n\n- Formula \\(\\frac{{1}}{{2}}\\)\n\n| Value |\n| --- |\n| $\\sqrt{{x}}$ |\n\n[ {SCENARIO} ]\n\nEnd"
            );
            for _ in 0..2 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 1200.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            super::super::show(ui, &mut cache, &source);
                            assert!(ui.min_rect().width() <= width);
                        });
                    },
                );
                let images: Vec<_> = output
                    .shapes
                    .iter()
                    .filter(|shape| shape.shape.texture_id() != egui::TextureId::default())
                    .map(|shape| shape.shape.visual_bounding_rect())
                    .collect();
                assert_eq!(
                    images.len(),
                    5,
                    "all formulas rendered as images at {width}"
                );
                assert!(images
                    .iter()
                    .all(|rect| rect.width() <= width && rect.height() > 5.0));
                assert!(
                    images[1].top() >= images[0].bottom(),
                    "display math gets its own row"
                );
                assert!(output.shapes.iter().any(|shape| matches!(
                    &shape.shape, egui::Shape::Text(text) if text.galley.job.text == "End"
                )));
            }
        }
    }

    #[test]
    fn streaming_math_recovers_and_cache_evicts_old_textures() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let cache = Cache::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let first = cache.get(ui, "x", false).unwrap();
                cache.show(ui, "x", false);
                for end in SCENARIO.char_indices().map(|(end, _)| end) {
                    cache.show(ui, &SCENARIO[..end], false);
                }
                assert!(cache.get(ui, SCENARIO, false).is_ok());
                for number in 0..CACHE_ENTRIES {
                    let _ = cache.get(ui, &number.to_string(), true).unwrap();
                }
                assert_eq!(cache.entries.borrow().len(), CACHE_ENTRIES);
                assert!(ctx
                    .try_load_image(&first.uri, egui::load::SizeHint::default())
                    .is_err());
            });
        });
    }
}
