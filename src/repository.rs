//! Native notifications wake one background scanner; there is no idle polling.
use eframe::egui;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(not(target_os = "macos"))]
use std::collections::HashSet;

const QUIET: Duration = Duration::from_millis(150);
const MAX_DELAY: Duration = Duration::from_millis(500);

pub struct Snapshot {
    pub files: Vec<PathBuf>,
    pub error: Option<String>,
}

pub struct RepositoryWatch {
    pub root: PathBuf,
    trigger: SyncSender<()>,
    stopped: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Snapshot>>>,
}

impl RepositoryWatch {
    pub fn new(root: PathBuf, ctx: egui::Context) -> Self {
        // A single pending signal coalesces even very large checkout bursts.
        let (trigger, rx) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let watch = Self {
            root,
            trigger,
            stopped,
            latest,
        };
        let root = watch.root.clone();
        let trigger = watch.trigger.clone();
        let stopped = watch.stopped.clone();
        let latest = watch.latest.clone();
        std::thread::spawn(move || run(root, ctx, trigger, rx, stopped, latest));
        watch
    }

    pub fn refresh(&self) {
        let _ = self.trigger.try_send(());
    }

    pub fn take(&self) -> Option<Snapshot> {
        self.latest.lock().unwrap().take()
    }
}

impl Drop for RepositoryWatch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.refresh(); // Wake the worker so it releases its native watches.
    }
}

fn relevant(root: &Path, event: &Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|path| {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        if relative.components().any(|part| {
            let name = part.as_os_str().to_string_lossy();
            matches!(name.as_ref(), "target" | "node_modules")
                || (name.starts_with('.') && !matches!(name.as_ref(), ".gitignore" | ".ignore"))
        }) {
            return false;
        }
        // Content changes also wake the GUI so its open editor can refresh,
        // even when the repository's list of paths has not changed.
        true
    })
}

