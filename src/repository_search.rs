use eframe::egui;
use std::{
    io::Read,
    ops::Range,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MATCHES: usize = 1000;

#[derive(Clone, Debug)]
pub struct Match {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub line: usize,
    pub column: usize,
    pub preview: String,
}

#[derive(Default)]
pub struct Results {
    pub matches: Vec<Match>,
    pub skipped: usize,
    pub truncated: bool,
}

pub struct Job {
    pub receiver: Receiver<Results>,
    cancelled: Arc<AtomicBool>,
}

impl Job {
    pub fn start(files: Vec<PathBuf>, query: String, ctx: egui::Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        std::thread::spawn(move || {
            let results = search(&files, &query, &stop);
            if !stop.load(Ordering::Relaxed) {
                let _ = sender.send(results);
                ctx.request_repaint();
            }
        });
        Self {
            receiver,
            cancelled,
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn search(files: &[PathBuf], query: &str, cancelled: &AtomicBool) -> Results {
    let mut results = Results::default();
    if query.is_empty() {
        return results;
    }
    for path in files {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let text = (|| {
            let file = std::fs::File::open(path).ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() || metadata.len() > MAX_BYTES {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() as u64 > MAX_BYTES || bytes.contains(&0) {
                return None;
            }
            String::from_utf8(bytes).ok()
        })();
        let Some(text) = text else {
            results.skipped += 1;
            continue;
        };
        let mut line = 1;
        let mut line_start = 0;
        let mut previous = 0;
        for (start, matched) in text.match_indices(query) {
            if cancelled.load(Ordering::Relaxed) {
                return results;
            }
            if results.matches.len() == MAX_MATCHES {
                results.truncated = true;
                return results;
            }
            for (offset, _) in text[previous..start].match_indices('\n') {
                line += 1;
                line_start = previous + offset + 1;
            }
            previous = start;
            let line_end = text[start..].find('\n').map_or(text.len(), |i| start + i);
            // Keep previews bounded, including for minified source files.
            let before: String = text[line_start..start].chars().rev().take(60).collect();
            let before: String = before.chars().rev().collect();
            let after: String = text[start..line_end].chars().take(160).collect();
            results.matches.push(Match {
                path: path.clone(),
                range: start..start + matched.len(),
                line,
                column: text[line_start..start].chars().count() + 1,
                preview: format!("{before}{after}"),
            });
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_text_with_unicode_locations_and_skips_binary_and_large_files() {
        let root = std::env::temp_dir().join(format!("skyforge-search-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.rs");
        let binary = root.join("binary");
        let large = root.join("large");
        std::fs::write(&first, "🦀 café café\nnext café").unwrap();
        std::fs::write(&second, "café").unwrap();
        std::fs::write(&binary, "café\0".as_bytes()).unwrap();
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        let files = vec![
            first.clone(),
            second.clone(),
            binary,
            large,
            root.join("missing"),
        ];
        let results = search(&files, "café", &AtomicBool::new(false));
        assert_eq!(results.matches.len(), 4);
        assert_eq!(results.skipped, 3);
        assert_eq!(results.matches[0].range, 5..10);
        assert_eq!((results.matches[0].line, results.matches[0].column), (1, 3));
        assert_eq!((results.matches[2].line, results.matches[2].column), (2, 6));
        assert_eq!(results.matches[3].path, second);
        assert!(search(&files, "", &AtomicBool::new(false))
            .matches
            .is_empty());
        assert!(search(&files, "café", &AtomicBool::new(true))
            .matches
            .is_empty());
        std::fs::write(&first, "a".repeat(MAX_MATCHES + 1)).unwrap();
        let results = search(&[first], "a", &AtomicBool::new(false));
        assert_eq!(results.matches.len(), MAX_MATCHES);
        assert!(results.truncated);
        std::fs::remove_dir_all(root).unwrap();
    }
}
