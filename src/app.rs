use crate::{
    editor::{markdown_job, FileBuffer},
    restart::RestartControl,
    runner::{self, AgentConfig, Envelope, Event, RunHandle},
    store::{Message, Mode, SearchHit, Store, ThinkerProject, Workstream},
    theme::{self, ACCENT, INK, INPUT, LINE, LINK, MUTED, ON_ACCENT, PAPER, PROMPT_SIZE, SIDEBAR},
};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, TextEdit};
use egui_commonmark::CommonMarkCache;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};

fn application_data_directory(base: &Path) -> PathBuf {
    // Keep using existing state in place: attachment and session paths may be
    // absolute, and another pre-rename instance may still have SQLite open.
    let legacy = base.join("Harness");
    if legacy.join("history.sqlite3").is_file() {
        legacy
    } else {
        base.join("Skyforge")
    }
}

#[derive(Default)]
struct FileNode {
    path: PathBuf,
    children: BTreeMap<String, FileNode>,
}

impl FileNode {
    fn build(root: &Path, files: &[PathBuf]) -> Self {
        let mut tree = Self::default();
        for path in files {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let mut node = &mut tree;
            let mut absolute = root.to_path_buf();
            for component in relative.components() {
                let name = component.as_os_str().to_string_lossy().into_owned();
                absolute.push(&name);
                node = node.children.entry(name).or_default();
                node.path = absolute.clone();
            }
        }
        tree
    }
}

pub struct Skyforge {
    store: Store,
    tabs: Vec<Workstream>,
    active: usize,
    messages: Vec<Message>,
    runs: HashMap<String, RunHandle>,
    activities: HashMap<String, BTreeMap<String, String>>,
    tx: SyncSender<Envelope>,
    rx: Receiver<Envelope>,
    delegation_host: crate::delegation_ipc::Host,
    repository_watch: Option<crate::repository::RepositoryWatch>,
    files: Vec<PathBuf>,
    tree: FileNode,
    scanning: bool,
    file_query: String,
    history: bool,
    query: String,
    project_filter: bool,
    days_filter: i64,
    hits: Vec<SearchHit>,
    history_dirty: bool,
    editor: Option<FileBuffer>,
    show_editor: bool,
    pending_file: Option<PathBuf>,
    new_file: Option<NewFile>,
    find_open: bool,
    find_focus: bool,
    find_jump: bool,
    source_editor_id: Option<egui::Id>,
    file_decision: bool,
    reload_decision: bool,
    settings_open: bool,
    config: AgentConfig,
    model_choices: Vec<(String, String)>,
    model_discovery: Option<Receiver<Result<crate::local::ModelCatalog, String>>>,
    model_discovery_due: Option<Instant>,
    model_discovery_status: String,
    draft_changed: Option<Instant>,
    notice: Option<String>,
    markdown: CommonMarkCache,
    jump_to: Option<i64>,
    history_context: bool,
    follow_tail: bool,
    quit_decision: bool,
    allow_quit: bool,
    restart: RestartControl,
    restart_decision: bool,
    focus_prompt: bool,
    capture_path: Option<PathBuf>,
    capture_started: Instant,
    capture_requested: bool,
    projects: Vec<ThinkerProject>,
    browse_project: Option<String>,
    project_editor: Option<ThinkerProject>,
    memories_open: bool,
    memory_scope: String,
    memory_key: String,
    memory_content: String,
    memory_edit_id: Option<i64>,
}

struct NewFile {
    directory: PathBuf,
    name: String,
    error: Option<String>,
    focus: bool,
}

impl Skyforge {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        project: Option<PathBuf>,
        restart_committed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, String> {
        let data = std::env::var_os("HARNESS_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|d| application_data_directory(&d)))
            .ok_or("Cannot locate application data directory")?;
        let mut app = Self::load(&cc.egui_ctx, project, data)?;
        app.restart.set_handoff(restart_committed);
        Ok(app)
    }

    fn load(ctx: &egui::Context, project: Option<PathBuf>, data: PathBuf) -> Result<Self, String> {
        theme::apply(ctx);
        egui_extras::install_image_loaders(ctx);
        let store = Store::open(data.join("history.sqlite3")).map_err(|e| e.to_string())?;
        let project = project
            .or_else(|| {
                store
                    .setting("last_project")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
            })
            .or_else(|| std::env::current_dir().ok().filter(|p| p != Path::new("/")))
            .or_else(dirs::home_dir)
            .ok_or("Choose a project directory to launch Skyforge")?;
        let project = project
            .canonicalize()
            .map_err(|e| format!("Cannot open project: {e}"))?;
        if !project.is_dir() {
            return Err("Pass a repository directory, not a file.".into());
        }
        let mut tabs = store.opened().map_err(|e| e.to_string())?;
        if tabs.is_empty() {
            let w = Workstream::new(&project);
            store.save(&w).map_err(|e| e.to_string())?;
            tabs.push(w);
        }
        for tab in &mut tabs {
            if tab.mode == Mode::Builder {
                if let Ok(path) = Path::new(&tab.project).canonicalize() {
                    tab.project = path.to_string_lossy().into_owned();
                }
            }
        }
        let previous_active = store.setting("active_workstream");
        let project_active = store.setting(&format!("active_project:{}", project.display()));
        let matches_project = |w: &Workstream| {
            w.mode == Mode::Builder
                && (Path::new(&w.project) == project
                    || Path::new(&w.project)
                        .canonicalize()
                        .is_ok_and(|p| p == project))
        };
        let active = tabs
            .iter()
            .position(|w| {
                Some(w.id.as_str()) == previous_active.as_deref()
                    && (w.mode == Mode::Thinker || matches_project(w))
            })
            .or_else(|| {
                tabs.iter().position(|w| {
                    matches_project(w) && Some(w.id.as_str()) == project_active.as_deref()
                })
            })
            .or_else(|| tabs.iter().position(matches_project))
            .unwrap_or_else(|| {
                tabs.push(Workstream::new(&project));
                tabs.len() - 1
            });
        store.save(&tabs[active]).map_err(|e| e.to_string())?;
        let config = AgentConfig {
            local: crate::local::load(&store, &project),
            binary: store
                .setting("codex_binary")
                .unwrap_or_else(runner::find_codex),
            claude_binary: store
                .setting("claude_binary")
                .unwrap_or_else(runner::find_claude),
            search: store.setting("web_search").as_deref() != Some("false"),
            writable: store.setting("writable").as_deref() != Some("false"),
        };
        let model_choices = runner::model_choices(&config.local);
        let messages = store
            .messages(&tabs[active].id, None)
            .map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::sync_channel(256);
        let projects = store.projects().map_err(|e| e.to_string())?;
        let browse_project = tabs[active].thinker_project.clone();
        let mut app = Self {
            store,
            tabs,
            active,
            messages,
            config,
            tx,
            rx,
            delegation_host: crate::delegation_ipc::Host::new(ctx.clone())?,
            repository_watch: None,
            runs: HashMap::new(),
            activities: HashMap::new(),
            files: vec![],
            tree: FileNode::default(),
            scanning: false,
            file_query: String::new(),
            history: false,
            query: String::new(),
            project_filter: true,
            days_filter: 0,
            hits: vec![],
            history_dirty: true,
            editor: None,
            show_editor: false,
            pending_file: None,
            new_file: None,
            find_open: false,
            find_focus: false,
            find_jump: false,
            source_editor_id: None,
            file_decision: false,
            reload_decision: false,
            settings_open: false,
            model_choices,
            model_discovery: None,
            model_discovery_due: None,
            model_discovery_status: String::new(),
            draft_changed: None,
            notice: None,
            markdown: CommonMarkCache::default(),
            jump_to: None,
            history_context: false,
            follow_tail: true,
            quit_decision: false,
            allow_quit: false,
            restart: RestartControl::new(Default::default()).map_err(|e| e.to_string())?,
            restart_decision: false,
            focus_prompt: true,
            capture_path: std::env::var_os("HARNESS_CAPTURE_PATH").map(PathBuf::from),
            capture_started: Instant::now(),
            capture_requested: false,
            projects,
            browse_project,
            project_editor: None,
            memories_open: false,
            memory_scope: String::new(),
            memory_key: String::new(),
            memory_content: String::new(),
            memory_edit_id: None,
        };
        app.recover_pending_builders(ctx); // One recovery scan; subsequent requests arrive over IPC.
        app.refresh_local_models(ctx);
        app.scan(ctx);
        Ok(app)
    }

    fn save_active(&mut self) {
        if let Err(e) = self.store.save(&self.tabs[self.active]) {
            self.notice = Some(format!("Could not save workstream: {e}"));
        }
        self.draft_changed = None;
        self.history_dirty = true;
        self.remember_selection();
    }

    fn remember_selection(&mut self) {
        if self.tabs[self.active].mode == Mode::Builder {
            let _ = self.store.set_setting(
                &format!("active_project:{}", self.tabs[self.active].project),
                &self.tabs[self.active].id,
            );
            if let Err(e) = self
                .store
                .set_setting("last_project", &self.tabs[self.active].project)
            {
                self.notice = Some(e.to_string());
            }
        }
        let _ = self
            .store
            .set_setting("active_workstream", &self.tabs[self.active].id);
    }

    fn changed(&mut self, ctx: &egui::Context) {
        self.tabs[self.active].updated = chrono::Utc::now().timestamp();
        self.draft_changed = Some(Instant::now());
        ctx.request_repaint_after(Duration::from_millis(450));
    }

    fn select(&mut self, index: usize, ctx: &egui::Context) {
        self.save_active();
        let different_project = self.tabs[self.active].project != self.tabs[index].project;
        self.active = index;
        self.browse_project = self.tabs[index].thinker_project.clone();
        self.history_dirty = true;
        self.memories_open = false;
        self.memory_edit_id = None;
        self.memory_key.clear();
        self.memory_content.clear();
        let _ = self.store.set_setting(
            &format!("active_{}", self.tabs[index].mode.label()),
            &self.tabs[index].id,
        );
        match self.store.messages(&self.tabs[index].id, None) {
            Ok(messages) => self.messages = messages,
            Err(e) => {
                self.messages.clear();
                self.notice = Some(e.to_string());
            }
        }
        self.jump_to = None;
        self.history_context = false;
        self.follow_tail = true;
        self.focus_prompt = true;
        self.remember_selection();
        if different_project {
            self.file_query.clear();
            self.scan(ctx);
            self.history_dirty = true;
        }
    }

    fn new_tab(&mut self, project: PathBuf, ctx: &egui::Context) {
        self.save_active();
        let project = project.canonicalize().unwrap_or(project);
        let w = Workstream::new(&project);
        if let Err(e) = self.store.save(&w) {
            self.notice = Some(e.to_string());
            return;
        }
        self.tabs.push(w);
        self.select(self.tabs.len() - 1, ctx);
    }

    fn builder_projects(&self) -> Vec<String> {
        let mut projects = Vec::new();
        for w in &self.tabs {
            if w.opened && w.mode == Mode::Builder && !projects.contains(&w.project) {
                projects.push(w.project.clone());
            }
        }
        projects
    }

    fn visible_workstream(&self, w: &Workstream) -> bool {
        let active = &self.tabs[self.active];
        w.opened
            && w.mode == active.mode
            && (active.mode == Mode::Thinker || w.project == active.project)
    }

    fn open_project(&mut self, project: PathBuf, ctx: &egui::Context) {
        let project = match project.canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ => {
                self.notice = Some("Choose an existing project directory.".into());
                return;
            }
        };
        self.save_active();
        let project = project.to_string_lossy().into_owned();
        let previous = self.store.setting(&format!("active_project:{project}"));
        let matches = |w: &Workstream| w.opened && w.mode == Mode::Builder && w.project == project;
        let index = self
            .tabs
            .iter()
            .position(|w| matches(w) && Some(&w.id) == previous.as_ref())
            .or_else(|| self.tabs.iter().position(matches));
        self.history = false;
        if let Some(index) = index {
            self.select(index, ctx);
        } else {
            self.new_tab(PathBuf::from(project), ctx);
        }
    }

    fn switch_mode(&mut self, mode: Mode, ctx: &egui::Context) {
        if self.tabs[self.active].mode == mode {
            return;
        }
        self.save_active();
        self.history = false;
        let previous = self.store.setting(&format!("active_{}", mode.label()));
        let index = self
            .tabs
            .iter()
            .position(|w| w.opened && w.mode == mode && Some(&w.id) == previous.as_ref())
            .or_else(|| self.tabs.iter().rposition(|w| w.opened && w.mode == mode));
        if let Some(index) = index {
            self.select(index, ctx);
        } else if mode == Mode::Builder {
            let project = self
                .store
                .setting("last_project")
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            self.new_tab(project, ctx);
        } else {
            self.browse_project = None;
            self.new_thinker(ctx);
        }
    }

    fn close_project(&mut self, project: &str, ctx: &egui::Context) {
        self.save_active();
        for w in &mut self.tabs {
            if w.opened && w.mode == Mode::Builder && w.project == project {
                w.opened = false;
                if let Err(error) = self.store.save(w) {
                    w.opened = true;
                    self.notice = Some(error.to_string());
                    break;
                }
            }
        }
        // Retain closed workstreams so their active runs can still finish.
        if !self.tabs[self.active].opened {
            if let Some(project) = self.builder_projects().first() {
                self.open_project(PathBuf::from(project), ctx);
            } else {
                self.switch_mode(Mode::Thinker, ctx);
            }
        }
        self.history_dirty = true;
    }

    fn new_conversation(&mut self, ctx: &egui::Context) {
        if self.tabs[self.active].mode == Mode::Thinker {
            self.new_thinker(ctx);
        } else {
            self.new_tab(PathBuf::from(&self.tabs[self.active].project), ctx);
        }
    }

