use eframe::egui::{self, TextEdit};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct QuickOpen {
    pub open: bool,
    focus: bool,
    query: String,
    selected: usize,
}

impl QuickOpen {
    pub fn show(&mut self, ctx: &egui::Context) {
        *self = Self {
            open: true,
            focus: true,
            ..Self::default()
        };
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("quick_open_input")));
    }

    pub fn dialog(
        &mut self,
        ctx: &egui::Context,
        root: &Path,
        files: &[PathBuf],
        scanning: bool,
    ) -> Option<PathBuf> {
        if !self.open {
            return None;
        }
        let mut chosen = None;
        let response = egui::Modal::new(egui::Id::new("quick_open")).show(ctx, |ui| {
            ui.set_width(650.0_f32.min((ctx.screen_rect().width() - 60.0).max(100.0)));
            ui.heading("Open file");
            // Consume navigation before the text field handles these keys.
            let up = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
            let down = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
            let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let input = ui.add(
                TextEdit::singleline(&mut self.query)
                    .id(egui::Id::new("quick_open_input"))
                    .hint_text("Search files by name or path…")
                    .desired_width(f32::INFINITY),
            );
            if self.focus || up || down || enter {
                input.request_focus();
                self.focus = false;
            }
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    input.id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
            if input.changed() {
                self.selected = 0;
            }
            let matches = ranked_paths(root, files, &self.query);
            self.selected = self.selected.min(matches.len().saturating_sub(1));
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if down {
                self.selected = (self.selected + 1).min(matches.len().saturating_sub(1));
            }
            ui.label("↑ ↓ to select · Enter to open · Esc to close");
            if scanning {
                ui.label("Updating repository files…");
            }
            if matches.is_empty() && !scanning {
                ui.label(if self.query.trim().is_empty() {
                    "No files in this repository."
                } else {
                    "No matching files."
                });
            }
            egui::ScrollArea::vertical()
                .id_salt("quick_open_results")
                .max_height((ctx.screen_rect().height() * 0.55).max(100.0))
                .show(ui, |ui| {
                    for (index, (_, relative, path)) in matches.iter().enumerate() {
                        let row = ui.selectable_label(index == self.selected, relative);
                        if index == self.selected && (up || down || input.changed()) {
                            row.scroll_to_me(Some(egui::Align::Center));
                        }
                        if row.clicked() || (enter && index == self.selected) {
                            chosen = Some((*path).clone());
                        }
                    }
                });
        });
        if chosen.is_some() || response.should_close() {
            self.open = false;
        }
        chosen
    }
}

fn ranked_paths<'a>(
    root: &Path,
    files: &'a [PathBuf],
    query: &str,
) -> Vec<(i64, String, &'a PathBuf)> {
    let query = query.trim().replace('\\', "/").to_lowercase();
    let mut matches: Vec<_> = files
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(root).ok()?.to_string_lossy().into_owned();
            let lower = relative.to_lowercase();
            let score = fuzzy_score(&lower, &query)?;
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            let bonus = if query.is_empty() {
                0
            } else if name == query {
                10_000
            } else if name.starts_with(&query) {
                5_000
            } else if name.contains(&query) {
                2_000
            } else {
                0
            };
            Some((score + bonus, relative, path))
        })
        .collect();
    matches.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    matches.truncate(100);
    matches
}

// Ordered subsequence matching accepts abbreviations such as "sapr" for
// "src/app.rs". Prefer contiguous matches and word/path boundaries.
fn fuzzy_score(path: &str, query: &str) -> Option<i64> {
    let mut remaining = query.chars().peekable();
    let mut score = 0;
    let mut previous_match = false;
    let mut boundary = true;
    for ch in path.chars() {
        if remaining.peek() == Some(&ch) {
            remaining.next();
            score += 10 + if previous_match { 15 } else { 0 } + if boundary { 20 } else { 0 };
            previous_match = true;
        } else {
            previous_match = false;
        }
        boundary = matches!(ch, '/' | '_' | '-' | '.' | ' ');
    }
    remaining.peek().is_none().then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_names_and_fuzzy_paths_and_handles_unicode() {
        let root = Path::new("/repo");
        let files: Vec<_> = ["docs/app.rs.md", "src/app.rs", "app.rs", "src/café.rs"]
            .iter()
            .map(|name| root.join(name))
            .chain([PathBuf::from("/another/app.rs")])
            .collect();
        let names = |query| {
            ranked_paths(root, &files, query)
                .into_iter()
                .map(|(_, name, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("APP.RS"), ["app.rs", "src/app.rs", "docs/app.rs.md"]);
        assert_eq!(names("sapr"), ["src/app.rs", "docs/app.rs.md"]);
        assert_eq!(names("SRC\\CAFÉ"), ["src/café.rs"]);
        assert!(names("missing").is_empty());
        assert_eq!(names("").len(), 4);
    }
}
