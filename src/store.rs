use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Builder,
    Thinker,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builder => "Builder",
            Self::Thinker => "Thinker",
        }
    }
    fn stored(self) -> &'static str {
        match self {
            Self::Builder => "builder",
            Self::Thinker => "thinker",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ThinkerProject {
    pub id: String,
    pub name: String,
    pub instructions: String,
    pub directory: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Memory {
    pub id: i64,
    pub scope: String,
    pub key: String,
    pub content: String,
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workstream {
    pub id: String,
    pub title: String,
    pub project: String,
    pub draft: String,
    pub attachments: Vec<String>,
    pub session: Option<String>,
    pub model: String,
    pub status: String,
    pub opened: bool,
    pub updated: i64,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub thinker_project: Option<String>,
    #[serde(default)]
    pub memory_revision: Option<i64>,
    #[serde(default)]
    pub invocation_turn: Option<String>,
    #[serde(skip)]
    pub context: String,
    #[serde(skip)]
    pub instructions_path: Option<PathBuf>,
    #[serde(skip)]
    pub memory_db: Option<PathBuf>,
    #[serde(skip)]
    pub delegation_endpoint: Option<crate::delegation_ipc::Endpoint>,
}

impl Workstream {
    pub fn new(project: &Path) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: "Untitled workstream".into(),
            project: project.to_string_lossy().into_owned(),
            draft: String::new(),
            attachments: vec![],
            session: None,
            model: String::new(),
            status: "Ready".into(),
            opened: true,
            updated: chrono::Utc::now().timestamp(),
            mode: Mode::Builder,
            thinker_project: None,
            memory_revision: None,
            invocation_turn: None,
            context: String::new(),
            instructions_path: None,
            memory_db: None,
            delegation_endpoint: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: i64,
    pub role: String,
    pub body: String,
    pub attachments: Vec<String>,
    pub created: i64,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub project: String,
    pub updated: i64,
    pub excerpt: String,
    pub message_id: Option<i64>,
}

pub struct Store {
    pub(crate) conn: Connection,
    pub path: PathBuf,
}

impl Store {
    // Tests can exercise concurrent database connections without running host recovery.
    #[cfg(test)]
    pub fn tool_connection(path: PathBuf) -> Result<Self> {
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn, path })
    }

    pub fn read_only(path: PathBuf) -> Result<Self> {
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        Ok(Self { conn, path })
    }
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path != Path::new(":memory:") {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS workstreams (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, project TEXT NOT NULL,
                draft TEXT NOT NULL, attachments TEXT NOT NULL, session TEXT,
                model TEXT NOT NULL, status TEXT NOT NULL, opened INTEGER NOT NULL,
                updated INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY, workstream TEXT NOT NULL REFERENCES workstreams(id),
                role TEXT NOT NULL, body TEXT NOT NULL, attachments TEXT NOT NULL, created INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_workstream ON messages(workstream, id);
            CREATE INDEX IF NOT EXISTS workstreams_updated ON workstreams(updated DESC);
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS submissions (
                workstream TEXT PRIMARY KEY REFERENCES workstreams(id), snapshot TEXT NOT NULL);
            UPDATE workstreams SET status='Interrupted' WHERE status IN ('Running', 'Stopping', 'Routing');")?;
        // Recover 'Routing' workstreams left by versions with automatic dispatch.
        // Additive migration: existing conversations and submission snapshots remain Builder.
        let columns = conn
            .prepare("PRAGMA table_info(workstreams)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !columns.iter().any(|column| column == "mode") {
            conn.execute_batch(
                "ALTER TABLE workstreams ADD COLUMN mode TEXT NOT NULL DEFAULT 'builder';",
            )?;
        }
        if !columns.iter().any(|column| column == "thinker_project") {
            conn.execute_batch("ALTER TABLE workstreams ADD COLUMN thinker_project TEXT REFERENCES thinker_projects(id);")?;
        }
        conn.execute_batch("CREATE TABLE IF NOT EXISTS thinker_projects (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, instructions TEXT NOT NULL DEFAULT '', directory TEXT);
            CREATE TABLE IF NOT EXISTS thinker_memories (
            id INTEGER PRIMARY KEY, scope TEXT NOT NULL, key TEXT NOT NULL, content TEXT NOT NULL,
            source TEXT REFERENCES workstreams(id), updated INTEGER NOT NULL, UNIQUE(scope,key));
            CREATE INDEX IF NOT EXISTS thinker_conversations ON workstreams(mode,thinker_project,updated DESC);
            CREATE INDEX IF NOT EXISTS thinker_memory_scope ON thinker_memories(scope,updated DESC);")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS builder_handoffs (
            builder TEXT PRIMARY KEY REFERENCES workstreams(id),
            source TEXT NOT NULL REFERENCES workstreams(id),
            turn TEXT NOT NULL, request TEXT NOT NULL, brief TEXT NOT NULL,
            startup TEXT NOT NULL DEFAULT 'queued', summary TEXT, error TEXT,
            UNIQUE(source,turn,request));
            UPDATE workstreams SET status='Interrupted' WHERE status='Starting';
            UPDATE builder_handoffs SET startup='interrupted',error='Startup was interrupted. Open Builder to inspect and retry the saved request.' WHERE startup='claimed';")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = path.as_os_str().to_os_string();
                sidecar.push(suffix);
                let sidecar = PathBuf::from(sidecar);
                if sidecar.try_exists()? {
                    std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        let store = Self { conn, path };
        store.init_wakes()?;
        Ok(store)
    }

    pub fn save(&self, w: &Workstream) -> Result<()> {
        self.conn.execute("INSERT INTO workstreams (id,title,project,draft,attachments,session,model,status,opened,updated,mode,thinker_project) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
            ON CONFLICT(id) DO UPDATE SET title=excluded.title, project=excluded.project,
            draft=excluded.draft, attachments=excluded.attachments, session=excluded.session,
            model=excluded.model, status=excluded.status, opened=excluded.opened, updated=excluded.updated,
            mode=excluded.mode, thinker_project=excluded.thinker_project",
            params![w.id, w.title, w.project, w.draft, serde_json::to_string(&w.attachments)?,
                w.session, w.model, w.status, w.opened, w.updated, w.mode.stored(), w.thinker_project])?;
        Ok(())
    }

    pub fn save_submission(&self, w: &Workstream) -> Result<()> {
        self.conn.execute("INSERT INTO submissions VALUES (?1,?2) ON CONFLICT(workstream) DO UPDATE SET snapshot=excluded.snapshot",
            params![w.id, serde_json::to_string(w)?])?;
        Ok(())
    }

    pub fn clear_submission(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM submissions WHERE workstream=?1", [id])?;
        Ok(())
    }

    pub fn submission(&self, id: &str) -> Result<Option<Workstream>> {
        use rusqlite::OptionalExtension;
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT snapshot FROM submissions WHERE workstream=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workstream> {
        let attachments: String = row.get(4)?;
        Ok(Workstream {
            id: row.get(0)?,
            title: row.get(1)?,
            project: row.get(2)?,
            draft: row.get(3)?,
            attachments: serde_json::from_str(&attachments).unwrap_or_default(),
            session: row.get(5)?,
            model: row.get(6)?,
            status: row.get(7)?,
            opened: row.get(8)?,
            updated: row.get(9)?,
            mode: if row.get::<_, String>(10)? == "thinker" {
                Mode::Thinker
            } else {
                Mode::Builder
            },
            thinker_project: row.get(11)?,
            memory_revision: None,
            invocation_turn: None,
            context: String::new(),
            instructions_path: None,
            memory_db: None,
            delegation_endpoint: None,
        })
    }

    pub fn opened(&self) -> Result<Vec<Workstream>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM workstreams WHERE opened=1 ORDER BY updated")?;
        let rows = stmt
            .query_map([], Self::row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn load(&self, id: &str) -> Result<Workstream> {
        Ok(self
            .conn
            .query_row("SELECT * FROM workstreams WHERE id=?1", [id], Self::row)?)
    }

    pub fn add_message(
        &self,
        id: &str,
        role: &str,
        body: &str,
        attachments: &[String],
    ) -> Result<Message> {
        let created = chrono::Utc::now().timestamp();
        self.conn.execute("INSERT INTO messages (workstream,role,body,attachments,created) VALUES (?1,?2,?3,?4,?5)",
            params![id, role, body, serde_json::to_string(attachments)?, created])?;
        Ok(Message {
            id: self.conn.last_insert_rowid(),
            role: role.into(),
            body: body.into(),
            attachments: attachments.to_vec(),
            created,
        })
    }

    pub fn messages(&self, id: &str, before: Option<i64>) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,role,body,attachments,created FROM messages
            WHERE workstream=?1 AND id < ?2 ORDER BY id DESC LIMIT 100",
        )?;
        let mut rows = stmt
            .query_map(params![id, before.unwrap_or(i64::MAX)], |row| {
                let a: String = row.get(3)?;
                Ok(Message {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    body: row.get(2)?,
                    attachments: serde_json::from_str(&a).unwrap_or_default(),
                    created: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    // Query in SQLite rather than loading every conversation into memory. Substring search
    // deliberately supports paths, identifiers and fragments as well as whole words.
    pub fn search(&self, query: &str, project: Option<&str>, since: i64) -> Result<Vec<SearchHit>> {
        self.search_mode(query, project, since, Mode::Builder, None)
    }

    pub fn search_mode(
        &self,
        query: &str,
        project: Option<&str>,
        since: i64,
        mode: Mode,
        thinker_project: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare("SELECT w.id,w.title,w.project,w.updated,
            COALESCE((SELECT substr(m.body, max(1,instr(lower(m.body),lower(?1))-70),240)
                FROM messages m WHERE m.workstream=w.id AND instr(lower(m.body),lower(?1))>0
                ORDER BY m.id DESC LIMIT 1),substr(w.draft,1,240)),
            (SELECT m.id FROM messages m WHERE m.workstream=w.id AND ?1!=''
                AND instr(lower(m.body),lower(?1))>0 ORDER BY m.id DESC LIMIT 1)
            FROM workstreams w WHERE (?2 IS NULL OR w.project=?2) AND w.updated>=?3 AND w.mode=?4
            AND (?5 IS NULL OR COALESCE(w.thinker_project,'')=?5)
            AND (?1='' OR instr(lower(w.title),lower(?1))>0 OR instr(lower(w.draft),lower(?1))>0
                OR EXISTS (SELECT 1 FROM messages m WHERE m.workstream=w.id AND instr(lower(m.body),lower(?1))>0))
            ORDER BY w.updated DESC LIMIT 60")?;
        let rows = stmt
            .query_map(
                params![query, project, since, mode.stored(), thinker_project],
                |r| {
                    Ok(SearchHit {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        project: r.get(2)?,
                        updated: r.get(3)?,
                        excerpt: r.get(4)?,
                        message_id: r.get(5)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn projects(&self) -> Result<Vec<ThinkerProject>> {
        Ok(self.conn.prepare("SELECT id,name,instructions,directory FROM thinker_projects ORDER BY name COLLATE NOCASE")?
            .query_map([], |r| Ok(ThinkerProject { id: r.get(0)?, name: r.get(1)?, instructions: r.get(2)?, directory: r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_project(&self, project: &ThinkerProject) -> Result<()> {
        if project.name.trim().is_empty() {
            return Err("A project needs a name.".into());
        }
        self.conn.execute("INSERT INTO thinker_projects VALUES (?1,?2,?3,?4)
            ON CONFLICT(id) DO UPDATE SET name=excluded.name,instructions=excluded.instructions,directory=excluded.directory",
            params![project.id, project.name.trim(), project.instructions, project.directory])?;
        Ok(())
    }

    pub fn move_conversation(&self, id: &str, project: Option<&str>) -> Result<()> {
        let changed = self.conn.execute("UPDATE workstreams SET thinker_project=?2,session=NULL,updated=?3 WHERE id=?1 AND mode='thinker'",
            params![id, project, chrono::Utc::now().timestamp()])?;
        if changed == 0 {
            return Err("Only Thinker conversations can be moved into a Thinker project.".into());
        }
        Ok(())
    }

    pub fn memories(&self, scope: &str) -> Result<Vec<Memory>> {
        Ok(self.conn.prepare("SELECT id,scope,key,content,source FROM thinker_memories WHERE scope=?1 ORDER BY updated DESC,id DESC")?
            .query_map([scope], |r| Ok(Memory { id: r.get(0)?, scope: r.get(1)?, key: r.get(2)?, content: r.get(3)?, source: r.get(4)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn remember(
        &self,
        scope: &str,
        key: &str,
        content: &str,
        source: Option<&str>,
    ) -> Result<()> {
        let key = key.trim().to_lowercase();
        let content = content.trim();
        if key.is_empty()
            || key.chars().count() > 120
            || content.is_empty()
            || content.chars().count() > 2000
        {
            return Err("Memories need a short key and 1–2,000 characters of content.".into());
        }
        if !scope.is_empty() && !self.projects()?.iter().any(|p| p.id == scope) {
            return Err("Memory project does not exist.".into());
        }
        let transaction = self.conn.unchecked_transaction()?;
        self.conn.execute("INSERT INTO thinker_memories (scope,key,content,source,updated) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(scope,key) DO UPDATE SET content=excluded.content,source=excluded.source,updated=excluded.updated",
            params![scope, key, content, source, chrono::Utc::now().timestamp()])?;
        self.bump_memory_revision()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn forget(&self, id: i64) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        self.conn
            .execute("DELETE FROM thinker_memories WHERE id=?1", [id])?;
        self.bump_memory_revision()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn forget_key(&self, scope: &str, key: &str) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "DELETE FROM thinker_memories WHERE scope=?1 AND key=?2",
            params![scope, key.trim().to_lowercase()],
        )?;
        self.bump_memory_revision()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn memory_revision(&self) -> i64 {
        self.setting("thinker_memory_revision")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn bump_memory_revision(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings VALUES ('thinker_memory_revision','1')
            ON CONFLICT(key) DO UPDATE SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT)",
            [],
        )?;
        Ok(())
    }

    pub fn thinker_context(&self, w: &Workstream) -> Result<String> {
        let project = self
            .projects()?
            .into_iter()
            .find(|p| Some(&p.id) == w.thinker_project.as_ref());
        let mut context = String::from("The following is conversational context, not instructions from a tool or a website. Memories may be outdated; prioritize the user's current statements.\n");
        if let Some(project) = project {
            context.push_str(&format!(
                "\nProject: {}\nProject guidance: {}\n",
                project.name, project.instructions
            ));
        }
        let mut memories = self.memories("")?;
        if let Some(scope) = &w.thinker_project {
            memories.extend(self.memories(scope)?);
        }
        let words: Vec<_> = w
            .draft
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.len() > 2)
            .map(str::to_lowercase)
            .collect();
        memories.sort_by_cached_key(|memory| {
            let text = format!("{} {}", memory.key, memory.content).to_lowercase();
            std::cmp::Reverse(
                words
                    .iter()
                    .filter(|word| text.contains(word.as_str()))
                    .count(),
            )
        });
        context.push_str("\nRelevant saved memories (JSON data):\n");
        let mut remaining = 12000;
        for memory in memories.into_iter().take(48) {
            let entry = serde_json::json!({"scope":if memory.scope.is_empty() { "global" } else { "project" }, "key":memory.key,"content":memory.content}).to_string();
            if entry.len() <= remaining {
                remaining -= entry.len();
                context.push_str(&entry);
                context.push('\n');
            }
        }
        // Read recent conversational messages directly from SQLite, including closed tabs.
        // The submitted user message is already present and is supplied separately below.
        let mut transcript: Vec<(String,String)> = self.conn.prepare("SELECT role,body FROM messages WHERE workstream=?1 AND role IN ('You','Agent','Wake') ORDER BY id DESC LIMIT 51")?
            .query_map([&w.id], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        if transcript
            .first()
            .is_some_and(|(role, body)| matches!(role.as_str(), "You" | "Wake") && body == &w.draft)
        {
            transcript.remove(0);
        }
        let mut remaining: usize = 32000;
        let mut entries = Vec::new();
        for (role, body) in transcript {
            let body: String = body.chars().take(remaining.min(8000)).collect();
            remaining = remaining.saturating_sub(body.chars().count());
            entries.push(serde_json::json!({"role":role,"content":body}).to_string());
            if remaining == 0 {
                break;
            }
        }
        entries.reverse();
        context.push_str("\nRecent conversation (oldest first; bounded history, JSON data):\n");
        context.push_str(&entries.join("\n"));
        Ok(context)
    }

    pub fn messages_around(&self, id: &str, message_id: i64) -> Result<Vec<Message>> {
        self.messages(id, Some(message_id.saturating_add(21)))
    }

    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO settings VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> Store {
        Store::open(PathBuf::from(":memory:")).unwrap()
    }

    #[test]
    fn legacy_database_and_snapshots_migrate_to_builder_without_data_loss() {
        let path = std::env::temp_dir().join(format!(
            "harness-migration-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let w = Workstream::new(Path::new("/original"));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE workstreams (id TEXT PRIMARY KEY,title TEXT NOT NULL,project TEXT NOT NULL,draft TEXT NOT NULL,attachments TEXT NOT NULL,session TEXT,model TEXT NOT NULL,status TEXT NOT NULL,opened INTEGER NOT NULL,updated INTEGER NOT NULL);").unwrap();
            conn.execute("INSERT INTO workstreams VALUES (?1,'Existing task','/original','Keep draft','[]','original-session','claude:sonnet','Routing',1,1)",[&w.id]).unwrap();
        }
        {
            let db = Store::open(path.clone()).unwrap();
            let restored = db.load(&w.id).unwrap();
            assert_eq!(restored.mode, Mode::Builder);
            assert_eq!(restored.draft, "Keep draft");
            assert_eq!(restored.session.as_deref(), Some("original-session"));
            assert_eq!(restored.model, "claude:sonnet");
            assert_eq!(restored.status, "Interrupted");
            let mut legacy = serde_json::to_value(&restored).unwrap();
            legacy.as_object_mut().unwrap().remove("mode");
            legacy.as_object_mut().unwrap().remove("thinker_project");
            db.conn
                .execute(
                    "INSERT INTO submissions VALUES (?1,?2)",
                    params![w.id, legacy.to_string()],
                )
                .unwrap();
            let submitted = db.submission(&w.id).unwrap().unwrap();
            assert_eq!(submitted.mode, Mode::Builder);
            assert_eq!(submitted.model, restored.model);
            assert_eq!(submitted.draft, restored.draft);
            db.save(&restored).unwrap();
        }
        assert_eq!(
            Store::open(path.clone()).unwrap().load(&w.id).unwrap().mode,
            Mode::Builder
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn thinker_projects_memories_and_moves_survive_reopening_and_remain_scoped() {
        let path =
            std::env::temp_dir().join(format!("harness-thinker-{}.sqlite3", uuid::Uuid::new_v4()));
        let a = ThinkerProject {
            id: "a".into(),
            name: "Research".into(),
            instructions: "Compare evidence".into(),
            directory: None,
        };
        let b = ThinkerProject {
            id: "b".into(),
            name: "Planning".into(),
            instructions: String::new(),
            directory: None,
        };
        let mut w = Workstream::new(Path::new("/tmp"));
        w.mode = Mode::Thinker;
        w.thinker_project = Some(a.id.clone());
        let builder = Workstream::new(Path::new("/tmp"));
        {
            let db = Store::open(path.clone()).unwrap();
            db.save_project(&a).unwrap();
            db.save_project(&b).unwrap();
            db.save(&w).unwrap();
            db.save(&builder).unwrap();
            db.add_message(&w.id, "You", "We chose the smaller launch.", &[])
                .unwrap();
            db.remember("", "Answer style", "Prefer brief answers", Some(&w.id))
                .unwrap();
            db.remember(
                "a",
                "Launch decision",
                "Start with a small pilot",
                Some(&w.id),
            )
            .unwrap();
            db.remember(
                "b",
                "Private planning fact",
                "Different project context",
                None,
            )
            .unwrap();
            db.remember("", "answer style", "Prefer concise answers", Some(&w.id))
                .unwrap();
            assert_eq!(db.memories("").unwrap().len(), 1);
        }
        {
            let db = Store::open(path.clone()).unwrap();
            let restored = db.load(&w.id).unwrap();
            let context = db.thinker_context(&restored).unwrap();
            for text in [
                "Compare evidence",
                "Prefer concise answers",
                "small pilot",
                "smaller launch",
            ] {
                assert!(context.contains(text));
            }
            assert!(!context.contains("Different project context"));
            assert_eq!(db.search("", None, 0).unwrap().len(), 1);
            db.move_conversation(&w.id, Some("b")).unwrap();
            let moved = db.load(&w.id).unwrap();
            assert_eq!(moved.thinker_project.as_deref(), Some("b"));
            assert!(moved.session.is_none());
            let context = db.thinker_context(&moved).unwrap();
            assert!(context.contains("Different project context"));
            assert!(!context.contains("small pilot"));
            assert!(db.move_conversation(&builder.id, Some("b")).is_err());
            assert!(db.move_conversation(&w.id, Some("missing")).is_err());
            let read_only = Store::read_only(path.clone()).unwrap();
            assert_eq!(read_only.projects().unwrap().len(), 2);
            assert!(read_only.remember("", "forbidden", "write", None).is_err());
            db.forget_key("", "answer style").unwrap();
            assert!(!db
                .thinker_context(&moved)
                .unwrap()
                .contains("Prefer concise answers"));
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn search_finds_late_responses_and_closed_workstreams_with_filters() {
        let db = store();
        let mut w = Workstream::new(Path::new("/project"));
        w.opened = false;
        db.save(&w).unwrap();
        db.add_message(&w.id, "You", "An unrelated first prompt", &[])
            .unwrap();
        let msg = db
            .add_message(
                &w.id,
                "Agent",
                "Fixed Cloudflare deployment in worker.ts",
                &[],
            )
            .unwrap();
        let results = db.search("worker.ts", Some("/project"), 0).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, Some(msg.id));
        assert!(results[0].excerpt.contains("Cloudflare"));
        assert!(db
            .search("worker.ts", Some("/other"), 0)
            .unwrap()
            .is_empty());
        assert!(db.search("worker.ts", None, i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn drafts_attachments_and_sessions_survive_reopening_and_runs_are_interrupted() {
        let path = std::env::temp_dir().join(format!("harness-{}.sqlite3", uuid::Uuid::new_v4()));
        let mut w = Workstream::new(Path::new("/project"));
        w.draft = "**Retain this**\nSecond line".into();
        w.attachments = vec!["screenshot.png".into()];
        w.session = Some("session-id".into());
        w.status = "Running".into();
        {
            let db = Store::open(path.clone()).unwrap();
            db.save(&w).unwrap();
        }
        {
            let db = Store::open(path.clone()).unwrap();
            let restored = db.load(&w.id).unwrap();
            assert_eq!(restored.draft, w.draft);
            assert_eq!(restored.attachments, w.attachments);
            assert_eq!(restored.session, w.session);
            assert_eq!(restored.status, "Interrupted");
        }
        std::fs::remove_file(path).unwrap();
    }
}