    fn thinker_directory(&self, project: Option<&str>) -> PathBuf {
        self.projects
            .iter()
            .find(|p| Some(p.id.as_str()) == project)
            .and_then(|p| p.directory.as_ref())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                self.store
                    .path
                    .parent()
                    .unwrap()
                    .join("chatbot/local-context")
            })
    }

    fn new_thinker(&mut self, ctx: &egui::Context) {
        self.save_active();
        let directory = self.thinker_directory(self.browse_project.as_deref());
        let linked = self
            .projects
            .iter()
            .any(|p| Some(&p.id) == self.browse_project.as_ref() && p.directory.is_some());
        if linked && !directory.is_dir() {
            self.notice = Some(
                "The linked local folder is unavailable. Update it in Project settings.".into(),
            );
            return;
        }
        if !linked {
            if let Err(error) = std::fs::create_dir_all(&directory) {
                self.notice = Some(error.to_string());
                return;
            }
        }
        let mut w = Workstream::new(&directory);
        w.mode = Mode::Thinker;
        w.thinker_project = self.browse_project.clone();
        w.title = "New conversation".into();
        w.model = self.store.setting("thinker_model").unwrap_or_default();
        if let Err(error) = self.store.save(&w) {
            self.notice = Some(error.to_string());
            return;
        }
        self.tabs.push(w);
        self.select(self.tabs.len() - 1, ctx);
    }

    fn move_thinker(&mut self, id: &str, project: Option<&str>) {
        if self.busy(id) {
            self.notice = Some("Wait for this conversation's reply before moving it.".into());
            return;
        }
        self.save_active();
        if let Err(error) = self.store.move_conversation(id, project) {
            self.notice = Some(error.to_string());
            return;
        }
        let directory = self
            .thinker_directory(project)
            .to_string_lossy()
            .into_owned();
        if let Some(w) = self.tabs.iter_mut().find(|w| w.id == id) {
            w.thinker_project = project.map(str::to_owned);
            w.session = None;
            w.project = directory;
            if let Err(error) = self.store.save(w) {
                self.notice = Some(error.to_string());
            }
        }
        if self.tabs[self.active].id == id {
            self.browse_project = project.map(str::to_owned);
        }
        self.history_dirty = true;
    }

    fn close_tab(&mut self, index: usize, ctx: &egui::Context) {
        self.save_active();
        self.tabs[index].opened = false;
        if let Err(e) = self.store.save(&self.tabs[index]) {
            self.notice = Some(e.to_string());
            return;
        }
        // Keep running closed workstreams in memory until they finish; only their view closes.
        if self.tabs[index].id == self.tabs[self.active].id {
            if let Some(next) = self
                .tabs
                .iter()
                .position(|w| w.id != self.tabs[index].id && self.visible_workstream(w))
            {
                self.select(next, ctx);
            } else {
                self.new_conversation(ctx);
            }
        }
        self.history_dirty = true;
    }

    fn scan(&mut self, ctx: &egui::Context) {
        let existing = self
            .repository_watch
            .as_ref()
            .is_some_and(|watch| watch.root == Path::new(&self.tabs[self.active].project));
        self.sync_repository_watch(ctx);
        if existing {
            if let Some(watch) = &self.repository_watch {
                self.scanning = true;
                watch.refresh();
            }
        }
    }

    fn sync_repository_watch(&mut self, ctx: &egui::Context) {
        if self.tabs[self.active].mode == Mode::Thinker {
            self.new_file = None;
            self.repository_watch = None;
            self.scanning = false;
            return;
        }
        let root = Path::new(&self.tabs[self.active].project);
        if self
            .repository_watch
            .as_ref()
            .is_some_and(|watch| watch.root == root)
        {
            return;
        }
        self.new_file = None;
        self.files.clear();
        self.tree = FileNode::default();
        self.scanning = true;
        self.repository_watch = Some(crate::repository::RepositoryWatch::new(
            root.to_owned(),
            ctx.clone(),
        ));
    }

    fn process_events(&mut self, ctx: &egui::Context) {
        for _ in 0..32 {
            let Ok(request) = self.delegation_host.try_recv() else {
                break;
            };
            let result = crate::delegation::create(
                &request.args,
                &self.store,
                &request.source,
                &request.turn,
            )
            .and_then(|created| {
                let created: serde_json::Value =
                    serde_json::from_str(&created).map_err(|e| e.to_string())?;
                let id = created["id"].as_str().ok_or("Missing Builder ID")?;
                self.start_pending_builder(id, ctx);
                let w = self.store.load(id).map_err(|e| e.to_string())?;
                Ok(crate::delegation::result(&w))
            });
            let _ = request.reply.send(result);
        }
        self.restart.poll();
        self.sync_repository_watch(ctx);
        if let Some(snapshot) = self
            .repository_watch
            .as_ref()
            .and_then(|watch| watch.take())
        {
            if let Some(editor) = &mut self.editor {
                match editor.refresh() {
                    Ok(true) => self.reload_decision = true,
                    Ok(false) => {
                        if !editor.disk_conflict() {
                            self.reload_decision = false;
                        }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            if self.files != snapshot.files {
                self.tree =
                    FileNode::build(Path::new(&self.tabs[self.active].project), &snapshot.files);
                self.files = snapshot.files;
            }
            self.scanning = false;
            if let Some(error) = snapshot.error {
                self.notice = Some(format!("Repository watcher: {error}"));
            }
        }
        // Bound work per frame so large tool output cannot monopolize input handling.
        for _ in 0..128 {
            let Ok(envelope) = self.rx.try_recv() else {
                break;
            };
            let id = envelope.workstream;
            let Some(index) = self.tabs.iter().position(|w| w.id == id) else {
                continue;
            };
            match envelope.event {
                Event::Session(session) => {
                    self.tabs[index].session = Some(session);
                }
                Event::Activity { id: item, body } => {
                    self.activities
                        .entry(id.clone())
                        .or_default()
                        .insert(item, body);
                    continue;
                }
                Event::Message { item, role, body } => {
                    let body = if role == "Agent" && self.tabs[index].mode == Mode::Thinker {
                        let (answer, mut updates) = crate::thinker::split_memories(&body);
                        let submitted = self
                            .store
                            .submission(&id)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| self.tabs[index].clone());
                        if !updates.is_empty()
                            && submitted
                                .memory_revision
                                .is_some_and(|revision| revision != self.store.memory_revision())
                        {
                            updates.clear();
                            self.notice=Some("Saved memories changed during this reply; its memory suggestions were skipped.".into());
                        }
                        let mut saved = 0;
                        for update in updates {
                            let scope = if update.scope == "global" {
                                ""
                            } else {
                                submitted.thinker_project.as_deref().unwrap_or("")
                            };
                            // Ungrouped project facts stay out of global memory.
                            if update.scope == "project" && scope.is_empty() {
                                continue;
                            }
                            let result = if update.action == "forget" {
                                self.store.forget_key(scope, &update.key)
                            } else {
                                self.store
                                    .remember(scope, &update.key, &update.content, Some(&id))
                            };
                            match result {
                                Ok(()) => saved += 1,
                                Err(error) => {
                                    self.notice = Some(format!("Could not update memory: {error}"))
                                }
                            }
                        }
                        if saved > 0 {
                            if let Ok(message) = self.store.add_message(
                                &id,
                                "Memory",
                                &format!("Updated {saved} saved memory item(s)."),
                                &[],
                            ) {
                                if index == self.active {
                                    self.messages.push(message);
                                }
                            }
                        }
                        answer
                    } else {
                        body
                    };
                    if let Some(items) = self.activities.get_mut(&id) {
                        items.remove(&item);
                    }
                    match self.store.add_message(&id, &role, &body, &[]) {
                        Ok(message) if index == self.active => {
                            self.messages.push(message);
                            if !self.history_context && self.messages.len() > 200 {
                                self.messages.drain(..self.messages.len() - 200);
                            }
                            self.follow_tail = true;
                        }
                        Ok(_) => {}
                        Err(e) => self.notice = Some(format!("Could not save agent output: {e}")),
                    }
                }
                Event::Finished { status } => {
                    if let Some(run) = self.runs.remove(&id) {
                        run.finish();
                    }
                    self.activities.remove(&id);
                    if self.tabs[index].mode == Mode::Builder {
                        if let Err(error) = self.store.persist_builder_result(&id, &status) {
                            self.notice = Some(format!("Could not save Builder result: {error}"));
                        }
                    }
                    self.tabs[index].status = status;
                    if index == self.active {
                        self.scan(ctx);
                    }
                }
            }
            self.tabs[index].updated = chrono::Utc::now().timestamp();
            if let Err(e) = self.store.save(&self.tabs[index]) {
                self.notice = Some(e.to_string());
            }
            self.history_dirty = true;
        }
        if !self.runs.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if self
            .draft_changed
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(400))
        {
            self.save_active();
        }
    }

    fn recover_pending_builders(&mut self, ctx: &egui::Context) {
        let queued = match self.store.queued_builders() {
            Ok(queued) => queued,
            Err(error) => {
                self.notice = Some(format!("Could not read Builder queue: {error}"));
                return;
            }
        };
        for w in queued {
            self.start_pending_builder(&w.id, ctx);
        }
    }

    fn start_pending_builder(&mut self, id: &str, ctx: &egui::Context) {
        let w = match self.store.load(id) {
            Ok(w) => w,
            Err(error) => {
                self.notice = Some(format!("Could not load Builder: {error}"));
                return;
            }
        };
        match self.store.claim_queued_builder(&w.id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.notice = Some(format!("Could not claim Builder startup: {error}"));
                return;
            }
        }
        let index = match self.tabs.iter().position(|tab| tab.id == w.id) {
            Some(index) => index,
            None => {
                self.tabs.push(w.clone());
                self.tabs.len() - 1
            }
        };
        if self.busy(&w.id) {
            return;
        }
        let result = self
            .store
            .submission(&w.id)
            .map_err(|e| e.to_string())
            .and_then(|snapshot| {
                snapshot.ok_or_else(|| {
                    "Missing saved Builder brief. Open Builder to inspect.".to_owned()
                })
            })
            .and_then(|snapshot| self.launch_submitted(index, &snapshot, self.config.clone(), ctx));
        if let Err(error) = result {
            self.tabs[index].status = "Failed".into();
            if let Err(save_error) = self.store.builder_start_result(&w.id, Some(&error)) {
                self.notice = Some(format!(
                    "Could not save Builder startup failure: {save_error}"
                ));
            }
        } else {
            self.tabs[index].draft.clear();
        }
        if let Err(error) = self.store.save(&self.tabs[index]) {
            self.notice = Some(format!("Could not save delegated Builder: {error}"));
        }
    }

    fn submit(&mut self, ctx: &egui::Context) {
        let w = &self.tabs[self.active];
        if w.draft.trim().is_empty() || self.busy(&w.id) {
            return;
        }
        if w.mode == Mode::Builder && !Path::new(&w.project).is_dir() {
            self.notice = Some("Project directory is unavailable.".into());
            return;
        }
        if let Some(missing) = w.attachments.iter().find(|p| !Path::new(p).is_file()) {
            self.notice = Some(format!("Attachment is unavailable: {missing}"));
            return;
        }
        if crate::local::is_local(&w.model) {
            if let Err(error) =
                crate::local::validate(&crate::local::selected_config(&self.config.local, &w.model))
            {
                self.notice = Some(error);
                return;
            }
        }
        self.save_active();
        let w = self.tabs[self.active].clone();
        if let Err(error) = self.store.clear_submission(&w.id) {
            self.notice = Some(format!("Could not update retry state: {error}"));
            return;
        }
        match self
            .store
            .add_message(&w.id, "You", &w.draft, &w.attachments)
        {
            Ok(message) => {
                self.messages.push(message);
            }
            Err(e) => {
                self.notice = Some(e.to_string());
                return;
            }
        };
        if let Err(error) = self.launch_submitted(self.active, &w, self.config.clone(), ctx) {
            self.notice = Some(error);
            return;
        }
        let w = &mut self.tabs[self.active];
        if w.title == "Untitled workstream" || w.title == "New conversation" {
            w.title = w
                .draft
                .lines()
                .find(|s| !s.trim().is_empty())
                .unwrap_or("New task")
                .trim()
                .chars()
                .take(48)
                .collect();
        }
        w.draft.clear();
        w.attachments.clear();
        w.status = "Running".into();
        self.follow_tail = true;
        self.save_active();
    }

    fn stop(&mut self) {
        if let Some(run) = self.runs.get(&self.tabs[self.active].id) {
            run.cancel();
            self.tabs[self.active].status = "Stopping".into();
            self.save_active();
        }
    }

    fn busy(&self, id: &str) -> bool {
        self.runs.contains_key(id)
    }

    fn launch_submitted(
        &mut self,
        index: usize,
        w: &Workstream,
        config: AgentConfig,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let mut submitted = w.clone();
        if submitted.mode == Mode::Thinker {
            submitted.delegation_endpoint = Some(self.delegation_host.endpoint());
            if submitted.invocation_turn.is_none() {
                submitted.invocation_turn = Some(uuid::Uuid::new_v4().to_string());
            }
            let directory = self.thinker_directory(submitted.thinker_project.as_deref());
            if self
                .projects
                .iter()
                .any(|p| Some(&p.id) == submitted.thinker_project.as_ref() && p.directory.is_some())
            {
                if !directory.is_dir() {
                    return Err(
                        "The linked local folder is unavailable. Update it in Project settings."
                            .into(),
                    );
                }
            } else {
                std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            }
            submitted.project = directory
                .canonicalize()
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            crate::thinker::prepare(&mut submitted, &self.store)?;
        }
        let w = &submitted;
        self.store
            .save_submission(w)
            .map_err(|e| format!("Could not save retry state: {e}"))?;
        if w.mode == Mode::Builder {
            if !Path::new(&w.project).is_dir() {
                return Err("Builder workspace is unavailable.".into());
            }
            self.store
                .reserve_builder_start(w)
                .map_err(|e| e.to_string())?;
        }
        let run = runner::start_agent(
            config,
            w,
            self.tx.clone(),
            ctx.clone(),
            Some(&self.restart.request_path()),
        );
        let run = match run {
            Ok(run) => run,
            Err(error) => {
                if w.mode == Mode::Builder {
                    self.store
                        .builder_start_result(&w.id, Some(&error))
                        .map_err(|e| e.to_string())?;
                    self.tabs[index].status = "Failed".into();
                }
                return Err(error);
            }
        };
        if w.mode == Mode::Builder {
            if let Err(error) = self.store.builder_start_result(&w.id, None) {
                self.notice = Some(format!("Could not save Builder startup state: {error}"));
            }
        }
        self.runs.insert(w.id.clone(), run);
        self.tabs[index].model = w.model.clone();
        self.tabs[index].session = w.session.clone();
        self.tabs[index].status = "Running".into();
        if let Err(error) = self.store.save(&self.tabs[index]) {
            self.notice = Some(format!("Could not save running workstream: {error}"));
        }
        Ok(())
    }

    fn retry(&mut self, ctx: &egui::Context) {
        let current = self.tabs[self.active].clone();
        if self.busy(&current.id) {
            return;
        }
        let result = (|| {
            let mut submitted = self
                .store
                .submission(&current.id)
                .map_err(|e| e.to_string())?
                .ok_or("No saved request to retry.")?;
            if runner::provider(&submitted.model) == runner::provider(&current.model) {
                submitted.session = current.session.clone();
            }
            if current.mode == Mode::Thinker {
                submitted.thinker_project = current.thinker_project.clone();
            }
            let mut config = self.config.clone();
            config.local.retry = true;
            self.launch_submitted(self.active, &submitted, config, ctx)
        })();
        match result {
            Ok(()) => {
                self.notice = None;
                self.follow_tail = true;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn attach(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let folder = self.store.path.parent().unwrap().join("attachments");
        if let Err(e) = std::fs::create_dir_all(&folder) {
            self.notice = Some(e.to_string());
            return;
        }
        for path in paths {
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                self.notice = Some("Attach a PNG, JPEG, or WebP image.".into());
                continue;
            }
            if std::fs::metadata(&path)
                .map(|m| m.len() > 20 * 1024 * 1024)
                .unwrap_or(true)
            {
                self.notice =
                    Some("Image unavailable or larger than the prototype's 20 MB limit.".into());
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy();
            let target = folder.join(format!("{}-{name}", uuid::Uuid::new_v4()));
            match std::fs::copy(path, &target) {
                Ok(_) => self.tabs[self.active]
                    .attachments
                    .push(target.to_string_lossy().into_owned()),
                Err(e) => self.notice = Some(format!("Cannot attach image: {e}")),
            }
        }
        self.changed(ctx);
        self.save_active();
    }

    fn open_file(&mut self, path: PathBuf) {
        if self.editor.as_ref().is_some_and(|e| e.path == path) {
            self.show_editor = true;
            return;
        }
        if self.editor.as_ref().is_some_and(FileBuffer::dirty) {
            self.pending_file = Some(path);
            self.file_decision = true;
            return;
        }
        match FileBuffer::open(path) {
            Ok(editor) => {
                self.find_open = false;
                self.find_jump = false;
                self.editor = Some(editor);
                self.show_editor = true;
                self.reload_decision = false;
            }
            Err(e) => self.notice = Some(e),
        }
    }

    fn save_file(&mut self) -> bool {
        if let Some(editor) = &mut self.editor {
            if let Err(e) = editor.save() {
                if editor.externally_changed() {
                    self.reload_decision = true;
                }
                self.notice = Some(e);
                return false;
            }
        }
        true
    }

    fn delete_file(&mut self, path: &Path) {
        if let Err(error) = std::fs::remove_file(path) {
            self.notice = Some(format!("Cannot delete {}: {error}", path.display()));
            return;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.path == path)
        {
            self.editor = None;
            self.show_editor = false;
            self.reload_decision = false;
            self.file_decision = false;
            self.pending_file = None;
        } else if self.pending_file.as_deref() == Some(path) {
            self.pending_file = None;
            self.file_decision = false;
        }
        self.files.retain(|file| file != path);
        self.tree = FileNode::build(Path::new(&self.tabs[self.active].project), &self.files);
    }

    fn create_file(&mut self, directory: &Path, name: &str) -> Result<(), String> {
        if name.trim().is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0'])
        {
            return Err("Enter a filename without directory separators.".into());
        }
        let root = Path::new(&self.tabs[self.active].project);
        let directory = directory.canonicalize().map_err(|e| e.to_string())?;
        if self.tabs[self.active].mode != Mode::Builder || !directory.starts_with(root) {
            return Err("Choose a directory in the current repository.".into());
        }
        let path = directory.join(name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    format!("{name} already exists. Choose another name.")
                } else {
                    format!("Cannot create {name}: {error}")
                }
            })?;
        self.files.push(path.clone());
        self.files.sort_unstable();
        self.files.dedup();
        self.tree = FileNode::build(root, &self.files);
        self.file_query.clear();
        if let Some(watch) = &self.repository_watch {
            watch.refresh();
        }
        self.open_file(path);
        Ok(())
    }

    fn reopen(&mut self, hit: SearchHit, ctx: &egui::Context) {
        let index = if let Some(i) = self.tabs.iter().position(|w| w.id == hit.id) {
            self.tabs[i].opened = true;
            i
        } else {
            match self.store.load(&hit.id) {
                Ok(mut w) => {
                    w.opened = true;
                    self.tabs.push(w);
                    self.tabs.len() - 1
                }
                Err(e) => {
                    self.notice = Some(e.to_string());
                    return;
                }
            }
        };
        self.select(index, ctx);
        self.save_active();
        if let Some(message_id) = hit.message_id {
            match self.store.messages_around(&hit.id, message_id) {
                Ok(messages) => {
                    self.messages = messages;
                    self.jump_to = Some(message_id);
                    self.history_context = true;
                    self.follow_tail = false;
                }
                Err(e) => self.notice = Some(e.to_string()),
            }
        }
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(PAPER)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ctx, |ui| {
                // Reserve the header height before laying out nested rows so all
                // controls share the title's vertical center.
                ui.spacing_mut().interact_size.y = ui
                    .fonts(|fonts| fonts.row_height(&egui::FontId::proportional(26.0)))
                    .max(
                        ui.text_style_height(&egui::TextStyle::Button)
                            + 2.0 * ui.spacing().button_padding.y,
                    )
                    .max(ui.spacing().interact_size.y);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Skyforge").size(26.0).strong());
                    ui.add_space(14.0);
                    ui.label(RichText::new("/ ").color(MUTED));
                    let thinker = self.tabs[self.active].mode == Mode::Thinker;
                    let project = Path::new(&self.tabs[self.active].project);
                    if thinker {
                        let name = self
                            .projects
                            .iter()
                            .find(|p| {
                                Some(&p.id) == self.tabs[self.active].thinker_project.as_ref()
                            })
                            .map(|p| p.name.as_str())
                            .unwrap_or("Thinker");
                        ui.label(RichText::new(name.chars().take(24).collect::<String>()).strong())
                            .on_hover_text(name);
                    } else {
                        let projects = self.builder_projects();
                        let mut selected = None;
                        let mut close = None;
                        egui::ScrollArea::horizontal()
                            .id_salt("project_tabs")
                            .max_width((ui.available_width() - 340.0).max(80.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 14.0;
                                    for path in &projects {
                                        ui.push_id(path, |ui| {
                                            ui.spacing_mut().item_spacing.x = 2.0;
                                            ui.horizontal(|ui| {
                                                let name = Path::new(path)
                                                    .file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy();
                                                if ui
                                                    .selectable_label(
                                                        path == &self.tabs[self.active].project,
                                                        RichText::new(name).strong(),
                                                    )
                                                    .on_hover_text(path)
                                                    .clicked()
                                                {
                                                    selected = Some(PathBuf::from(path));
                                                }
                                                ui.scope(|ui| {
                                                    ui.spacing_mut().button_padding.x = 2.0;
                                                    if ui
                                                        .add_sized(
                                                            [22.0, ui.spacing().interact_size.y],
                                                            egui::Button::new(
                                                                RichText::new("×").size(16.0).color(MUTED),
                                                            )
                                                            .frame(false),
                                                        )
                                                        .on_hover_text("Close project; history is retained and active runs continue")
                                                        .clicked()
                                                    {
                                                        close = Some(path.clone());
                                                    }
                                                });
                                            });
                                        });
                                    }
                                });
                            });
                        if ui.button("Open project…").clicked() {
                            selected = rfd::FileDialog::new().set_directory(project).pick_folder();
                        }
                        if let Some(path) = selected {
                            self.open_project(path, ctx);
                        }
                        if let Some(project) = close {
                            self.close_project(&project, ctx);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Settings").clicked() {
                            self.settings_open = true;
                        }
                        if ui.selectable_label(thinker, "Thinker").clicked() {
                            self.switch_mode(Mode::Thinker, ctx);
                        }
                        if ui.selectable_label(!thinker, "Builder").clicked() {
                            self.switch_mode(Mode::Builder, ctx);
                        }
                    });
                });
            });
        egui::TopBottomPanel::bottom("statusbar")
            .frame(
                egui::Frame::new()
                    .fill(PAPER)
                    .inner_margin(egui::Margin::symmetric(18, 7)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.restart.requested {
                        "Cancel restart"
                    } else {
                        "Restart"
                    };
                    if ui
                        .small_button(label)
                        .on_hover_text(
                            "Save drafts and restart Skyforge after all active runs finish",
                        )
                        .clicked()
                    {
                        self.restart.requested = !self.restart.requested;
                        self.restart_decision = false;
                    }
                    let running = self.runs.len();
                    ui.label(
                        RichText::new(if self.restart.requested {
                            format!("Restart queued · waiting for {running} run(s)")
                        } else if running == 0 {
                            "Ready".into()
                        } else {
                            format!("{running} active run(s)")
                        })
                        .size(16.0)
                        .color(MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("Enter send · Shift+Enter new line")
                                .size(16.0)
                                .color(MUTED),
                        );
                    });
                });
            });
        egui::TopBottomPanel::top("workstream_tabs")
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ctx, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                const LABEL_HEIGHT: f32 = 24.0;
                const TAB_HEIGHT: f32 = LABEL_HEIGHT + 12.0;
                ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), TAB_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let mut select=None; let mut close=None;
                    for (i,w) in self.tabs.iter().enumerate().filter(|(_,w)| self.visible_workstream(w)) {
                        let selected=i==self.active;
                        let tab = egui::Frame::new().fill(if selected { PAPER } else { Color32::TRANSPARENT })
                            .corner_radius(6).inner_margin(egui::Margin::symmetric(10,6)).show(ui, |ui| {
                                ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), LABEL_HEIGHT),
                                    egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                                    let dot=if self.busy(&w.id) { "● " } else { "" };
                                    let shorten = |text: &str| {
                                        let mut chars = text.chars();
                                        let mut label: String = chars.by_ref().take(28).collect();
                                        if chars.next().is_some() { label.push('…'); }
                                        label
                                    };
                                    let mut label = egui::text::LayoutJob::default();
                                    label.append(&format!("{dot}{}", shorten(&w.title)), 0.0,
                                        egui::TextFormat {
                                            font_id: FontId::proportional(16.0),
                                            color: if selected { ON_ACCENT } else { INK },
                                            line_height: Some(20.0),
                                            ..Default::default()
                                        });
                                    let tooltip = format!("{}\n{}", w.title, w.project);
                                    if ui.add(egui::Button::new(label)
                                        .frame(false)
                                        .min_size(egui::vec2(0.0, LABEL_HEIGHT))
                                        .wrap_mode(egui::TextWrapMode::Extend))
                                        .on_hover_text(tooltip).clicked() { select=Some(i); }
                                    if ui.add_sized([22.0, 24.0], egui::Button::new(RichText::new("×").size(16.0).color(MUTED)).frame(false)).on_hover_text("Close tab; history is retained and active runs continue").clicked() { close=Some(i); }
                                });
                            });
                        if selected {
                            let rect = tab.response.rect;
                            ui.painter().line_segment(
                                [egui::pos2(rect.left() + 10.0, rect.bottom() - 1.0),
                                 egui::pos2(rect.right() - 10.0, rect.bottom() - 1.0)],
                                Stroke::new(2.0_f32, LINK),
                            );
                        }
                    }
                    if ui.add_sized([72.0, 28.0], egui::Button::new(RichText::new("+ New").size(16.0).color(INK)).frame(false)).clicked() { self.new_conversation(ctx); }
                    if let Some(i)=select { self.select(i,ctx); }
                    if let Some(i)=close { self.close_tab(i,ctx); }
                });
            });
        });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(250.0)
            .width_range(190.0..=450.0)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(14))
            .show(ctx, |ui| {
                if self.tabs[self.active].mode == Mode::Thinker {
                    self.projects_ui(ui, ctx);
                    return;
                }
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.history, "Repository").clicked() {
                        self.history = false;
                    }
                    if ui.selectable_label(self.history, "History").clicked() {
                        self.history = true;
                        self.history_dirty = true;
                    }
                });
                ui.add_space(12.0);
                if self.history {
                    self.history_ui(ui, ctx);
                } else {
                    self.files_ui(ui);
                }
            });
    }

    fn files_ui(&mut self, ui: &mut egui::Ui) {
        let mut create = None;
        ui.add(
            TextEdit::singleline(&mut self.file_query)
                .hint_text("Find a file…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.0);
        let root = PathBuf::from(&self.tabs[self.active].project);
        ui.horizontal(|ui| {
            ui.label(RichText::new("PROJECT FILES").size(15.0).color(MUTED))
                .context_menu(|ui| new_file_menu(ui, &root, &mut create));
            if ui
                .small_button("+")
                .on_hover_text("New file in project root")
                .clicked()
            {
                create = Some(root.clone());
            }
        });
        if self.scanning {
            ui.label(RichText::new("Reading repository…").color(MUTED));
        }
        let mut open = None;
        let mut delete = None;
        egui::ScrollArea::vertical()
            .id_salt("file_tree")
            .show(ui, |ui| {
                if self.file_query.is_empty() {
                    file_tree(
                        ui,
                        &self.tree,
                        self.editor.as_ref().map(|e| e.path.as_path()),
                        &mut open,
                        &mut delete,
                        &mut create,
                    );
                } else {
                    let query = self.file_query.to_lowercase();
                    for path in self
                        .files
                        .iter()
                        .filter(|p| p.to_string_lossy().to_lowercase().contains(&query))
                        .take(200)
                    {
                        let label = path
                            .strip_prefix(&self.tabs[self.active].project)
                            .unwrap_or(path)
                            .display()
                            .to_string();
                        file_entry(ui, path, label, false, &mut open, &mut delete, &mut create);
                    }
                }
            });
        if let Some(directory) = create {
            self.new_file = Some(NewFile {
                directory,
                name: String::new(),
                error: None,
                focus: true,
            });
        } else if let Some(path) = delete {
            self.delete_file(&path);
        } else if let Some(path) = open {
            self.open_file(path);
        }
    }

    fn projects_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("PROJECTS").size(15.0).color(MUTED));
            if ui.small_button("+ Project").clicked() {
                self.project_editor = Some(ThinkerProject {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: String::new(),
                    instructions: String::new(),
                    directory: None,
                });
            }
        });
        ui.add_space(8.0);
        if ui
            .selectable_label(self.browse_project.is_none(), "No project")
            .clicked()
        {
            self.browse_project = None;
            self.history_dirty = true;
        }
        egui::ScrollArea::vertical()
            .id_salt("thinker_projects")
            .max_height(180.0)
            .show(ui, |ui| {
                for project in &self.projects {
                    if ui
                        .selectable_label(
                            self.browse_project.as_ref() == Some(&project.id),
                            &project.name,
                        )
                        .clicked()
                    {
                        self.browse_project = Some(project.id.clone());
                        self.history_dirty = true;
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("New conversation").clicked() {
                self.new_thinker(ctx);
            }
            if let Some(project) = self
                .projects
                .iter()
                .find(|p| Some(&p.id) == self.browse_project.as_ref())
            {
                if ui.small_button("Project settings").clicked() {
                    self.project_editor = Some(project.clone());
                }
            }
            if ui.small_button("Memories").clicked() {
                self.memories_open = true;
            }
        });
        ui.separator();
        if ui
            .add(
                TextEdit::singleline(&mut self.query)
                    .hint_text("Search conversations…")
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            self.history_dirty = true;
        }
        if self.history_dirty {
            match self.store.search_mode(
                &self.query,
                None,
                0,
                Mode::Thinker,
                Some(self.browse_project.as_deref().unwrap_or("")),
            ) {
                Ok(hits) => self.hits = hits,
                Err(error) => self.notice = Some(error.to_string()),
            }
            self.history_dirty = false;
        }
        let mut open = None;
        let mut move_to = None;
        egui::ScrollArea::vertical()
            .id_salt("thinker_conversations")
            .show(ui, |ui| {
                if self.hits.is_empty() {
                    ui.label(RichText::new("No conversations here yet.").color(MUTED));
                }
                for hit in &self.hits {
                    let response = ui.add(
                        egui::Button::new(&hit.title)
                            .wrap()
                            .frame(false)
                            .selected(hit.id == self.tabs[self.active].id),
                    );
                    if response.clicked() {
                        open = Some(hit.clone());
                    }
                    response.context_menu(|ui| {
                        ui.label("Move conversation to");
                        if ui.button("No project").clicked() {
                            move_to = Some((hit.id.clone(), None));
                            ui.close_menu();
                        }
                        for project in &self.projects {
                            if ui.button(&project.name).clicked() {
                                move_to = Some((hit.id.clone(), Some(project.id.clone())));
                                ui.close_menu();
                            }
                        }
                    });
                }
            });
        if let Some((id, project)) = move_to {
            self.move_thinker(&id, project.as_deref());
        }
        if let Some(hit) = open {
            self.reopen(hit, ctx);
        }
    }

    fn history_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if ui
            .add(
                TextEdit::singleline(&mut self.query)
                    .hint_text("Search entire conversations…")
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            self.history_dirty = true;
        }
        if ui
            .checkbox(&mut self.project_filter, "Current project only")
            .changed()
        {
            self.history_dirty = true;
        }
        egui::ComboBox::from_id_salt("history_date")
            .selected_text(match self.days_filter {
                7 => "Past week",
                30 => "Past month",
                _ => "All dates",
            })
            .show_ui(ui, |ui| {
                for (days, label) in [(0, "All dates"), (7, "Past week"), (30, "Past month")] {
                    if ui
                        .selectable_value(&mut self.days_filter, days, label)
                        .changed()
                    {
                        self.history_dirty = true;
                    }
                }
            });
        if self.history_dirty {
            let project = self
                .project_filter
                .then_some(self.tabs[self.active].project.as_str());
            let since = if self.days_filter == 0 {
                0
            } else {
                chrono::Utc::now().timestamp() - self.days_filter * 86400
            };
            match self.store.search(&self.query, project, since) {
                Ok(hits) => self.hits = hits,
                Err(e) => self.notice = Some(e.to_string()),
            }
            self.history_dirty = false;
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(if self.query.is_empty() {
                "RECENT WORKSTREAMS"
            } else {
                "MATCHING WORKSTREAMS"
            })
            .size(15.0)
            .color(MUTED),
        );
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("history_results")
            .show(ui, |ui| {
                if self.hits.is_empty() {
                    ui.label(RichText::new("No workstreams found.").color(MUTED));
                }
                for hit in &self.hits {
                    ui.add_space(8.0);
                    let response = egui::Frame::new()
                        .fill(PAPER)
                        .corner_radius(6)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 4.0).max(50.0));
                            if ui
                                .add(
                                    egui::Button::new(RichText::new(&hit.title).strong())
                                        .wrap()
                                        .frame(false),
                                )
                                .clicked()
                            {
                                open = Some(hit.clone());
                            }
                            let project = Path::new(&hit.project)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy();
                            ui.label(
                                RichText::new(format!("{project} · {}", date(hit.updated)))
                                    .size(16.0)
                                    .color(MUTED),
                            );
                            if !hit.excerpt.trim().is_empty() {
                                ui.label(
                                    RichText::new(
                                        hit.excerpt.chars().take(150).collect::<String>(),
                                    )
                                    .size(17.0)
                                    .color(MUTED),
                                );
                            }
                        })
                        .response;
                    if response.interact(egui::Sense::click()).clicked() {
                        open = Some(hit.clone());
                    }
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Showing up to 60 results. Refine the search to find older work.",
                    )
                    .size(16.0)
                    .color(MUTED),
                );
            });
        if let Some(hit) = open {
            self.reopen(hit, ctx);
        }
    }

    fn file_find_shortcut(&mut self, ctx: &egui::Context) {
        if self.show_editor
            && self.editor.is_some()
            && self.tabs[self.active].mode == Mode::Builder
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
        {
            self.find_open = true;
            self.find_focus = true;
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("file_find_input")));
        }
    }

    fn file_find_dialog(&mut self, ctx: &egui::Context) {
        if !self.find_open {
            return;
        }
        if !self.show_editor
            || self.tabs[self.active].mode != Mode::Builder
            || self.editor.is_none()
        {
            self.find_open = false;
            return;
        }
        let editor = self.editor.as_mut().unwrap();
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("file_find_dialog")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Find in file");
            // Consume Enter before TextEdit can surrender focus, so repeated
            // presses stay in this dialog and never reach the prompt or source.
            let previous =
                ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter));
            let next = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let mut input = TextEdit::singleline(&mut editor.find)
                .id(egui::Id::new("file_find_input"))
                .hint_text("Search text…")
                .desired_width(f32::INFINITY)
                .show(ui);
            if self.find_focus {
                input.response.request_focus();
                let end = editor.find.chars().count();
                input
                    .state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(end),
                    )));
                input.state.store(ctx, input.response.id);
                self.find_focus = false;
            }
            let changed = input.response.changed();
            if changed {
                editor.find_match = None;
            }
            let mut backwards = previous;
            let mut forwards = next || changed;
            ui.horizontal(|ui| {
                backwards |= ui.button("Previous").on_hover_text("Shift+Enter").clicked();
                forwards |= ui.button("Next").on_hover_text("Enter").clicked();
                close = ui.button("Close").clicked();
            });
            if backwards || forwards {
                editor.find_next(backwards);
                self.find_jump = true;
                input.response.request_focus();
                ctx.request_repaint();
            }
            let matches: Vec<_> = if editor.find.is_empty() {
                vec![]
            } else {
                editor
                    .text
                    .match_indices(&editor.find)
                    .map(|(start, _)| start)
                    .collect()
            };
            let current = editor
                .find_match
                .as_ref()
                .and_then(|range| matches.iter().position(|start| *start == range.start));
            ui.label(match current {
                Some(index) => format!("{} of {} matches", index + 1, matches.len()),
                None if editor.find.is_empty() => {
                    "Enter text to search. Enter: next · Shift+Enter: previous".into()
                }
                None => format!("{} matches", matches.len()),
            });
        });
        if close || response.should_close() {
            self.find_open = false;
            if let Some(id) = self.source_editor_id {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
    }

    fn editor_ui(&mut self, ctx: &egui::Context) {
        if !self.show_editor || self.tabs[self.active].mode == Mode::Thinker {
            return;
        }
        egui::SidePanel::right("file_editor")
            .resizable(true)
            .default_width(480.0)
            .width_range(300.0..=850.0)
            .frame(egui::Frame::new().fill(PAPER).inner_margin(16))
            .show(ctx, |ui| {
                let Some(editor) = &mut self.editor else {
                    ui.heading("Repository editor");
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Choose a file in the repository to read or edit it here.")
                            .color(MUTED),
                    );
                    return;
                };
                let mut save = false;
                ui.horizontal(|ui| {
                    let title = editor
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    ui.label(
                        RichText::new(format!("{title}{}", if editor.dirty() { " •" } else { "" }))
                            .strong(),
                    );
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.small_button("×").clicked() {
                        self.show_editor = false;
                        self.find_open = false;
                    }
                });
                ui.label(
                    RichText::new(editor.path.display().to_string())
                        .size(15.0)
                        .color(MUTED),
                );
                ui.separator();
                egui::ScrollArea::both()
                    .id_salt("source_scroll")
                    .show(ui, |ui| {
                        let ext = editor
                            .path
                            .extension()
                            .and_then(|s| s.to_str())
                            .unwrap_or("txt")
                            .to_owned();
                        let theme = egui_extras::syntax_highlighting::CodeTheme::dark(19.0);
                        let find = editor.find.clone();
                        let active_match = editor.find_match.clone();
                        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
                            let mut job = egui_extras::syntax_highlighting::highlight(
                                ui.ctx(),
                                ui.style(),
                                &theme,
                                text,
                                &ext,
                            );
                            if !find.is_empty() {
                                highlight_matches(&mut job, text, &find, active_match.as_ref());
                            }
                            job.wrap.max_width = wrap_width;
                            ui.fonts(|fonts| fonts.layout_job(job))
                        };
                        let id = ui.make_persistent_id(("source_editor", &editor.path));
                        self.source_editor_id = Some(id);
                        let jump = self.find_jump.then(|| editor.find_match.clone()).flatten();
                        self.find_jump = false;
                        let selection = jump
                            .as_ref()
                            .filter(|range| range.end <= editor.text.len())
                            .map(|range| {
                                egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(
                                        editor.text[..range.start].chars().count(),
                                    ),
                                    egui::text::CCursor::new(
                                        editor.text[..range.end].chars().count(),
                                    ),
                                )
                            });
                        if let Some(selection) = selection {
                            let mut state =
                                egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
                            state.cursor.set_char_range(Some(selection));
                            state.store(ctx, id);
                        }
                        let output = TextEdit::multiline(&mut editor.text)
                            .id(id)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(30)
                            .code_editor()
                            .layouter(&mut layouter)
                            .show(ui);
                        if let Some(selection) = selection {
                            let rect = output
                                .galley
                                .pos_from_ccursor(selection.primary)
                                .union(output.galley.pos_from_ccursor(selection.secondary))
                                .translate(output.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                        if output.response.changed() {
                            editor.find_match = None;
                        }
                    });
                if save {
                    self.save_file();
                }
            });
    }

    fn workspace(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAPER).inner_margin(22))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let w = &mut self.tabs[self.active];
                    let response = ui.add(
                        TextEdit::singleline(&mut w.title)
                            .font(FontId::proportional(26.0))
                            .frame(false)
                            .desired_width((ui.available_width() - 120.0).max(100.0)),
                    );
                    if response.changed() {
                        self.changed(ctx);
                    }
                    let status = &self.tabs[self.active].status;
                    ui.label(RichText::new(status).size(17.0).color(
                        if matches!(status.as_str(), "Running") {
                            LINK
                        } else {
                            MUTED
                        },
                    ));
                });
                let shared = self
                    .tabs
                    .iter()
                    .filter(|w| {
                        self.runs.contains_key(&w.id) && w.project == self.tabs[self.active].project
                    })
                    .count();
                if shared > 1 && self.tabs[self.active].mode == Mode::Builder {
                    ui.label(
                        RichText::new(
                            "Multiple runs share this repository. Agents can edit the same files.",
                        )
                        .size(17.0)
                        .color(Color32::from_rgb(226, 192, 141)),
                    );
                }
                ui.add_space(8.0);
                if self.tabs[self.active].mode == Mode::Thinker {
                    let previous = self.tabs[self.active].thinker_project.clone();
                    let mut destination = previous.clone();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Chatbot · read-only").size(16.0).color(MUTED));
                        egui::ComboBox::from_id_salt("move_conversation")
                            .selected_text(
                                self.projects
                                    .iter()
                                    .find(|p| Some(&p.id) == previous.as_ref())
                                    .map(|p| p.name.as_str())
                                    .unwrap_or("No project"),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut destination, None, "No project");
                                for project in &self.projects {
                                    ui.selectable_value(
                                        &mut destination,
                                        Some(project.id.clone()),
                                        &project.name,
                                    );
                                }
                            });
                        if ui.small_button("Memories").clicked() {
                            self.memories_open = true;
                        }
                    });
                    if destination != previous {
                        let id = self.tabs[self.active].id.clone();
                        self.move_thinker(&id, destination.as_deref());
                    }
                }
                if let Some(notice) = self.notice.clone() {
                    egui::Frame::new()
                        .fill(Color32::from_rgb(57, 44, 24))
                        .corner_radius(6)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(notice).color(Color32::from_rgb(226, 192, 141)),
                                );
                                if ui.small_button("Dismiss").clicked() {
                                    self.notice = None;
                                }
                            });
                        });
                    ui.add_space(8.0);
                }
                let max_height = (ui.available_height() - 90.0).max(180.0);
                egui::TopBottomPanel::bottom("composer_panel")
                    .resizable(true)
                    .default_height(290.0)
                    .min_height(190.0)
                    .max_height(max_height)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 12)))
                    .show_inside(ui, |ui| {
                        // PanelState stores the content rect, so fill the selected height
                        // instead of letting the content shrink the panel every frame.
                        ui.set_min_height(ui.available_height());
                        self.composer(ui, ctx);
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new())
                    .show_inside(ui, |ui| {
                        self.conversation(ui);
                    });
            });
    }

    fn composer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let thinker = self.tabs[self.active].mode == Mode::Thinker;
        let mut send_requested = false;
        let attachments = self.tabs[self.active].attachments.clone();
        if !attachments.is_empty() {
            let mut remove = None;
            egui::ScrollArea::horizontal()
                .id_salt("attachments")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, path) in attachments.iter().enumerate() {
                            egui::Frame::new()
                                .stroke(Stroke::new(1.0_f32, LINE))
                                .corner_radius(5)
                                .inner_margin(5)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::Image::new(format!("file://{path}"))
                                                .max_size(egui::vec2(52.0, 40.0)),
                                        );
                                        let name = Path::new(path)
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy();
                                        ui.label(
                                            RichText::new(
                                                name.split_once('-')
                                                    .map(|(_, n)| n)
                                                    .unwrap_or(&name)
                                                    .chars()
                                                    .rev()
                                                    .take(22)
                                                    .collect::<String>()
                                                    .chars()
                                                    .rev()
                                                    .collect::<String>(),
                                            )
                                            .size(16.0),
                                        );
                                        if ui.small_button("×").clicked() {
                                            remove = Some(i);
                                        }
                                    });
                                });
                        }
                    });
                });
            if let Some(i) = remove {
                self.tabs[self.active].attachments.remove(i);
                self.changed(ctx);
            }
        }
        let height = (ui.available_height() - 72.0).max(60.0);
        let response = egui::Frame::new()
            .fill(INPUT)
            .stroke(Stroke::new(1.0_f32, LINE))
            .corner_radius(9)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_min_height(height - 24.0);
                ui.set_max_height(height - 24.0);
                egui::ScrollArea::vertical()
                    .id_salt(("prompt_scroll", &self.tabs[self.active].id))
                    .show(ui, |ui| {
                        let id = ui.make_persistent_id(("prompt", &self.tabs[self.active].id));
                        send_requested = take_prompt_send(ctx, id);
                        let mut layouter = |ui: &egui::Ui, text: &str, width: f32| {
                            ui.fonts(|fonts| fonts.layout_job(markdown_job(text, width, INK)))
                        };
                        let response = ui.add(
                            TextEdit::multiline(&mut self.tabs[self.active].draft)
                                .id(id)
                                .return_key(egui::KeyboardShortcut::new(
                                    egui::Modifiers::SHIFT,
                                    egui::Key::Enter,
                                ))
                                .font(FontId::proportional(PROMPT_SIZE))
                                .desired_width(f32::INFINITY)
                                .desired_rows(1)
                                .min_size(egui::vec2(0.0, (height - 24.0).max(45.0)))
                                .frame(false)
                                .layouter(&mut layouter),
                        );
                        if self.focus_prompt {
                            response.request_focus();
                            self.focus_prompt = false;
                        }
                        response.changed()
                    })
                    .inner
            });
        if response.inner {
            self.changed(ctx);
        }
        if send_requested {
            self.submit(ctx);
        }
        ui.add_space(8.0);
        let running = self.busy(&self.tabs[self.active].id);
        // Inset the toolbar so expanded hover backgrounds stay inside the panel.
        egui::Frame::new().inner_margin(4).show(ui, |ui| {
            // Reserve the full button height before placing any controls so nested
            // enabled scopes share the same vertical center as the Image button.
            ui.spacing_mut().interact_size.y = (ui.text_style_height(&egui::TextStyle::Button)
                + 2.0 * ui.spacing().button_padding.y)
                .max(30.0)
                .max(ui.spacing().interact_size.y);
            ui.horizontal(|ui| {
                if ui.button("+ Image").clicked() {
                    if let Some(paths) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                        .pick_files()
                    {
                        self.attach(paths, ctx);
                    }
                }
                let previous = self.tabs[self.active].model.clone();
                let mut selection =
                    crate::local::canonical_selection(&self.config.local, &previous);
                let selected = self
                    .model_choices
                    .iter()
                    .find(|(id, _)| id == &selection)
                    .map(|(_, label)| label.as_str())
                    .unwrap_or(if selection == crate::local::MODEL_ID {
                        "Local · OpenAI compatible model"
                    } else {
                        &selection
                    })
                    .to_owned();
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("agent_model")
                        .selected_text(selected)
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in &self.model_choices {
                                ui.selectable_value(&mut selection, id.clone(), label);
                            }
                            if !self.model_choices.iter().any(|(id, _)| id == &selection) {
                                let current = selection.clone();
                                ui.selectable_value(&mut selection, current.clone(), &current);
                            }
                        });
                });
                self.tabs[self.active].model = selection;
                if previous != self.tabs[self.active].model {
                    if thinker {
                        let _ = self
                            .store
                            .set_setting("thinker_model", &self.tabs[self.active].model);
                    }
                    if runner::provider(&previous)
                        != runner::provider(&self.tabs[self.active].model)
                    {
                        self.tabs[self.active].session = None;
                    }
                    self.changed(ctx);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("Stop run").clicked() {
                            self.stop();
                        }
                    } else if ui
                        .add_enabled(
                            !self.tabs[self.active].draft.trim().is_empty(),
                            egui::Button::new(RichText::new("Send  ↗").color(ON_ACCENT))
                                .fill(ACCENT)
                                .min_size(egui::vec2(90.0, 30.0)),
                        )
                        .clicked()
                    {
                        self.submit(ctx);
                    }
                });
            });
        });
    }

    fn conversation(&mut self, ui: &mut egui::Ui) {
        let id = self.tabs[self.active].id.clone();
        let can_retry = !self.busy(&id)
            && matches!(
                self.tabs[self.active].status.as_str(),
                "Failed" | "Interrupted" | "Stopped"
            )
            && self.store.submission(&id).ok().flatten().is_some();
        let mut retry = false;
        let mut load_older = false;
        let mut latest = false;
        let mut jumped = false;
        let links = match self.store.linked_builders(&id) {
            Ok(links) => links,
            Err(error) => {
                self.notice = Some(format!("Could not load linked Builders: {error}"));
                vec![]
            }
        };
        let mut open_builder = None;
        egui::ScrollArea::vertical()
            .id_salt(("conversation", &id))
            .stick_to_bottom(self.follow_tail)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                ui.spacing_mut().item_spacing.y = 4.0;
                if self.messages.len() >= 100 && ui.small_button("Load earlier messages").clicked()
                {
                    load_older = true;
                }
                if self.history_context && ui.small_button("Return to latest messages").clicked() {
                    latest = true;
                }
                if self.messages.is_empty() {
                    ui.label(RichText::new("No messages yet.").color(MUTED));
                }
                for message in &self.messages {
                    // Hide wire diagnostics retained by older releases.
                    if matches!(message.role.as_str(), "Local request" | "Local response") {
                        continue;
                    }
                    ui.add_space(4.0);
                    let is_you = message.role == "You";
                    let matched = self.jump_to == Some(message.id);
                    let response = egui::Frame::new()
                        .fill(if matched {
                            Color32::from_rgb(38, 71, 120)
                        } else if is_you {
                            SIDEBAR
                        } else {
                            PAPER
                        })
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(14, 6))
                        .show(ui, |ui| {
                            if !activity_output(message) && message.role != "Usage" {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(message.role.to_uppercase())
                                        .size(15.0)
                                        .strong()
                                        .color(MUTED),
                                );
                                ui.label(
                                    RichText::new(time(message.created)).size(15.0).color(MUTED),
                                );
                            });
                            ui.add_space(2.0);
                            }
                            if message.role == "Usage" {
                                ui.label(RichText::new(&message.body).small().color(MUTED));
                            } else if activity_output(message) {
                                let body = if message.role == "Files" {
                                    message.body.strip_prefix("File changes\n").unwrap_or(&message.body)
                                } else { &message.body };
                                wrapped_activity(ui, ui.make_persistent_id(message.id), activity_summary(message, body), body, message.role == "Shell");
                            } else {
                                crate::markdown::show(ui, &mut self.markdown, &message.body);
                            }
                            for path in &message.attachments {
                                ui.add(
                                    egui::Image::new(format!("file://{path}"))
                                        .max_size(egui::vec2(250.0, 140.0)),
                                );
                            }
                        })
                        .response;
                    if matched {
                        response.scroll_to_me(Some(egui::Align::Center));
                        jumped = true;
                    }
                }
                for link in &links {
                    egui::Frame::new().fill(PAPER).corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(14, 6)).show(ui, |ui| {
                            if linked_builder_card(ui, link) { open_builder = Some(link.id.clone()); }
                        });
                }
                if let Some(items) = self.activities.get(&id) {
                    for (item, body) in items {
                        ui.add_space(4.0);
                        wrapped_activity(ui, ui.make_persistent_id((&id, item)), format!(
                            "In progress · {}",
                            body.lines()
                                .next()
                                .unwrap_or("Working")
                                .chars()
                                .take(80)
                                .collect::<String>()
                        ), body, false);
                    }
                }
                if self.busy(&id) {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("● Agent / tool running…")
                            .color(LINK)
                            .size(17.0),
                    );
                }
                if can_retry {
                    ui.add_space(12.0);
                    retry = ui.button("Retry request").on_hover_text("Retry the original request with its selected model. Completed tools remain in the session.").clicked();
                }
                ui.add_space(16.0);
            });
        if let Some(builder) = open_builder {
            let result = self.store.load(&builder);
            match result {
                Ok(mut w) => {
                    w.opened = true;
                    if let Err(error) = self.store.save(&w) {
                        self.notice = Some(error.to_string());
                    } else {
                        let index = match self.tabs.iter().position(|tab| tab.id == builder) {
                            Some(index) => {
                                self.tabs[index].opened = true;
                                index
                            }
                            None => {
                                self.tabs.push(w);
                                self.tabs.len() - 1
                            }
                        };
                        self.select(index, ui.ctx());
                        return;
                    }
                }
                Err(error) => self.notice = Some(error.to_string()),
            }
        }
        if retry {
            self.retry(ui.ctx());
        }
        // Only scroll to a search hit once, so subsequent manual scrolling works.
        if jumped {
            self.jump_to = None;
        }
        if load_older {
            let before = self.messages.first().map(|m| m.id);
            match self.store.messages(&id, before) {
                Ok(mut older) => {
                    older.append(&mut self.messages);
                    self.messages = older;
                    self.history_context = true;
                    self.follow_tail = false;
                }
                Err(e) => self.notice = Some(e.to_string()),
            }
        }
        if latest {
            if let Ok(messages) = self.store.messages(&id, None) {
                self.messages = messages;
                self.history_context = false;
                self.jump_to = None;
                self.follow_tail = true;
            }
        }
    }

    fn refresh_local_models(&mut self, ctx: &egui::Context) {
        self.model_discovery_due = None;
        let config = self.config.local.clone();
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        self.model_discovery = Some(rx);
        self.model_discovery_status = "Loading server models…".into();
        self.config.local.available_models.clear();
        self.model_choices = runner::model_choices(&self.config.local);
        std::thread::spawn(move || {
            let result = crate::local::discover_models(&config).map_err(|error| {
                if config.api_key.is_empty() {
                    error
                } else {
                    error.replace(&config.api_key, "[redacted]")
                }
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn settings_values(&self) -> [(&'static str, String); 11] {
        [
            ("codex_binary", self.config.binary.clone()),
            ("claude_binary", self.config.claude_binary.clone()),
            ("local_host", self.config.local.host.clone()),
            ("local_api_key", self.config.local.api_key.clone()),
            ("local_model", self.config.local.model.clone()),
            (
                "local_context_windows",
                serde_json::to_string(&self.config.local.context_windows).unwrap_or_default(),
            ),
            ("local_output_limits", serde_json::to_string(&self.config.local.output_limits).unwrap_or_default()),
            ("local_model_capabilities", serde_json::to_string(&self.config.local.model_capabilities).unwrap_or_default()),
            ("local_reasoning_replay", serde_json::to_string(&self.config.local.reasoning_replay).unwrap_or_default()),
            ("web_search", self.config.search.to_string()),
            ("writable", self.config.writable.to_string()),
        ]
    }

    fn persist_settings(&mut self) {
        self.model_choices = runner::model_choices(&self.config.local);
        for (key, value) in self.settings_values() {
            if let Err(e) = self.store.set_setting(key, &value) {
                self.notice = Some(e.to_string());
            }
        }
    }

    fn advance_restart(&mut self, ctx: &egui::Context) {
        if !self.restart.requested
            || !self.runs.is_empty()
            || self.restart_decision
            || self.allow_quit
        {
            return;
        }
        if self.editor.as_ref().is_some_and(FileBuffer::dirty) {
            self.restart_decision = true;
        } else {
            self.commit_restart(ctx);
        }
    }

    fn commit_restart(&mut self, ctx: &egui::Context) {
        // A new run may have started while the unsaved-file dialog was open.
        if !self.runs.is_empty() {
            self.restart_decision = false;
            return;
        }
        let result = (|| -> Result<(), String> {
            crate::restart::executable()?;
            for tab in &self.tabs {
                self.store.save(tab).map_err(|e| e.to_string())?;
            }
            let active = &self.tabs[self.active];
            if active.mode == Mode::Builder {
                self.store
                    .set_setting("last_project", &active.project)
                    .map_err(|e| e.to_string())?;
            }
            self.store
                .set_setting("active_workstream", &active.id)
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.restart.commit();
                self.allow_quit = true;
                self.restart_decision = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => {
                self.notice = Some(format!("Could not restart Skyforge: {error}"));
                self.restart.requested = false;
                self.restart_decision = false;
            }
        }
    }

    fn thinker_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut project) = self.project_editor.take() {
            let mut open = true;
            let mut save = false;
            let mut cancel = false;
            egui::Window::new("Thinker project").id(egui::Id::new("thinker_project_editor"))
                .open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
                    ui.label("Name");
                    ui.add(TextEdit::singleline(&mut project.name).hint_text("Project name").desired_width(f32::INFINITY));
                    ui.label("Project guidance");
                    ui.add(TextEdit::multiline(&mut project.instructions).hint_text("Goals, background, and how you want Thinker to help…").desired_rows(5).desired_width(f32::INFINITY));
                    ui.add_space(8.0);
                    ui.label("Optional local files");
                    ui.label(RichText::new("Link a folder for local documents and Builder delegation. Thinker reads it; delegated Builders use normal Builder permissions.").color(MUTED));
                    if let Some(directory)=&project.directory { ui.label(directory); }
                    ui.horizontal(|ui| {
                        if ui.button("Link folder…").clicked() {
                            if let Some(path)=rfd::FileDialog::new().pick_folder() { project.directory=Some(path.to_string_lossy().into_owned()); }
                        }
                        if project.directory.is_some() && ui.button("Remove folder").clicked() { project.directory=None; }
                    });
                    ui.horizontal(|ui| {
                        save=ui.add_enabled(!project.name.trim().is_empty(),egui::Button::new("Save project")).clicked();
                        cancel=ui.button("Cancel").clicked();
                    });
                });
            if save {
                match self.store.save_project(&project) {
                    Ok(()) => {
                        match self.store.projects() {
                            Ok(projects) => self.projects = projects,
                            Err(error) => self.notice = Some(error.to_string()),
                        }
                        self.browse_project = Some(project.id);
                        self.history_dirty = true;
                    }
                    Err(error) => {
                        self.notice = Some(error.to_string());
                        self.project_editor = Some(project);
                    }
                }
            } else if open && !cancel {
                self.project_editor = Some(project);
            }
        }
        if self.memories_open {
            let mut open = true;
            let project = self.browse_project.clone();
            let mut memories = match self.store.memories("") {
                Ok(memories) => memories,
                Err(error) => {
                    self.notice = Some(error.to_string());
                    vec![]
                }
            };
            if let Some(project) = &project {
                match self.store.memories(project) {
                    Ok(items) => memories.extend(items),
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
            let mut source = None;
            let mut forget = None;
            let mut save = false;
            egui::Window::new("Thinker memories").open(&mut open).default_width(600.0).show(ctx, |ui| {
                ui.label("Shared memories follow Thinker across conversations. Project memories apply only within their project.");
                egui::ScrollArea::vertical().id_salt("memory_list").max_height(260.0).show(ui, |ui| {
                    if memories.is_empty() { ui.label(RichText::new("No saved memories yet.").color(MUTED)); }
                    for memory in &memories {
                        ui.push_id(memory.id, |ui| {
                            ui.separator();
                            ui.label(RichText::new(format!("{} · {}",if memory.scope.is_empty() { "Shared" } else { "Project" },memory.key)).strong());
                            ui.label(&memory.content);
                            ui.horizontal(|ui| {
                                if ui.small_button("Edit").clicked() {
                                    self.memory_edit_id=Some(memory.id); self.memory_key=memory.key.clone();
                                    self.memory_content=memory.content.clone(); self.memory_scope=memory.scope.clone();
                                }
                                if ui.small_button("Forget").clicked() { forget=Some(memory.id); }
                                if let Some(id)=&memory.source { if ui.small_button("Source conversation").clicked() { source=Some(id.clone()); } }
                            });
                        });
                    }
                });
                ui.separator();
                ui.label(if self.memory_edit_id.is_some() { "Edit memory" } else { "Add memory" });
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.memory_scope,String::new(),"Shared");
                    if let Some(project)=&project { ui.selectable_value(&mut self.memory_scope,project.clone(),"This project"); }
                });
                ui.add(TextEdit::singleline(&mut self.memory_key).hint_text("Key, e.g. preferred-answer-style").desired_width(f32::INFINITY));
                ui.add(TextEdit::multiline(&mut self.memory_content).hint_text("What should Thinker remember?").desired_rows(3).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    save=ui.add_enabled(!self.memory_key.trim().is_empty() && !self.memory_content.trim().is_empty(),egui::Button::new("Save memory")).clicked();
                    if self.memory_edit_id.is_some() && ui.button("Cancel edit").clicked() { self.memory_edit_id=None; self.memory_key.clear(); self.memory_content.clear(); }
                });
            });
            if save {
                match self.store.remember(
                    &self.memory_scope,
                    &self.memory_key,
                    &self.memory_content,
                    None,
                ) {
                    Ok(()) => {
                        if let Some(old) =
                            memories.iter().find(|m| Some(m.id) == self.memory_edit_id)
                        {
                            if old.scope != self.memory_scope
                                || old.key != self.memory_key.trim().to_lowercase()
                            {
                                if let Err(error) = self.store.forget(old.id) {
                                    self.notice = Some(error.to_string());
                                }
                            }
                        }
                        self.memory_edit_id = None;
                        self.memory_key.clear();
                        self.memory_content.clear();
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
            if let Some(id) = forget {
                if let Err(error) = self.store.forget(id) {
                    self.notice = Some(error.to_string());
                }
                if self.memory_edit_id == Some(id) {
                    self.memory_edit_id = None;
                    self.memory_key.clear();
                    self.memory_content.clear();
                }
            }
            self.memories_open = open;
            if let Some(id) = source {
                match self.store.load(&id) {
                    Ok(w) => self.reopen(
                        SearchHit {
                            id: w.id,
                            title: w.title,
                            project: w.project,
                            updated: w.updated,
                            excerpt: String::new(),
                            message_id: None,
                        },
                        ctx,
                    ),
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.file_find_dialog(ctx);
        self.thinker_dialogs(ctx);
        if self.restart_decision {
            egui::Window::new("Before restarting Skyforge").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("The file editor has unsaved changes. Prompt drafts and history will be kept.");
                ui.horizontal(|ui| {
                    if ui.button("Save file and restart").clicked() && self.save_file() { self.commit_restart(ctx); }
                    if ui.button("Discard file edits and restart").clicked() { self.commit_restart(ctx); }
                    if ui.button("Keep working").clicked() { self.restart.requested = false; self.restart_decision = false; }
                });
            });
        }
        if self.settings_open {
            let mut open = true;
            let settings = self.settings_values();
            let connection = (
                self.config.local.host.clone(),
                self.config.local.api_key.clone(),
            );
            egui::Window::new("Agent settings").open(&mut open).collapsible(false).resizable(true).default_width(580.0).show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height((ctx.screen_rect().height()-140.0).max(250.0)).show(ui, |ui| {
                ui.label(RichText::new("Changes are saved automatically.").color(MUTED));
                ui.label("Codex executable");
                ui.add(TextEdit::singleline(&mut self.config.binary).desired_width(f32::INFINITY));
                if ui.button("Choose executable…").clicked() {
                    if let Some(path)=rfd::FileDialog::new().pick_file() { self.config.binary=path.to_string_lossy().into_owned(); }
                }
                ui.add_space(10.0);
                ui.label("Claude Code executable");
                ui.add(TextEdit::singleline(&mut self.config.claude_binary).desired_width(f32::INFINITY));
                ui.add_space(10.0);
                ui.separator();
                ui.label("OpenAI compatible model");
                ui.label("Host");
                ui.add(TextEdit::singleline(&mut self.config.local.host).desired_width(f32::INFINITY));
                ui.label("API key");
                ui.add(TextEdit::singleline(&mut self.config.local.api_key).password(true).desired_width(f32::INFINITY));
                ui.label("Model name (exact served ID)");
                ui.add(TextEdit::singleline(&mut self.config.local.model).desired_width(f32::INFINITY));
                if ui.add_enabled(self.model_discovery.is_none(), egui::Button::new("Refresh server models")).clicked() { self.refresh_local_models(ctx); }
                ui.label(&self.model_discovery_status);
                if !self.config.local.available_models.is_empty() {
                    egui::ComboBox::from_id_salt("local_default_model").selected_text(if self.config.local.model.is_empty() { "Choose a server model" } else { &self.config.local.model }).show_ui(ui, |ui| {
                        for model in &self.config.local.available_models {
                            ui.selectable_value(&mut self.config.local.model, model.clone(), model);
                        }
                    });
                }
                if !self.config.local.model.trim().is_empty() {
                    let model = self.config.local.model.trim().to_owned();
                    let window = self.config.local.context_windows.entry(model.clone()).or_insert(32_768);
                    ui.horizontal(|ui| {
                        ui.label("Context size (tokens)");
                        ui.add(egui::DragValue::new(window).range(8_192..=2_000_000).speed(1024));
                    });
                    ui.label(RichText::new("Set this to the server's configured context window. Older history is compacted automatically near the limit.").color(MUTED));
                    let maximum = window.saturating_sub(512 + 1024);
                    let mut automatic = !self.config.local.output_limits.contains_key(&model);
                    if ui.checkbox(&mut automatic, "Use automatic output limit").changed() {
                        if automatic {
                            self.config.local.output_limits.remove(&model);
                        } else {
                            let output = self.config.local.output_limit();
                            self.config.local.output_limits.insert(model.clone(), output);
                        }
                    }
                    let mut output = self.config.local.output_limits.get(&model).copied().unwrap_or_else(|| self.config.local.output_limit()).clamp(1, maximum);
                    ui.horizontal(|ui| {
                        ui.label("Maximum output (tokens)");
                        if ui.add_enabled(!automatic, egui::DragValue::new(&mut output).range(1..=maximum).speed(256)).changed() {
                            self.config.local.output_limits.insert(model.clone(), output);
                        }
                    });
                    ui.label(RichText::new(format!("Effective output limit: {} tokens. The same space is reserved for each answer, including thinking where the server counts it as output.", self.config.local.output_limit())).color(MUTED));
                    let resolved = self.config.local.replay_policy();
                    let key = self.config.local.replay_key();
                    let policy = self.config.local.reasoning_replay.entry(key).or_default();
                    egui::ComboBox::from_id_salt("local_reasoning_replay").selected_text(format!("Reasoning replay: {}", policy.label())).show_ui(ui, |ui| {
                        for choice in [crate::local::ReasoningReplay::Auto, crate::local::ReasoningReplay::Omit, crate::local::ReasoningReplay::CurrentTurn, crate::local::ReasoningReplay::PreserveHistory] {
                            ui.selectable_value(policy, choice, choice.label());
                        }
                    });
                    ui.label(RichText::new(format!("Effective policy: {}. Reasoning stays hidden. Overrides apply to this host and model.", resolved.label())).color(MUTED));
                }
                ui.separator();
                ui.checkbox(&mut self.config.search,"Enable live web research");
                ui.checkbox(&mut self.config.writable,"Allow Builder full filesystem access");
                ui.label(RichText::new("Thinker uses the Chatbot profile: read-only context tools, conversation and memories. It can delegate execution to a separate Builder with normal Builder permissions.").color(MUTED));
                ui.collapsing("Thinker system prompt", |ui| { ui.label(crate::thinker::SYSTEM_PROMPT); });
                ui.add_space(8.0);
                ui.label(RichText::new("Uses your existing Codex or Claude Code login. Builder runs have full filesystem access with your account permissions. Disabling access limits runs to read-only tools.").color(MUTED));
                ui.add_space(8.0); ui.label(RichText::new(format!("History: {}",self.store.path.display())).size(16.0).color(MUTED));
                });
            });
            if settings != self.settings_values() {
                self.persist_settings();
            }
            if connection
                != (
                    self.config.local.host.clone(),
                    self.config.local.api_key.clone(),
                )
            {
                // Save each edit immediately, but wait for typing to pause before querying the server.
                self.model_discovery = None;
                self.config.local.available_models.clear();
                self.model_choices = runner::model_choices(&self.config.local);
                self.model_discovery_status = "Waiting to refresh server models…".into();
                self.model_discovery_due = Some(Instant::now() + Duration::from_millis(500));
                ctx.request_repaint_after(Duration::from_millis(500));
            }
            if !open {
                self.settings_open = false;
            }
        }
        if let Some(mut draft) = self.new_file.take() {
            let mut open = true;
            let mut create = false;
            let mut cancel = false;
            egui::Window::new("New file")
                .id(egui::Id::new("new_repository_file"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("Create in {}", draft.directory.display()));
                    let input = ui.add(TextEdit::singleline(&mut draft.name).hint_text("Filename"));
                    if draft.focus {
                        input.request_focus();
                        draft.focus = false;
                    }
                    create = (input.has_focus() || input.lost_focus())
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                    cancel =
                        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                    if let Some(error) = &draft.error {
                        ui.label(RichText::new(error).color(Color32::RED));
                    }
                    ui.horizontal(|ui| {
                        create |= ui.button("Create").clicked();
                        cancel |= ui.button("Cancel").clicked();
                    });
                });
            if open && !cancel {
                if create {
                    if let Err(error) = self.create_file(&draft.directory, &draft.name) {
                        draft.error = Some(error);
                        draft.focus = true;
                        self.new_file = Some(draft);
                    }
                } else {
                    self.new_file = Some(draft);
                }
            }
        }
        if self.file_decision {
            egui::Window::new("Keep your file edits?")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("The current file has unsaved changes.");
                    ui.horizontal(|ui| {
                        if ui.button("Save and open").clicked() && self.save_file() {
                            self.file_decision = false;
                            if let Some(path) = self.pending_file.take() {
                                self.open_file(path);
                            }
                        }
                        if ui.button("Discard and open").clicked() {
                            self.file_decision = false;
                            self.editor = None;
                            if let Some(path) = self.pending_file.take() {
                                self.open_file(path);
                            }
                        }
                        if ui.button("Keep editing").clicked() {
                            self.file_decision = false;
                            self.pending_file = None;
                        }
                    });
                });
        }
        if self.reload_decision {
            egui::Window::new("File changed on disk")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("The file changed on disk while you had unsaved edits.");
                    ui.horizontal(|ui| {
                        if ui.button("Load disk version").clicked() {
                            if let Some(editor) = &mut self.editor {
                                match editor.load_disk_version() {
                                    Ok(()) => self.reload_decision = false,
                                    Err(e) => self.notice = Some(e),
                                }
                            }
                        }
                        if ui.button("Keep editing").clicked() {
                            self.reload_decision = false;
                        }
                    });
                });
        }
        if self.quit_decision {
            egui::Window::new("Before closing Skyforge").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("The file editor has unsaved changes. Draft prompts are saved automatically. Active runs will be stopped.");
                ui.horizontal(|ui| {
                    if ui.button("Save file and quit").clicked() && self.save_file() { self.allow_quit=true; ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                    if ui.button("Discard file edits and quit").clicked() { self.allow_quit=true; ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
                    if ui.button("Keep working").clicked() { self.quit_decision=false; }
                });
            });
        }
    }
}

