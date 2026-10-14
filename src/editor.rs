use crate::theme::{LINE, PROMPT_CODE_SIZE, PROMPT_HEADING_SIZE, PROMPT_SIZE};
use eframe::egui::{text::LayoutJob, Color32, FontFamily, FontId, TextFormat};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::path::PathBuf;

pub fn markdown_job(text: &str, width: f32, color: Color32) -> LayoutJob {
    let base = TextFormat {
        font_id: FontId::proportional(PROMPT_SIZE),
        color,
        ..Default::default()
    };
    let mut spans: Vec<(std::ops::Range<usize>, TextFormat)> = vec![];
    let (mut italic, mut bold, mut code, mut heading) = (false, false, false, false);
    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::CodeBlock(_)) => code = true,
            Event::End(TagEnd::CodeBlock) => code = false,
            Event::Start(Tag::Heading { .. }) => heading = true,
            Event::End(TagEnd::Heading(_)) => heading = false,
            Event::Text(_) | Event::Code(_) => {
                let inline_code = matches!(event, Event::Code(_));
                let format = TextFormat {
                    italics: italic,
                    font_id: if code || inline_code {
                        FontId::monospace(PROMPT_CODE_SIZE)
                    } else {
                        FontId::new(
                            if heading {
                                PROMPT_HEADING_SIZE
                            } else {
                                PROMPT_SIZE
                            },
                            if bold || heading {
                                FontFamily::Name("strong".into())
                            } else {
                                FontFamily::Proportional
                            },
                        )
                    },
                    color,
                    background: if inline_code {
                        LINE
                    } else {
                        Color32::TRANSPARENT
                    },
                    ..base.clone()
                };
                spans.push((range, format));
            }
            _ => {}
        }
    }
    spans.sort_by_key(|s| s.0.start);
    let mut job = LayoutJob::default();
    let mut cursor = 0;
    for (range, format) in spans {
        if range.start < cursor || range.end > text.len() {
            continue;
        }
        if range.start > cursor {
            job.append(&text[cursor..range.start], 0.0, base.clone());
        }
        job.append(&text[range.clone()], 0.0, format);
        cursor = range.end;
    }
    job.append(&text[cursor..], 0.0, base);
    job.wrap.max_width = width;
    job
}

pub struct FileBuffer {
    pub path: PathBuf,
    pub text: String,
    original: String,
    observed: String,
    pub find: String,
    pub find_match: Option<std::ops::Range<usize>>,
}