fn run(
    root: PathBuf,
    ctx: egui::Context,
    trigger: SyncSender<()>,
    rx: Receiver<()>,
    stopped: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Snapshot>>>,
) {
    let event_root = root.clone();
    #[cfg(not(target_os = "macos"))]
    let trigger_rescan = trigger.clone();
    let mut error = None;
    let mut watcher = match notify::recommended_watcher(move |event: notify::Result<Event>| {
        if event
            .as_ref()
            .map_or(true, |event| relevant(&event_root, event))
        {
            let _ = trigger.try_send(());
        }
    }) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            error = Some(e.to_string());
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    let mut watched = HashSet::new();
    // FSEvents handles a recursive root efficiently. Other backends subscribe
    // only to visible directories, avoiding watches throughout build outputs.
    if let Some(watcher) = &mut watcher {
        #[cfg(target_os = "macos")]
        let mode = RecursiveMode::Recursive;
        #[cfg(not(target_os = "macos"))]
        let mode = RecursiveMode::NonRecursive;
        match watcher.watch(&root, mode) {
            Ok(()) => {
                #[cfg(not(target_os = "macos"))]
                watched.insert(root.clone());
            }
            Err(e) => error = Some(e.to_string()),
        }
    }
    loop {
        if stopped.load(Ordering::Relaxed) {
            return;
        }
        #[cfg(not(target_os = "macos"))]
        let mut directories = HashSet::new();
        let mut files = Vec::new();
        for entry in ignore::WalkBuilder::new(&root)
            .hidden(true)
            .max_depth(Some(25))
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some("target" | "node_modules" | ".git" | ".harness")
                )
            })
            .build()
            .filter_map(Result::ok)
        {
            if stopped.load(Ordering::Relaxed) {
                return;
            }
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                #[cfg(not(target_os = "macos"))]
                {
                    let directory = entry.path().to_owned();
                    directories.insert(directory);
                }
            } else if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(entry.into_path());
                if files.len() == 10000 {
                    break;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            for directory in watched.difference(&directories) {
                if let Some(watcher) = &mut watcher {
                    let _ = watcher.unwatch(directory);
                }
            }
            watched.retain(|directory| directories.contains(directory));
            // Remove obsolete paths first: a renamed directory can retain the
            // same native watch descriptor under its new name.
            let mut added = false;
            for directory in &directories {
                if !watched.contains(directory) {
                    if let Some(watcher) = &mut watcher {
                        match watcher.watch(directory, RecursiveMode::NonRecursive) {
                            Ok(()) => {
                                watched.insert(directory.clone());
                                added = true;
                            }
                            Err(e) => error = Some(e.to_string()),
                        }
                    }
                }
            }
            // Reconcile once after subscribing to close the setup/scan gap.
            if added {
                let _ = trigger_rescan.try_send(());
            }
        }
        files.sort_unstable();
        *latest.lock().unwrap() = Some(Snapshot {
            files,
            error: error.take(),
        });
        ctx.request_repaint();
        if rx.recv().is_err() {
            return;
        }
        // Limit both the quiet period and total delay, so sustained changes
        // cannot postpone updates indefinitely. Events during a scan stay queued.
        let began = Instant::now();
        while !stopped.load(Ordering::Relaxed) {
            let remaining = MAX_DELAY.saturating_sub(began.elapsed());
            if remaining.is_zero() || rx.recv_timeout(QUIET.min(remaining)).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("harness-watch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Skyforge test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Skyforge test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn wait_for(watch: &RepositoryWatch, expected: &[&str]) {
        let mut expected: Vec<_> = expected.iter().map(|path| watch.root.join(path)).collect();
        expected.sort_unstable();
        let began = Instant::now();
        let mut last = None;
        while began.elapsed() < Duration::from_secs(10) {
            if let Some(snapshot) = watch.take() {
                assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
                if snapshot.files == expected {
                    return;
                }
                last = Some(snapshot.files);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("Expected {expected:?}, last snapshot: {last:?}");
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "Requires native FSEvents delivery outside the macOS agent sandbox"
    )]
    fn terminal_git_pull_and_nested_changes_refresh_without_manual_scan() {
        let temp = Temp::new();
        let source = temp.0.join("source");
        let project = temp.0.join("checkout");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("old.txt"), "old").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "initial"]);
        git(
            &temp.0,
            &["clone", source.to_str().unwrap(), project.to_str().unwrap()],
        );
        let watch = RepositoryWatch::new(project.clone(), egui::Context::default());
        wait_for(&watch, &["old.txt"]);

        std::fs::create_dir_all(source.join("src/nested")).unwrap();
        std::fs::write(source.join("src/nested/new.txt"), "new").unwrap();
        git(&source, &["mv", "old.txt", "renamed.txt"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "new directory and rename"]);
        git(&project, &["pull", "--ff-only"]);
        wait_for(&watch, &["renamed.txt", "src/nested/new.txt"]);

        // The newly discovered directory must itself remain watched.
        std::fs::write(project.join("src/nested/later.txt"), "later").unwrap();
        std::fs::remove_file(project.join("renamed.txt")).unwrap();
        wait_for(&watch, &["src/nested/later.txt", "src/nested/new.txt"]);
        std::fs::rename(project.join("src"), project.join("lib")).unwrap();
        wait_for(&watch, &["lib/nested/later.txt", "lib/nested/new.txt"]);
        std::fs::remove_dir_all(project.join("lib")).unwrap();
        wait_for(&watch, &[]);
    }

    #[test]
    fn refresh_reconciles_ignore_rules_and_newly_visible_directories() {
        let temp = Temp::new();
        git(&temp.0, &["init"]);
        std::fs::create_dir_all(temp.0.join("generated/nested")).unwrap();
        std::fs::write(temp.0.join("generated/nested/a.txt"), "a").unwrap();
        std::fs::write(temp.0.join(".gitignore"), "generated/\n").unwrap();
        let watch = RepositoryWatch::new(temp.0.clone(), egui::Context::default());
        wait_for(&watch, &[]);
        std::fs::write(temp.0.join(".gitignore"), "").unwrap();
        watch.refresh();
        wait_for(&watch, &["generated/nested/a.txt"]);
        std::fs::write(temp.0.join("generated/nested/b.txt"), "b").unwrap();
        watch.refresh();
        wait_for(
            &watch,
            &["generated/nested/a.txt", "generated/nested/b.txt"],
        );
        std::fs::write(temp.0.join(".gitignore"), "generated/\n").unwrap();
        watch.refresh();
        wait_for(&watch, &[]);
    }

    #[test]
    fn filters_access_and_build_output_but_keeps_content_edits_renames_and_rescans() {
        use notify::event::{AccessKind, DataChange, ModifyKind, RenameMode};
        let root = Path::new("/workspace");
        let event = |kind, path: &str| Event::new(kind).add_path(root.join(path));
        assert!(!relevant(
            root,
            &event(EventKind::Access(AccessKind::Read), "src/main.rs")
        ));
        assert!(relevant(
            root,
            &event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                "src/main.rs"
            )
        ));
        for path in [
            "target/debug/build",
            "node_modules/pkg/index.js",
            ".git/index",
            ".harness/state",
        ] {
            assert!(!relevant(root, &event(EventKind::Any, path)));
        }
        assert!(relevant(
            root,
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                "src/main.rs"
            )
        ));
        assert!(relevant(
            root,
            &event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                ".gitignore"
            )
        ));
        assert!(relevant(
            root,
            &Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
    }
}