impl eframe::App for Skyforge {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(due) = self.model_discovery_due {
            if Instant::now() >= due {
                self.refresh_local_models(ctx);
            } else {
                ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
            }
        }
        if let Some(result) = self
            .model_discovery
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.model_discovery = None;
            match result {
                Ok(catalog) => {
                    self.model_discovery_status =
                        format!("{} server models available in the composer.", catalog.ids.len());
                    self.config.local.apply_catalog(catalog);
                    self.persist_settings();
                    self.model_choices = runner::model_choices(&self.config.local);
                }
                Err(error) => self.model_discovery_status = error,
            }
        }
        // Optional developer smoke capture uses the app framebuffer only, never the desktop.
        if self.capture_path.is_some() {
            if !self.capture_requested && self.capture_started.elapsed() >= Duration::from_secs(1) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.capture_requested = true;
            } else if !self.capture_requested {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
            let screenshot = ctx.input(|i| {
                i.events.iter().find_map(|e| {
                    if let egui::Event::Screenshot { image, .. } = e {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(image) = screenshot {
                let path = self.capture_path.take().unwrap();
                let pixels = image
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_array())
                    .collect::<Vec<_>>();
                let result = image::save_buffer(
                    &path,
                    &pixels,
                    image.size[0] as u32,
                    image.size[1] as u32,
                    image::ColorType::Rgba8,
                );
                if let Err(e) = result {
                    self.notice = Some(format!("Capture failed: {e}"));
                }
                if std::env::var_os("HARNESS_CAPTURE_EXIT").is_some() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        self.process_events(ctx);
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_quit
            && self.editor.as_ref().is_some_and(FileBuffer::dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_decision = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.new_conversation(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            self.history = true;
            self.history_dirty = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            if self.tabs[self.active].mode == Mode::Builder {
                self.save_file();
            }
            self.save_active();
        }
        self.file_find_shortcut(ctx);
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            self.attach(dropped, ctx);
        }
        self.toolbar(ctx);
        self.sidebar(ctx);
        self.editor_ui(ctx);
        self.workspace(ctx);
        self.dialogs(ctx);
        self.advance_restart(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.delegation_host.shutdown();
        while self.delegation_host.try_recv().is_ok() {}
        self.save_active();
        for run in self.runs.values() {
            run.cancel();
        }
        // Drain output while joining so bounded worker channels cannot deadlock shutdown.
        let runs = std::mem::take(&mut self.runs);
        let join = std::thread::spawn(move || {
            for (_, run) in runs {
                run.finish();
            }
        });
        while !join.is_finished() {
            self.process_events(&egui::Context::default());
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = join.join();
        self.process_events(&egui::Context::default());
        for w in &mut self.tabs {
            if matches!(w.status.as_str(), "Running" | "Stopping") {
                w.status = "Stopped".into();
            }
            let _ = self.store.save(w);
        }
    }
}

fn take_prompt_send(ctx: &egui::Context, id: egui::Id) -> bool {
    if !ctx.memory(|memory| memory.has_focus(id)) {
        return false;
    }
    ctx.input_mut(|input| {
        let mut send = false;
        input.events.retain(|event| {
            if let egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                modifiers,
                repeat,
                ..
            } = event
            {
                // consume_key ignores extra Shift/Alt modifiers. Match exactly so
                // Shift+Enter reaches TextEdit and Enter in other fields stays local.
                if *modifiers == egui::Modifiers::NONE {
                    send |= !repeat;
                    return false;
                }
            }
            true
        });
        send
    })
}

fn file_tree(
    ui: &mut egui::Ui,
    tree: &FileNode,
    selected: Option<&Path>,
    open: &mut Option<PathBuf>,
    delete: &mut Option<PathBuf>,
    create: &mut Option<PathBuf>,
) {
    for (name, node) in tree.children.iter().filter(|(_, n)| !n.children.is_empty()) {
        egui::CollapsingHeader::new(name)
            .id_salt(&node.path)
            .default_open(name == "docs" || name == "src")
            .show(ui, |ui| {
                file_tree(ui, node, selected, open, delete, create);
            })
            .header_response
            .context_menu(|ui| new_file_menu(ui, &node.path, create));
    }
    for (name, node) in tree.children.iter().filter(|(_, n)| n.children.is_empty()) {
        file_entry(
            ui,
            &node.path,
            RichText::new(name).size(18.0),
            selected == Some(node.path.as_path()),
            open,
            delete,
            create,
        );
    }
}

fn file_entry(
    ui: &mut egui::Ui,
    path: &Path,
    label: impl Into<egui::WidgetText>,
    selected: bool,
    open: &mut Option<PathBuf>,
    delete: &mut Option<PathBuf>,
    create: &mut Option<PathBuf>,
) {
    let response = ui
        .selectable_label(selected, label)
        .on_hover_text(path.display().to_string());
    if response.clicked() {
        *open = Some(path.to_path_buf());
    }
    response.context_menu(|ui| {
        if let Some(directory) = path.parent() {
            new_file_menu(ui, directory, create);
        }
        if ui.button("Delete file").clicked() {
            *delete = Some(path.to_path_buf());
            ui.close_menu();
        }
    });
}

fn new_file_menu(ui: &mut egui::Ui, directory: &Path, create: &mut Option<PathBuf>) {
    if ui.button("New file…").clicked() {
        *create = Some(directory.to_path_buf());
        ui.close_menu();
    }
}

fn date(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%d %b").to_string())
        .unwrap_or_default()
}

fn activity_output(message: &Message) -> bool {
    matches!(
        message.role.as_str(),
        "Tool" | "Files" | "Shell" | "Search" | "Thinking" | "Diagnostics"
    )
}

fn activity_summary(message: &Message, body: &str) -> String {
    if message.role == "Files" {
        if let Ok(serde_json::Value::Array(changes)) = serde_json::from_str(body) {
            let paths = changes
                .iter()
                .filter_map(|change| change["path"].as_str())
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                return paths.join(", ").chars().take(100).collect();
            }
        }
    }
    body.lines()
        .next()
        .unwrap_or("View output")
        .chars()
        .take(100)
        .collect()
}

fn wrapped_activity(ui: &mut egui::Ui, id: egui::Id, title: String, body: &str, open: bool) {
    // CollapsingHeader forces its title onto one line, which can widen the
    // entire conversation. A custom header wraps both the title and output.
    let mut clicked = false;
    let mut header =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, open)
            .show_header(ui, |ui| {
                clicked = ui
                    .add(egui::Label::new(title).wrap().sense(egui::Sense::click()))
                    .clicked();
            });
    if clicked {
        header.toggle();
    }
    header.body(|ui| {
        ui.add(
            egui::Label::new(RichText::new(body).monospace().size(17.0))
                .wrap()
                .selectable(true),
        );
    });
}

fn linked_builder_card(ui: &mut egui::Ui, link: &crate::delegation::LinkedBuilder) -> bool {
    ui.label(RichText::new(format!("Builder · {}", link.title)).strong());
    let status = match link.status.as_str() {
        "Ready" if link.summary.is_some() => "Complete",
        "Stopped" => "Cancelled",
        status => status,
    };
    ui.label(format!("Status: {status}"));
    let open = ui.button("Open Builder").clicked();
    if let Some(error) = &link.error {
        ui.label(error);
    }
    if let Some(summary) = &link.summary {
        egui::CollapsingHeader::new("Completion summary · Builder reported output")
            .id_salt(("builder_result", &link.id))
            .show(ui, |ui| {
                ui.add(egui::Label::new(summary).wrap().selectable(true));
            });
    } else if matches!(link.status.as_str(), "Complete" | "Ready") {
        ui.label("Builder finished without a saved completion response. Open Builder for details.");
    }
    open
}

fn time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

fn highlight_matches(
    job: &mut egui::text::LayoutJob,
    text: &str,
    find: &str,
    active: Option<&std::ops::Range<usize>>,
) {
    let matches = text
        .match_indices(find)
        .map(|(i, s)| i..i + s.len())
        .collect::<Vec<_>>();
    let mut sections = vec![];
    for section in &job.sections {
        let mut cursor = section.byte_range.start;
        for hit in matches
            .iter()
            .filter(|h| h.start < section.byte_range.end && h.end > section.byte_range.start)
        {
            let start = hit.start.max(cursor);
            let end = hit.end.min(section.byte_range.end);
            if start > cursor {
                let mut s = section.clone();
                s.byte_range = cursor..start;
                sections.push(s);
            }
            let mut s = section.clone();
            s.byte_range = start..end;
            s.format.background = if active == Some(hit) {
                Color32::from_rgb(200, 135, 5)
            } else {
                Color32::from_rgb(158, 106, 3)
            };
            sections.push(s);
            cursor = end;
        }
        if cursor < section.byte_range.end {
            let mut s = section.clone();
            s.byte_range = cursor..section.byte_range.end;
            sections.push(s);
        }
    }
    job.sections = sections;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff_fixture() -> (
        PathBuf,
        egui::Context,
        Skyforge,
        Workstream,
        serde_json::Value,
    ) {
        let root =
            std::env::temp_dir().join(format!("skyforge-handoff-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let app = Skyforge::load(&ctx, Some(root.clone()), root.join("state")).unwrap();
        app.store
            .save_project(&ThinkerProject {
                id: "linked".into(),
                name: "Linked project".into(),
                instructions: String::new(),
                directory: Some(root.to_string_lossy().into()),
            })
            .unwrap();
        let mut source = Workstream::new(&root);
        source.mode = Mode::Thinker;
        source.thinker_project = Some("linked".into());
        source.invocation_turn = Some("host-turn".into());
        source.opened = false;
        app.store.save(&source).unwrap();
        app.store.save_submission(&source).unwrap();
        let args = serde_json::json!({"title":"Delegated task","task":"Requested work","acceptance_criteria":["Test it"]});
        (root, ctx, app, source, args)
    }

    fn handoff_via_ipc(
        app: &mut Skyforge,
        ctx: &egui::Context,
        source: &Workstream,
        args: &serde_json::Value,
    ) -> serde_json::Value {
        let endpoint = app.delegation_host.endpoint();
        let source = source.id.clone();
        let args = args.clone();
        let request = std::thread::spawn(move || {
            crate::delegation_ipc::call(&endpoint, &source, "host-turn", &args)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !request.is_finished() && Instant::now() < deadline {
            app.process_events(ctx);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            request.is_finished(),
            "Host must respond without periodic queue polling"
        );
        serde_json::from_str(&request.join().unwrap().unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn delegated_builder_runs_normal_path_once_and_finishes_with_closed_source() {
        use std::os::unix::fs::PermissionsExt;
        let (root, ctx, mut app, source, args) = handoff_fixture();
        let executable = root.join("codex-fixture");
        std::fs::write(&executable,r#"#!/bin/sh
printf '%s\n' "$@" > builder-arguments.txt
printf 'start\n' >> builder-starts.txt
cat > builder-prompt.txt
sleep 0.1
printf '%s\n' '{"type":"item.completed","item":{"id":"final","type":"agent_message","text":"Implemented and tested."}}'
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        app.config.binary = executable.to_string_lossy().into();
        app.config.writable = false;
        let result = handoff_via_ipc(&mut app, &ctx, &source, &args);
        assert_eq!(result["status"], "Running");
        let id = result["id"].as_str().unwrap();
        app.process_events(&ctx);
        assert!(app.busy(id));
        assert_eq!(
            app.tabs.iter().find(|w| w.id == id).unwrap().mode,
            Mode::Builder
        );
        let replay = handoff_via_ipc(&mut app, &ctx, &source, &args);
        assert_eq!(replay["id"], id);
        // Closing Builder's view also leaves its run and persistence intact.
        let index = app.tabs.iter().position(|w| w.id == id).unwrap();
        app.close_tab(index, &ctx);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.busy(id) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.process_events(&ctx);
        }
        assert!(!app.busy(id));
        assert_eq!(app.store.load(id).unwrap().status, "Complete");
        assert_eq!(
            std::fs::read_to_string(root.join("builder-starts.txt")).unwrap(),
            "start\n"
        );
        let prompt = std::fs::read_to_string(root.join("builder-prompt.txt")).unwrap();
        assert!(
            prompt.contains("Requested work")
                && prompt.contains("## Acceptance criteria")
                && prompt.contains(&source.id)
        );
        let cli_args = std::fs::read_to_string(root.join("builder-arguments.txt")).unwrap();
        assert!(
            cli_args.contains("read-only"),
            "Builder must keep the host's normal authorization"
        );
        assert!(!cli_args.contains("harness_thinker"));
        let link = app.store.linked_builders(&source.id).unwrap().remove(0);
        assert_eq!(link.summary.as_deref(), Some("Implemented and tested."));
        drop(app);
        let restored = Skyforge::load(&ctx, Some(root.clone()), root.join("state")).unwrap();
        let link = restored
            .store
            .linked_builders(&source.id)
            .unwrap()
            .remove(0);
        assert_eq!(link.status, "Complete");
        assert_eq!(link.summary.as_deref(), Some("Implemented and tested."));
        assert!(restored.store.queued_builders().unwrap().is_empty());
        drop(restored);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_builder_startup_error_is_visible_and_replay_does_not_restart() {
        let (root, ctx, mut app, source, args) = handoff_fixture();
        let result: serde_json::Value = serde_json::from_str(
            &crate::delegation::create(&args, &app.store, &source.id, "host-turn").unwrap(),
        )
        .unwrap();
        let id = result["id"].as_str().unwrap();
        let mut w = app.store.load(id).unwrap();
        w.model = crate::local::MODEL_ID.into();
        app.store.save(&w).unwrap();
        app.store.save_submission(&w).unwrap();
        app.config.local.host = "not a valid URL".into();
        let response = handoff_via_ipc(&mut app, &ctx, &source, &args);
        assert_eq!(response["id"], id);
        assert_eq!(response["status"], "Failed");
        app.process_events(&ctx);
        assert!(!app.busy(id));
        assert_eq!(app.store.load(id).unwrap().status, "Failed");
        let link = app.store.linked_builders(&source.id).unwrap().remove(0);
        assert!(link.error.is_some());
        assert!(link.summary.is_none());
        let replay = handoff_via_ipc(&mut app, &ctx, &source, &args);
        assert_eq!(replay["id"], id);
        assert_eq!(replay["status"], "Failed");
        app.process_events(&ctx);
        assert!(app.store.queued_builders().unwrap().is_empty());
        assert_eq!(app.store.messages(id, None).unwrap().len(), 1);
        assert!(app
            .store
            .submission(id)
            .unwrap()
            .unwrap()
            .draft
            .contains("Requested work"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_handoff_is_recovered_once_on_startup_without_idle_queue_polling() {
        let (root, ctx, mut app, source, args) = handoff_fixture();
        let result: serde_json::Value = serde_json::from_str(
            &crate::delegation::create(&args, &app.store, &source.id, "host-turn").unwrap(),
        )
        .unwrap();
        let id = result["id"].as_str().unwrap();
        // A raw database insertion does not wake or dispatch the live host.
        app.process_events(&ctx);
        assert_eq!(app.store.load(id).unwrap().status, "Queued");
        assert!(!app.busy(id));
        let mut snapshot = app.store.submission(id).unwrap().unwrap();
        snapshot.project = root.join("unavailable-workspace").to_string_lossy().into();
        app.store.save_submission(&snapshot).unwrap();
        drop(app);
        let mut restored = Skyforge::load(&ctx, Some(root.clone()), root.join("state")).unwrap();
        assert_eq!(restored.store.load(id).unwrap().status, "Failed");
        assert!(restored.store.linked_builders(&source.id).unwrap()[0]
            .error
            .as_ref()
            .unwrap()
            .contains("unavailable"));
        restored.process_events(&ctx);
        assert!(restored.store.queued_builders().unwrap().is_empty());
        drop(restored);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_builder_card_renders_lifecycle_results_and_navigation() {
        let ctx = egui::Context::default();
        ctx.memory_mut(|memory| memory.set_everything_is_visible(true));
        for (status, label, error, summary) in [
            ("Queued", "Queued", None, None),
            ("Running", "Running", None, None),
            ("Complete", "Complete", None, Some("Tests passed")),
            ("Ready", "Complete", None, Some("Local response")),
            ("Failed", "Failed", Some("Startup failed"), None),
            (
                "Stopped",
                "Cancelled",
                Some("Builder execution was cancelled."),
                None,
            ),
            (
                "Interrupted",
                "Interrupted",
                Some("Open Builder to inspect and retry."),
                None,
            ),
        ] {
            let link = crate::delegation::LinkedBuilder {
                id: "builder".into(),
                title: "Task title".into(),
                status: status.into(),
                summary: summary.map(str::to_owned),
                error: error.map(str::to_owned),
            };
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    linked_builder_card(ui, &link);
                });
            });
            let labels = output
                .shapes
                .iter()
                .filter_map(|s| {
                    if let egui::Shape::Text(t) = &s.shape {
                        Some(t.galley.job.text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(labels.contains("Builder · Task title"));
            assert!(labels.contains(&format!("Status: {label}")));
            assert!(labels.contains("Open Builder"));
            if let Some(error) = error {
                assert!(labels.contains(error));
            }
            if let Some(summary) = summary {
                assert!(labels.contains("Completion summary · Builder reported output"));
                assert!(labels.contains(summary));
            }
        }
    }

    #[test]
    fn settings_edits_survive_closing_and_restarting_without_save() {
        fn text_rect(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_rect(shape, label)),
                _ => None,
            }
        }
        let data = std::env::temp_dir().join(format!("skyforge-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.config.local.model = "deepseek-first".into();
        app.config
            .local
            .context_windows
            .insert("deepseek-first".into(), 35_000);
        app.config
            .local
            .context_windows
            .insert("deepseek-second".into(), 8192);
        app.persist_settings();
        app.settings_open = true;
        let frame = |app: &mut Skyforge, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 1400.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.dialogs(ctx),
            )
        };
        frame(&mut app, vec![]);
        let output = frame(&mut app, vec![]);
        let binary_pos = output
            .shapes
            .iter()
            .find_map(|shape| text_rect(&shape.shape, &app.config.binary))
            .unwrap()
            .center();
        let click = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(binary_pos),
                click(binary_pos, true),
            ],
        );
        frame(&mut app, vec![click(binary_pos, false)]);
        frame(
            &mut app,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                },
                egui::Event::Text("/tmp/autosaved-codex".into()),
            ],
        );
        assert_eq!(app.config.binary, "/tmp/autosaved-codex");
        let output = frame(&mut app, vec![]);
        let scroll_pos = output
            .shapes
            .iter()
            .find_map(|shape| text_rect(&shape.shape, "Host"))
            .unwrap()
            .center();
        frame(&mut app, vec![egui::Event::PointerMoved(scroll_pos)]);
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(scroll_pos),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -300.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        for _ in 0..15 {
            frame(&mut app, vec![]);
        }
        let output = frame(&mut app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| text_rect(&shape.shape, "35000"))
            .expect("context size control should be visible")
            .center();
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            &mut app,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(
            &mut app,
            vec![
                egui::Event::Text("65536".into()),
                egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(app.config.local.context_windows["deepseek-first"], 65536);
        frame(
            &mut app,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -250.0),
                modifiers: egui::Modifiers::NONE,
            }],
        );
        for _ in 0..15 {
            frame(&mut app, vec![]);
        }
        let output = frame(&mut app, vec![]);
        assert!(!output
            .shapes
            .iter()
            .any(|shape| text_rect(&shape.shape, "Save settings").is_some()));
        let checkbox_pos = output
            .shapes
            .iter()
            .find_map(|shape| text_rect(&shape.shape, "Enable live web research"))
            .unwrap()
            .center();
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(checkbox_pos),
                click(checkbox_pos, true),
            ],
        );
        frame(&mut app, vec![click(checkbox_pos, false)]);
        assert!(!app.config.search);
        app.settings_open = false;
        drop(app);
        let app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        assert_eq!(app.config.binary, "/tmp/autosaved-codex");
        assert!(!app.config.search);
        assert_eq!(app.config.local.context_windows["deepseek-first"], 65536);
        assert_eq!(app.config.local.context_windows["deepseek-second"], 8192);
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn rename_preserves_existing_state_and_uses_skyforge_for_new_installations() {
        let base = std::env::temp_dir().join(format!("skyforge-data-{}", uuid::Uuid::new_v4()));
        assert_eq!(application_data_directory(&base), base.join("Skyforge"));
        let legacy = base.join("Harness");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(application_data_directory(&base), base.join("Skyforge"));
        let store = Store::open(legacy.join("history.sqlite3")).unwrap();
        store
            .set_setting("last_project", "existing-project")
            .unwrap();
        assert_eq!(application_data_directory(&base), legacy);
        let retained =
            Store::open(application_data_directory(&base).join("history.sqlite3")).unwrap();
        assert_eq!(
            retained.setting("last_project").as_deref(),
            Some("existing-project")
        );
        drop(retained);
        drop(store);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn file_find_dialog_shortcut_navigates_selects_matches_and_closes_with_escape() {
        let data = std::env::temp_dir().join(format!("skyforge-find-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join("notes.md");
        let text = format!("🦀 first café\n{}last café", "line\n".repeat(100));
        std::fs::write(&path, &text).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.open_file(path);
        let frame = |app: &mut Skyforge, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    app.file_find_shortcut(ctx);
                    app.editor_ui(ctx);
                    app.workspace(ctx);
                    app.dialogs(ctx);
                },
            )
        };
        let key = |key, modifiers| egui::Event::Key {
            key,
            modifiers,
            physical_key: None,
            pressed: true,
            repeat: false,
        };
        let output = frame(&mut app, vec![]);
        assert!(!output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(text) if text.galley.job.text == "Find in file…")));
        frame(&mut app, vec![key(egui::Key::F, egui::Modifiers::COMMAND)]);
        assert!(app.find_open);
        frame(&mut app, vec![egui::Event::Text("café".into())]);
        let first = text.find("café").unwrap();
        assert_eq!(
            app.editor.as_ref().unwrap().find_match,
            Some(first..first + "café".len())
        );
        frame(&mut app, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        let last = text.rfind("café").unwrap();
        assert_eq!(
            app.editor.as_ref().unwrap().find_match,
            Some(last..last + "café".len())
        );
        frame(&mut app, vec![]); // Apply selection and scroll in the editor.
        let state =
            egui::text_edit::TextEditState::load(&ctx, app.source_editor_id.unwrap()).unwrap();
        let selected = state.cursor.char_range().unwrap();
        assert_eq!(
            selected.primary.index.min(selected.secondary.index),
            text[..last].chars().count()
        );
        assert_eq!(
            selected.primary.index.max(selected.secondary.index),
            text[..last + "café".len()].chars().count()
        );
        frame(
            &mut app,
            vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)],
        );
        assert_eq!(
            app.editor.as_ref().unwrap().find_match,
            Some(first..first + "café".len())
        );
        frame(
            &mut app,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        assert!(!app.find_open);
        assert_eq!(app.editor.as_ref().unwrap().text, text);
        assert!(!app.editor.as_ref().unwrap().dirty());
        assert!(app.tabs[app.active].draft.is_empty());
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn repository_snapshots_refresh_editor_even_when_file_paths_are_unchanged() {
        let data =
            std::env::temp_dir().join(format!("skyforge-editor-watch-{}", uuid::Uuid::new_v4()));
        let project = data.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let path = project.join("notes.md");
        std::fs::write(&path, "original").unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(project), data.join("state")).unwrap();
        app.open_file(path.clone());
        app.editor.as_mut().unwrap().find = "agent".into();
        let wait = |app: &mut Skyforge, predicate: &dyn Fn(&Skyforge) -> bool| {
            let began = Instant::now();
            while began.elapsed() < Duration::from_secs(5) {
                app.process_events(&ctx);
                if predicate(app) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("Editor did not refresh from repository snapshot");
        };
        wait(&mut app, &|app| !app.scanning);
        let files = app.files.clone();
        std::fs::write(&path, "agent edit").unwrap();
        // Explicit wake makes the pipeline deterministic without relying on
        // FSEvents delivery in sandboxed macOS test environments.
        app.repository_watch.as_ref().unwrap().refresh();
        wait(&mut app, &|app| {
            app.editor.as_ref().unwrap().text == "agent edit"
        });
        assert_eq!(app.files, files);
        assert_eq!(app.editor.as_ref().unwrap().find, "agent");
        assert!(!app.editor.as_ref().unwrap().dirty());
        assert!(!app.reload_decision);

        app.editor.as_mut().unwrap().text = "unsaved local edits".into();
        std::fs::write(&path, "second agent edit").unwrap();
        app.repository_watch.as_ref().unwrap().refresh();
        wait(&mut app, &|app| app.reload_decision);
        assert_eq!(app.editor.as_ref().unwrap().text, "unsaved local edits");
        assert!(!app.save_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second agent edit");
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn repository_refresh_keeps_tree_visible_and_navigation_uses_current_watch() {
        let data = std::env::temp_dir().join(format!("harness-tree-{}", uuid::Uuid::new_v4()));
        let first = data.join("first");
        let second = data.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        std::fs::write(first.join("before.txt"), "before").unwrap();
        std::fs::write(second.join("second.txt"), "second").unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(first.clone()), data.join("state")).unwrap();
        let wait = |app: &mut Skyforge, expected: Vec<PathBuf>| {
            let began = Instant::now();
            while began.elapsed() < Duration::from_secs(5) {
                app.process_events(&ctx);
                if !app.scanning && app.files == expected {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("Expected {expected:?}, got {:?}", app.files);
        };
        wait(&mut app, vec![first.join("before.txt")]);
        std::fs::rename(first.join("before.txt"), first.join("after.txt")).unwrap();
        app.scan(&ctx);
        assert!(app.tree.children.contains_key("before.txt"));
        wait(&mut app, vec![first.join("after.txt")]);
        assert!(app.tree.children.contains_key("after.txt"));

        app.scan(&ctx); // Leave a refresh pending on the previous repository.
        app.new_tab(second.clone(), &ctx);
        wait(&mut app, vec![second.join("second.txt")]);
        assert_eq!(app.repository_watch.as_ref().unwrap().root, second);
        assert!(!app.tree.children.contains_key("after.txt"));
        app.switch_mode(Mode::Thinker, &ctx);
        app.process_events(&ctx);
        assert!(app.repository_watch.is_none());
        app.switch_mode(Mode::Builder, &ctx);
        wait(&mut app, vec![second.join("second.txt")]);
        drop(app);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn modes_keep_drafts_separate_and_thinker_memory_projects_survive_navigation() {
        let data = std::env::temp_dir().join(format!("harness-mode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.tabs[app.active].draft = "Builder draft".into();
        let builder = app.tabs[app.active].id.clone();
        app.switch_mode(Mode::Thinker, &ctx);
        assert_eq!(app.tabs[app.active].mode, Mode::Thinker);
        app.tabs[app.active].draft = "Thinker draft".into();
        let thinker = app.tabs[app.active].id.clone();
        let project = ThinkerProject {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Launch plans".into(),
            instructions: "Discuss the pilot".into(),
            directory: None,
        };
        app.store.save_project(&project).unwrap();
        app.projects = app.store.projects().unwrap();
        app.move_thinker(&thinker, Some(&project.id));
        app.tx.send(Envelope{workstream:thinker.clone(),event:Event::Message{item:"answer".into(),role:"Agent".into(),body:"Let's discuss it.\n\n```harness-memory\n[{\"scope\":\"global\",\"key\":\"style\",\"content\":\"Concise answers\"},{\"scope\":\"project\",\"key\":\"launch\",\"content\":\"Pilot first\"}]\n```".into()}}).unwrap();
        app.process_events(&ctx);
        assert_eq!(app.store.memories("").unwrap().len(), 1);
        assert_eq!(app.store.memories(&project.id).unwrap().len(), 1);
        assert!(app
            .messages
            .iter()
            .any(|m| m.role == "Agent" && m.body == "Let's discuss it."));
        assert!(!app
            .messages
            .iter()
            .any(|m| m.body.contains("harness-memory")));
        let mut submitted = app.tabs[app.active].clone();
        submitted.memory_revision = Some(app.store.memory_revision());
        app.store.save_submission(&submitted).unwrap();
        app.store.forget_key("", "style").unwrap();
        app.tx.send(Envelope {workstream:thinker.clone(),event:Event::Message {item:"outdated".into(),role:"Agent".into(),body:"A reply from an older context.\n```harness-memory\n[{\"scope\":\"global\",\"key\":\"style\",\"content\":\"Concise answers\"}]\n```".into()}}).unwrap();
        app.process_events(&ctx);
        assert!(
            app.store.memories("").unwrap().is_empty(),
            "An in-flight reply must not resurrect forgotten memories"
        );
        app.switch_mode(Mode::Builder, &ctx);
        assert_eq!(app.tabs[app.active].id, builder);
        assert_eq!(app.tabs[app.active].draft, "Builder draft");
        app.switch_mode(Mode::Thinker, &ctx);
        assert_eq!(app.tabs[app.active].id, thinker);
        assert_eq!(app.tabs[app.active].draft, "Thinker draft");
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1360.0, 900.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            app.toolbar(ctx);
            app.sidebar(ctx);
            app.workspace(ctx);
        });
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|s| {
                if let egui::Shape::Text(text) = &s.shape {
                    Some(text.galley.job.text.as_str())
                } else {
                    None
                }
            })
            .collect();
        for label in [
            "Builder",
            "Thinker",
            "+ Project",
            "New conversation",
            "Chatbot · read-only",
        ] {
            assert!(labels.contains(&label), "Missing UI: {label}");
        }
        assert!(!labels.contains(&"Editor"));
        assert!(!labels.contains(&"PROJECT FILES"));
        app.close_tab(app.active, &ctx);
        assert_eq!(app.tabs[app.active].mode, Mode::Thinker);
        assert_ne!(app.tabs[app.active].id, thinker);
        assert!(app
            .store
            .thinker_context(&app.tabs[app.active])
            .unwrap()
            .contains("Pilot first"));
        app.save_active();
        let active = app.tabs[app.active].id.clone();
        drop(app);
        let restored = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        assert_eq!(restored.tabs[restored.active].id, active);
        assert_eq!(
            restored.store.setting("last_project"),
            Some(data.canonicalize().unwrap().to_string_lossy().into_owned())
        );
        drop(restored);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn new_file_dialog_supports_enter_errors_and_escape() {
        let data =
            std::env::temp_dir().join(format!("skyforge-create-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let project = data.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(project.clone()), data.join("state")).unwrap();
        let frame = |app: &mut Skyforge, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.dialogs(ctx),
            );
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        app.new_file = Some(NewFile {
            directory: project.clone(),
            name: String::new(),
            error: None,
            focus: true,
        });
        frame(&mut app, vec![]);
        frame(&mut app, vec![key(egui::Key::Enter)]);
        assert!(app.new_file.as_ref().unwrap().error.is_some());
        frame(&mut app, vec![]);
        frame(&mut app, vec![egui::Event::Text("keyboard.txt".into())]);
        frame(&mut app, vec![key(egui::Key::Enter)]);
        assert!(app.new_file.is_none());
        assert_eq!(
            app.editor.as_ref().unwrap().path,
            project.join("keyboard.txt")
        );

        app.new_file = Some(NewFile {
            directory: project.clone(),
            name: "cancelled.txt".into(),
            error: None,
            focus: true,
        });
        frame(&mut app, vec![]);
        frame(&mut app, vec![key(egui::Key::Escape)]);
        assert!(app.new_file.is_none());
        assert!(!project.join("cancelled.txt").exists());
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn creating_repository_files_opens_editor_and_preserves_existing_files_and_edits() {
        let data = std::env::temp_dir().join(format!("skyforge-create-{}", uuid::Uuid::new_v4()));
        let project = data.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let project = project.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(project.clone()), data.join("state")).unwrap();

        app.file_query = "old search".into();
        app.create_file(&project, "first.txt").unwrap();
        let first = project.join("first.txt");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "");
        assert_eq!(app.editor.as_ref().unwrap().path, first);
        assert!(app.show_editor);
        assert!(app.file_query.is_empty());
        assert!(app.tree.children.contains_key("first.txt"));

        app.editor.as_mut().unwrap().text = "unsaved edits".into();
        app.create_file(&project.join("src"), "second.rs").unwrap();
        let second = project.join("src/second.rs");
        assert!(second.is_file());
        assert!(app.tree.children["src"].children.contains_key("second.rs"));
        assert_eq!(app.editor.as_ref().unwrap().path, first);
        assert_eq!(app.editor.as_ref().unwrap().text, "unsaved edits");
        assert_eq!(app.pending_file.as_ref(), Some(&second));
        assert!(app.file_decision);

        std::fs::write(&second, "existing contents").unwrap();
        let files = app.files.clone();
        assert!(app
            .create_file(&project.join("src"), "second.rs")
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            "existing contents"
        );
        assert_eq!(app.files, files);
        assert_eq!(app.editor.as_ref().unwrap().text, "unsaved edits");

        for name in [
            "",
            "  ",
            ".",
            "..",
            "../outside.txt",
            "/absolute.txt",
            "nested/file.txt",
            "bad\\name",
            "bad\0name",
        ] {
            assert!(
                app.create_file(&project, name).is_err(),
                "Accepted {name:?}"
            );
        }
        assert!(app.create_file(&data, "outside.txt").is_err());
        assert!(!data.join("outside.txt").exists());
        assert!(app
            .create_file(&project.join("missing"), "file.txt")
            .is_err());
        assert_eq!(app.files, files);
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn deleting_repository_files_updates_tree_and_closes_deleted_editor_without_prompt() {
        let data = std::env::temp_dir().join(format!("harness-delete-{}", uuid::Uuid::new_v4()));
        let project = data.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let project = project.canonicalize().unwrap();
        let current = project.join("src/current.txt");
        let other = project.join("other.txt");
        std::fs::write(&current, "saved text").unwrap();
        std::fs::write(&other, "other text").unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(project.clone()), data.join("state")).unwrap();
        app.files = vec![current.clone(), other.clone()];
        app.tree = FileNode::build(&project, &app.files);
        app.open_file(current.clone());
        app.editor.as_mut().unwrap().text = "unsaved edits".into();

        // A failed deletion keeps the file list and unsaved editor intact.
        app.delete_file(&project.join("src"));
        assert!(app.notice.as_ref().unwrap().contains("Cannot delete"));
        assert_eq!(app.files, vec![current.clone(), other.clone()]);
        assert!(app.editor.as_ref().unwrap().dirty());
        assert!(current.exists());
        app.notice = None;

        // Deleting another file leaves the current edits untouched.
        app.delete_file(&other);
        assert!(!other.exists());
        assert_eq!(app.files, vec![current.clone()]);
        assert!(!app.tree.children.contains_key("other.txt"));
        assert_eq!(app.editor.as_ref().unwrap().text, "unsaved edits");
        assert!(app.show_editor);
        assert!(!app.file_decision);

        app.delete_file(&current);
        assert!(!current.exists());
        assert!(app.files.is_empty());
        assert!(app.tree.children.is_empty());
        assert!(app.editor.is_none());
        assert!(!app.show_editor);
        assert!(!app.file_decision);
        assert!(!app.reload_decision);
        assert!(app.pending_file.is_none());
        assert!(app.notice.is_none());
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn conversation_wraps_long_output_and_omits_tool_heading() {
        fn check_text(shape: &egui::Shape, width: f32) {
            match shape {
                egui::Shape::Text(text) => {
                    assert!(
                        text.galley.size().x <= width,
                        "Text overflow: {} > {width}: {}",
                        text.galley.size().x,
                        text.galley.text().chars().take(110).collect::<String>()
                    );
                    assert_ne!(text.galley.text(), "TOOL");
                    assert_ne!(text.galley.text(), "FILES");
                    assert_ne!(text.galley.text(), "File changes");
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        check_text(shape, width);
                    }
                }
                _ => {}
            }
        }
        let data = std::env::temp_dir().join(format!("harness-wrap-{}", uuid::Uuid::new_v4()));
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(std::env::temp_dir()), data.clone()).unwrap();
        let long_line = "Long output with text that should wrap. ".repeat(25);
        let long_token = "unbroken_output".repeat(80);
        for width in [360.0, 700.0] {
            for (role, body) in [
                ("Agent", long_line.clone()),
                ("Agent", long_token.clone()),
                ("Agent", format!("```text\n{long_line}\n{long_token}\n```")),
                (
                    "Shell",
                    format!("Command output\n{long_line}\n{long_token}"),
                ),
                ("Tool", long_line.clone()),
                ("Files", format!("File changes\n{long_line}")),
            ] {
                app.messages = vec![Message {
                    id: 1,
                    role: role.into(),
                    body,
                    attachments: vec![],
                    created: 0,
                }];
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 800.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            app.conversation(ui);
                        });
                    },
                );
                for shape in output.shapes {
                    check_text(&shape.shape, width - 16.0);
                }
            }
        }
        app.messages = [
            ("Agent", "Updating documentation."),
            ("Tool", "$ cargo check\nCheck passed."),
            ("Files", "File changes\nREADME.md updated"),
            ("Agent", "Done."),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (role, body))| Message {
            id: i as i64,
            role: role.into(),
            body: body.into(),
            attachments: vec![],
            created: 0,
        })
        .collect();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.conversation(ui));
            },
        );
        fn count_agent(shape: &egui::Shape) -> usize {
            match shape {
                egui::Shape::Text(text) => usize::from(text.galley.text() == "AGENT"),
                egui::Shape::Vec(shapes) => shapes.iter().map(count_agent).sum(),
                _ => 0,
            }
        }
        assert_eq!(
            output
                .shapes
                .iter()
                .map(|s| count_agent(&s.shape))
                .sum::<usize>(),
            2
        );
        for shape in output.shapes {
            check_text(&shape.shape, 684.0);
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn prompt_shift_enter_adds_newline_and_enter_sends_only_when_focused() {
        use std::os::unix::fs::PermissionsExt;
        let data = std::env::temp_dir().join(format!("harness-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let executable = data.join("agent-fixture");
        std::fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"fixture\"}'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.config.binary = executable.to_string_lossy().into_owned();
        app.tabs[app.active].draft = "Hello".into();
        let frame = |app: &mut Skyforge, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.workspace(ctx),
            );
        };
        let enter = |modifiers| {
            [true, false]
                .into_iter()
                .map(|pressed| egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers,
                })
                .collect()
        };
        frame(&mut app, vec![]);
        frame(&mut app, enter(egui::Modifiers::SHIFT));
        assert_eq!(app.tabs[app.active].draft, "Hello\n");
        assert!(app.runs.is_empty());
        frame(&mut app, enter(egui::Modifiers::COMMAND));
        assert_eq!(app.tabs[app.active].draft, "Hello\n");
        assert!(app.runs.is_empty());
        ctx.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
        frame(&mut app, enter(egui::Modifiers::NONE));
        assert_eq!(app.tabs[app.active].draft, "Hello\n");
        assert!(app.runs.is_empty());
        app.focus_prompt = true;
        frame(&mut app, vec![]);
        frame(&mut app, enter(egui::Modifiers::NONE));
        assert!(app.tabs[app.active].draft.is_empty());
        assert_eq!(app.messages.iter().filter(|m| m.role == "You").count(), 1);
        assert_eq!(
            app.messages.iter().find(|m| m.role == "You").unwrap().body,
            "Hello\n"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.runs.is_empty() {
            assert!(Instant::now() < deadline);
            app.process_events(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn restart_waits_for_runs_protects_file_edits_and_restores_active_tab() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let data =
            std::env::temp_dir().join(format!("harness-restart-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.tabs[0].draft = "First draft".into();
        let first_id = app.tabs[0].id.clone();
        app.new_tab(data.clone(), &ctx);
        let active_id = app.tabs[app.active].id.clone();
        app.tabs[app.active].draft = "Second draft".into();
        app.tabs[app.active].session = Some("existing-session".into());
        let document = data.join("notes.md");
        std::fs::write(&document, "original").unwrap();
        app.open_file(document.clone());
        app.editor.as_mut().unwrap().text = "unsaved file changes".into();
        let committed = Arc::new(AtomicBool::new(false));
        app.restart.set_handoff(committed.clone());
        let run = runner::start_shell(
            "sleep 0.1; printf 'run completed'",
            &app.tabs[app.active],
            app.tx.clone(),
            ctx.clone(),
        );
        app.runs.insert(active_id.clone(), run);
        std::fs::write(app.restart.request_path(), "").unwrap();
        app.process_events(&ctx);
        app.advance_restart(&ctx);
        assert!(app.restart.requested);
        assert!(!app.restart_decision && !committed.load(Ordering::SeqCst));
        // Even a direct commit must wait for a newly started run.
        app.commit_restart(&ctx);
        assert!(!committed.load(Ordering::SeqCst));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.runs.is_empty() {
            assert!(Instant::now() < deadline);
            app.process_events(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        app.advance_restart(&ctx);
        assert!(app.restart_decision && !committed.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(&document).unwrap(), "original");
        assert!(app.save_file());
        app.commit_restart(&ctx);
        assert!(app.allow_quit && committed.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(&document).unwrap(),
            "unsaved file changes"
        );
        drop(app);
        let restored = Skyforge::load(&ctx, None, data.join("state")).unwrap();
        assert_eq!(restored.tabs[restored.active].id, active_id);
        assert_eq!(restored.tabs[restored.active].draft, "Second draft");
        assert_eq!(
            restored.tabs[restored.active].session.as_deref(),
            Some("existing-session")
        );
        assert_eq!(
            restored
                .tabs
                .iter()
                .find(|w| w.id == first_id)
                .unwrap()
                .draft,
            "First draft"
        );
        assert!(restored
            .messages
            .iter()
            .any(|m| m.body.contains("run completed")));
        assert!(!restored.restart.requested);
        drop(restored);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn workstream_navigation_retains_drafts_and_closed_history() {
        let data = std::env::temp_dir().join(format!("harness-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        let first = app.tabs[0].id.clone();
        app.tabs[0].draft = "First **draft**".into();
        app.new_tab(data.clone(), &ctx);
        app.tabs[app.active].draft = "Second *draft*".into();
        app.select(0, &ctx);
        assert_eq!(app.tabs[0].draft, "First **draft**");
        app.close_tab(0, &ctx);
        assert_eq!(app.tabs[app.active].draft, "Second *draft*");
        assert!(!app.store.load(&first).unwrap().opened);
        let hit = app.store.search("First", None, 0).unwrap().remove(0);
        app.reopen(hit, &ctx);
        assert_eq!(app.tabs[app.active].id, first);
        assert_eq!(app.tabs[app.active].draft, "First **draft**");
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn project_tabs_restore_workstreams_and_repository_across_navigation_and_reload() {
        let data = std::env::temp_dir().join(format!("harness-projects-{}", uuid::Uuid::new_v4()));
        let first = data.join("first");
        let second = data.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(first.clone()), data.join("state")).unwrap();
        let initial = app.tabs[app.active].id.clone();
        app.new_conversation(&ctx);
        let first_active = app.tabs[app.active].id.clone();
        app.tabs[app.active].draft = "first project draft".into();
        app.store
            .add_message(&first_active, "User", "first project message", &[])
            .unwrap();
        app.open_project(second.clone(), &ctx);
        let second_initial = app.tabs[app.active].id.clone();
        app.new_conversation(&ctx);
        let second_active = app.tabs[app.active].id.clone();
        app.tabs[app.active].draft = "second project draft".into();
        assert_eq!(app.builder_projects().len(), 2);
        assert_eq!(
            app.tabs
                .iter()
                .filter(|w| app.visible_workstream(w))
                .count(),
            2
        );
        app.open_project(first.join("."), &ctx);
        assert_eq!(app.tabs[app.active].id, first_active);
        assert_eq!(app.tabs[app.active].draft, "first project draft");
        assert_eq!(app.messages[0].body, "first project message");
        assert_eq!(app.repository_watch.as_ref().unwrap().root, first);
        assert!(app.tabs.iter().find(|w| w.id == initial).unwrap().opened);
        assert_eq!(
            app.tabs.len(),
            4,
            "Opening an existing project must reuse its tab"
        );
        app.open_project(second.clone(), &ctx);
        assert_eq!(app.tabs[app.active].id, second_active);
        assert_eq!(app.repository_watch.as_ref().unwrap().root, second);
        app.save_active();
        drop(app);
        let mut app = Skyforge::load(&ctx, None, data.join("state")).unwrap();
        assert_eq!(app.tabs[app.active].id, second_active);
        app.open_project(first.clone(), &ctx);
        assert_eq!(app.tabs[app.active].id, first_active);
        // Closing the last tab creates a replacement in the same project.
        app.close_tab(app.active, &ctx);
        assert_eq!(app.tabs[app.active].id, initial);
        app.close_tab(app.active, &ctx);
        assert_eq!(Path::new(&app.tabs[app.active].project), first);
        app.open_project(second, &ctx);
        assert_eq!(app.tabs[app.active].id, second_active);
        app.close_tab(app.active, &ctx);
        assert_eq!(app.tabs[app.active].id, second_initial);
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn closing_projects_retains_history_and_restores_remaining_selection() {
        let data =
            std::env::temp_dir().join(format!("harness-close-projects-{}", uuid::Uuid::new_v4()));
        let first = data.join("first");
        let second = data.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(first.clone()), data.join("state")).unwrap();
        app.new_conversation(&ctx);
        let first_active = app.tabs[app.active].id.clone();
        app.tabs[app.active].draft = "preserved project draft".into();
        app.store
            .add_message(&first_active, "User", "preserved project history", &[])
            .unwrap();
        app.open_project(second.clone(), &ctx);
        app.new_conversation(&ctx);
        let second_active = app.tabs[app.active].id.clone();
        app.store
            .add_message(&second_active, "User", "second project history", &[])
            .unwrap();
        app.open_project(first.clone(), &ctx);

        app.close_project(&second.to_string_lossy(), &ctx);
        assert_eq!(
            app.tabs[app.active].id, first_active,
            "Closing a background project keeps the selection"
        );
        assert!(app
            .tabs
            .iter()
            .filter(|w| Path::new(&w.project) == second)
            .all(|w| !w.opened && !app.store.load(&w.id).unwrap().opened));
        let hit = app
            .store
            .search("second project history", None, 0)
            .unwrap()
            .into_iter()
            .find(|hit| hit.id == second_active)
            .unwrap();
        app.reopen(hit, &ctx);
        app.new_conversation(&ctx);
        let remaining_active = app.tabs[app.active].id.clone();
        app.open_project(first.clone(), &ctx);
        app.close_project(&first.to_string_lossy(), &ctx);
        assert_eq!(app.tabs[app.active].id, remaining_active);
        assert_eq!(app.repository_watch.as_ref().unwrap().root, second);
        assert_eq!(
            app.store.load(&first_active).unwrap().draft,
            "preserved project draft"
        );
        assert_eq!(
            app.store.messages(&first_active, None).unwrap()[0].body,
            "preserved project history"
        );
        assert!(app
            .tabs
            .iter()
            .filter(|w| Path::new(&w.project) == first)
            .all(|w| !w.opened && !app.store.load(&w.id).unwrap().opened));

        app.close_project(&second.to_string_lossy(), &ctx);
        assert!(app.builder_projects().is_empty());
        assert_eq!(app.tabs[app.active].mode, Mode::Thinker);
        assert!(app.tabs[app.active].opened);
        assert!(app.repository_watch.is_none());
        app.save_active();
        drop(app);
        let mut app = Skyforge::load(&ctx, None, data.join("state")).unwrap();
        assert!(
            app.builder_projects().is_empty(),
            "Closed projects stay closed after reload"
        );
        let hit = app
            .store
            .search("preserved", None, 0)
            .unwrap()
            .into_iter()
            .find(|hit| hit.id == first_active)
            .unwrap();
        app.reopen(hit, &ctx);
        assert_eq!(app.tabs[app.active].id, first_active);
        assert_eq!(app.tabs[app.active].draft, "preserved project draft");
        assert_eq!(
            app.builder_projects(),
            vec![first.to_string_lossy().into_owned()]
        );
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn composer_retains_height_after_dragging_divider() {
        let data = std::env::temp_dir().join(format!("harness-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        let panel_id = egui::Id::new("composer_panel");
        let mut frame = |events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.workspace(ctx),
            );
            egui::containers::panel::PanelState::load(&ctx, panel_id)
                .unwrap()
                .rect
        };
        let initial = frame(vec![]);
        let start = egui::pos2(initial.center().x, initial.top());
        frame(vec![
            egui::Event::PointerMoved(start),
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let end = start - egui::vec2(0.0, 150.0);
        frame(vec![egui::Event::PointerMoved(end)]);
        frame(vec![]);
        let resized = frame(vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(resized.height() > initial.height() + 100.0);
        for _ in 0..20 {
            let settled = frame(vec![]);
            assert!((settled.height() - resized.height()).abs() < 1.0);
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn workstream_tabs_only_show_selected_project_and_keep_controls_aligned() {
        fn text_rects(shape: &egui::Shape, labels: &[&str], rects: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Text(text) if labels.contains(&text.galley.text()) => {
                    rects.push(egui::Rect::from_min_size(text.pos, text.galley.size()));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        text_rects(shape, labels, rects);
                    }
                }
                _ => {}
            }
        }
        let data = std::env::temp_dir().join(format!("harness-tabs-{}", uuid::Uuid::new_v4()));
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.tabs = vec![
            Workstream::new(Path::new("/projects/Skyforge")),
            Workstream::new(Path::new("/projects/Storefront")),
        ];
        app.tabs[0].title = "Improve onboarding".into();
        app.tabs[1].title = "Fix checkout".into();
        app.active = 0;
        for size in [egui::vec2(850.0, 600.0), egui::vec2(1360.0, 900.0)] {
            for frame in 0..3 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| app.toolbar(ctx),
                );
                if frame == 0 {
                    continue; // ScrollArea establishes its viewport on the first frame.
                }
                let mut project_rects = Vec::new();
                for shape in &output.shapes {
                    text_rects(&shape.shape, &["Storefront"], &mut project_rects);
                }
                assert_eq!(
                    project_rects.len(),
                    1,
                    "Other projects stay accessible in the toolbar"
                );
                let mut rects = Vec::new();
                for shape in &output.shapes {
                    text_rects(
                        &shape.shape,
                        &["Improve onboarding", "Fix checkout", "×", "+ New"],
                        &mut rects,
                    );
                }
                let mut workstream_rects = Vec::new();
                for shape in &output.shapes {
                    text_rects(&shape.shape, &["Improve onboarding"], &mut workstream_rects);
                }
                let workstream_y = workstream_rects[0].center().y;
                let project_closes = rects
                    .iter()
                    .filter(|rect| rect.center().y < workstream_y - 1.0)
                    .count();
                assert_eq!(project_closes, 2, "Each project has a close button");
                rects.retain(|rect| rect.center().y >= workstream_y - 1.0);
                assert_eq!(
                    rects.len(),
                    3,
                    "Only the selected project's workstream and its controls must render"
                );
                for rect in &rects[1..] {
                    assert!(
                        (rect.center().y - rects[0].center().y).abs() < 1.0,
                        "Tab controls must share a vertical center: {rects:?}"
                    );
                }
            }
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn composer_controls_fit_at_minimum_window_size() {
        fn text_rect(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| text_rect(s, label)),
                _ => None,
            }
        }
        let data = std::env::temp_dir().join(format!("harness-ui-{}", uuid::Uuid::new_v4()));
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(std::env::temp_dir()), data.clone()).unwrap();
        app.tabs[app.active].model = "claude:sonnet".into();
        for size in [egui::vec2(850.0, 600.0), egui::vec2(1360.0, 900.0)] {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ctx| {
                    app.toolbar(ctx);
                    app.sidebar(ctx);
                    app.workspace(ctx);
                },
            );
            let panel =
                egui::containers::panel::PanelState::load(&ctx, egui::Id::new("composer_panel"))
                    .unwrap()
                    .rect;
            let centers: Vec<_> = ["+ Image", "Anthropic Claude · Sonnet"]
                .iter()
                .map(|label| {
                    output
                        .shapes
                        .iter()
                        .find_map(|s| text_rect(&s.shape, label))
                        .unwrap()
                        .center()
                        .y
                })
                .collect();
            assert!(
                centers.iter().all(|y| (y - centers[0]).abs() <= 1.0),
                "Composer controls must share a vertical center: {centers:?}"
            );
            for label in ["+ Image", "Anthropic Claude · Sonnet", "Send  ↗"] {
                let (clip, rect) = output
                    .shapes
                    .iter()
                    .find_map(|s| text_rect(&s.shape, label).map(|r| (s.clip_rect, r)))
                    .unwrap();
                assert!(
                    clip.contains_rect(rect),
                    "{label} must remain visible: {rect:?} in {clip:?}"
                );
                assert!(
                    panel.contains_rect(rect.expand2(egui::vec2(12.0, 8.0))),
                    "{label} hover must fit inside the panel: {rect:?} in {panel:?}"
                );
            }
            for label in [
                "Web",
                "Write",
                "Preview",
                "Shell",
                "YOUR PROMPT",
                "RUST PROTOTYPE",
                "Expand ↗",
                "Shrink ↙",
            ] {
                assert!(!output
                    .shapes
                    .iter()
                    .any(|s| text_rect(&s.shape, label).is_some()));
            }
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn ui_renders_composer_editor_and_history() {
        let data = std::env::temp_dir().join(format!("harness-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let document = data.join("notes.md");
        std::fs::write(&document, "# Notes\n\nA *readable* document.").unwrap();
        let ctx = egui::Context::default();
        let mut app = Skyforge::load(&ctx, Some(data.clone()), data.join("state")).unwrap();
        app.tabs[0].draft =
            "# A plan\n\nBuild *something useful* with **Rust**.\n\n```rust\nfn main() {}\n```"
                .into();
        for mode in 0..4 {
            app.settings_open = mode == 3;
            app.history = mode == 1;
            if mode == 2 {
                app.open_file(document.clone());
            }
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    app.process_events(ctx);
                    app.toolbar(ctx);
                    app.sidebar(ctx);
                    app.editor_ui(ctx);
                    app.workspace(ctx);
                    app.dialogs(ctx);
                },
            );
            assert!(!output.shapes.is_empty());
            if mode == 2 {
                for label in ["Read", "Edit", "Reload…", "Save copy…"] {
                    assert!(!output
                        .shapes
                        .iter()
                        .any(|s| matches!(&s.shape, egui::Shape::Text(text) if text.galley.job.text == label)));
                }
            }
        }
        drop(app);
        std::fs::remove_dir_all(data).unwrap();
    }
}