impl FileBuffer {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if size > 2 * 1024 * 1024 {
            return Err("The prototype editor supports text files up to 2 MB.".into());
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("Cannot open text file: {e}"))?;
        Ok(Self {
            path,
            original: text.clone(),
            observed: text.clone(),
            text,
            find: String::new(),
            find_match: None,
        })
    }

    pub fn dirty(&self) -> bool {
        self.text != self.original
    }

    pub fn find_next(&mut self, backwards: bool) {
        if self.find.is_empty() {
            self.find_match = None;
            return;
        }
        let matches: Vec<_> = self
            .text
            .match_indices(&self.find)
            .map(|(start, text)| start..start + text.len())
            .collect();
        let current = self.find_match.as_ref().map(|range| range.start);
        self.find_match = if backwards {
            matches
                .iter()
                .rev()
                .find(|range| current.is_some_and(|start| range.start < start))
                .or_else(|| matches.last())
                .cloned()
        } else {
            matches
                .iter()
                .find(|range| current.is_none_or(|start| range.start > start))
                .or_else(|| matches.first())
                .cloned()
        };
    }

    pub fn externally_changed(&self) -> bool {
        std::fs::read_to_string(&self.path)
            .map(|s| s != self.original)
            .unwrap_or(true)
    }

    /// Reload clean buffers; report a new conflict without discarding local edits.
    pub fn refresh(&mut self) -> Result<bool, String> {
        let disk = Self::open(self.path.clone())?.text;
        let changed = disk != self.observed;
        self.observed.clone_from(&disk);
        if self.dirty() && self.text != disk {
            return Ok(changed && disk != self.original);
        }
        self.original.clone_from(&disk);
        if self.text != disk {
            self.find_match = None;
        }
        self.text = disk;
        Ok(false)
    }

    pub fn disk_conflict(&self) -> bool {
        self.observed != self.original && self.dirty()
    }

    pub fn load_disk_version(&mut self) -> Result<(), String> {
        let disk = Self::open(self.path.clone())?.text;
        self.original.clone_from(&disk);
        self.observed.clone_from(&disk);
        self.text = disk;
        self.find_match = None;
        Ok(())
    }

    pub fn save(&mut self) -> Result<(), String> {
        if self.externally_changed() {
            return Err("This file changed on disk. Resolve the conflict before saving; your edits are still here.".into());
        }
        std::fs::write(&self.path, &self.text).map_err(|e| e.to_string())?;
        self.original.clone_from(&self.text);
        self.observed.clone_from(&self.text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_layout_preserves_every_source_character_and_styles_emphasis() {
        let source = "# Title\n\nA *small* **bold** `code` prompt 🦀\n";
        let layout = markdown_job(source, 500.0, Color32::BLACK);
        assert_eq!(layout.text, source);
        assert!(layout
            .sections
            .iter()
            .any(|s| s.format.italics && &source[s.byte_range.clone()] == "small"));
    }
    #[test]
    fn file_search_wraps_in_both_directions_and_handles_unicode_and_missing_matches() {
        let path = std::env::temp_dir().join(format!("skyforge-find-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "🦀 café\nnext café\nlast café").unwrap();
        let mut buffer = FileBuffer::open(path.clone()).unwrap();
        buffer.find = "café".into();
        let matches: Vec<_> = buffer
            .text
            .match_indices(&buffer.find)
            .map(|(start, text)| start..start + text.len())
            .collect();
        for expected in [&matches[0], &matches[1], &matches[2], &matches[0]] {
            buffer.find_next(false);
            assert_eq!(buffer.find_match.as_ref(), Some(expected));
        }
        buffer.find_next(true);
        assert_eq!(buffer.find_match.as_ref(), Some(&matches[2]));
        buffer.find_next(true);
        assert_eq!(buffer.find_match.as_ref(), Some(&matches[1]));
        buffer.find_match = None;
        buffer.find_next(true);
        assert_eq!(buffer.find_match.as_ref(), Some(&matches[2]));
        buffer.find = "missing".into();
        buffer.find_next(false);
        assert!(buffer.find_match.is_none());
        buffer.find.clear();
        buffer.find_next(true);
        assert!(buffer.find_match.is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disk_refresh_updates_clean_buffers_and_preserves_find_and_local_edits() {
        let path =
            std::env::temp_dir().join(format!("skyforge-refresh-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&path, "original").unwrap();
        let mut buffer = FileBuffer::open(path.clone()).unwrap();
        buffer.find = "agent".into();
        std::fs::write(&path, "agent edit").unwrap();
        assert!(!buffer.refresh().unwrap());
        assert_eq!(buffer.text, "agent edit");
        assert_eq!(buffer.find, "agent");
        assert!(!buffer.dirty());

        buffer.text = "user edit".into();
        std::fs::write(&path, "another agent edit").unwrap();
        assert!(buffer.refresh().unwrap());
        assert!(buffer.disk_conflict());
        assert_eq!(buffer.text, "user edit");
        assert!(
            !buffer.refresh().unwrap(),
            "An acknowledged conflict must not prompt again"
        );
        assert!(buffer.save().is_err());
        buffer.load_disk_version().unwrap();
        assert_eq!(buffer.text, "another agent edit");
        assert_eq!(buffer.find, "agent");
        assert!(!buffer.dirty());

        buffer.text = "same edit".into();
        std::fs::write(&path, "same edit").unwrap();
        assert!(!buffer.refresh().unwrap());
        assert!(!buffer.dirty());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(buffer.refresh().is_err());
        assert_eq!(buffer.text, "same edit");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn editor_does_not_overwrite_an_external_change() {
        let path = std::env::temp_dir().join(format!("harness-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&path, "original").unwrap();
        let mut buffer = FileBuffer::open(path.clone()).unwrap();
        buffer.text = "user edit".into();
        std::fs::write(&path, "agent edit").unwrap();
        assert!(buffer.save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "agent edit");
        assert_eq!(buffer.text, "user edit");
        std::fs::remove_file(path).unwrap();
    }
}
