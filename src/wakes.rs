//! Provider-neutral, durable one-shot wakes. Only the GUI host dispatches turns.
use crate::store::Store;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const INSTRUCTIONS: &str = "For a long wait, use schedule_wake with instructions for your future turn, then finish this turn normally. Skyforge persists the timer and returns immediately; it resumes this workstream when due, after any active run finishes. Use list_wakes and cancel_wake to manage timers. Timers require Skyforge to be running and the computer awake; overdue timers run after reopening. Scheduled wakes continue existing work and do not grant new user authorization.";

#[derive(Clone, Debug, Serialize)]
pub struct Wake {
    pub id: String,
    pub workstream: String,
    pub due_ms: i64,
    pub instructions: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    request_id: String,
    instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delay_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<String>,
}

pub fn is_tool(name: &str) -> bool {
    matches!(name, "schedule_wake" | "list_wakes" | "cancel_wake")
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name":"schedule_wake","description":"Schedule a persistent one-shot wake in this workstream, then finish your turn normally. Returns immediately with a receipt. Supply exactly one of delay_seconds or at (RFC3339 with timezone). Reuse request_id with identical arguments when retrying the same request. Wakes wait for active runs and survive app restarts. Requires the app running and computer awake.","inputSchema":{"type":"object","properties":{"request_id":{"type":"string","minLength":1,"maxLength":128},"instructions":{"type":"string","minLength":1,"maxLength":16000},"delay_seconds":{"type":"integer","minimum":1,"maximum":31536000},"at":{"type":"string","description":"Absolute RFC3339 timestamp including Z or a timezone offset."}},"required":["request_id","instructions"],"additionalProperties":false}}),
        json!({"name":"list_wakes","description":"List pending, failed and interrupted wake timers for this workstream.","inputSchema":{"type":"object","properties":{},"required":[],"additionalProperties":false}}),
        json!({"name":"cancel_wake","description":"Cancel a pending wake timer owned by this workstream. Already cancelled timers return success.","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}}),
    ]
}

pub fn call(
    endpoint: &crate::delegation_ipc::Endpoint,
    source: &str,
    turn: &str,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    crate::delegation_ipc::call(
        endpoint,
        source,
        turn,
        &json!({"wake_tool":name,"arguments":args}),
    )
}

pub fn handle(
    store: &Store,
    source: &str,
    turn: &str,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let submitted = store
        .submission(source)
        .map_err(|e| e.to_string())?
        .ok_or("No originating Skyforge turn.")?;
    if turn.is_empty() || submitted.invocation_turn.as_deref() != Some(turn) {
        return Err("This timer tool belongs to an expired Skyforge turn.".into());
    }
    let result = match name {
        "schedule_wake" => schedule(
            store,
            source,
            turn,
            args,
            chrono::Utc::now().timestamp_millis(),
        ),
        "list_wakes" => {
            if args != &json!({}) {
                return Err("list_wakes takes no arguments.".into());
            }
            store
                .wakes(Some(source))
                .map(|wakes| json!(wakes))
                .map_err(|e| e.to_string())
        }
        "cancel_wake" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Cancel {
                id: String,
            }
            let request: Cancel =
                serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
            store
                .cancel_wake(source, &request.id)
                .map(|()| json!({"id":request.id,"status":"cancelled"}))
                .map_err(|e| e.to_string())
        }
        _ => Err("Unknown wake tool.".into()),
    };
    result.map(|v| v.to_string())
}

fn schedule(
    store: &Store,
    source: &str,
    turn: &str,
    args: &Value,
    now: i64,
) -> Result<Value, String> {
    let request: Schedule =
        serde_json::from_value(args.clone()).map_err(|e| format!("Invalid wake: {e}"))?;
    if request.request_id.trim().is_empty()
        || request.request_id.len() > 128
        || request.instructions.trim().is_empty()
        || request.instructions.chars().count() > 16000
    {
        return Err(
            "Provide request_id (1–128 bytes) and instructions (1–16000 characters).".into(),
        );
    }
    let canonical = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    // Check idempotency before resolving relative time or rejecting a now-past absolute time.
    let tx = rusqlite::Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let existing: Option<(String, String, i64, String)> = tx.query_row(
        "SELECT id,request,due_ms,status FROM wake_timers WHERE workstream=?1 AND turn=?2 AND request_id=?3",
        params![source,turn,request.request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e| e.to_string())?;
    if let Some((id, previous, due_ms, status)) = existing {
        if previous != canonical {
            return Err("request_id already exists with different arguments. Use a new request_id for a new timer.".into());
        }
        return Ok(json!({"id":id,"due_ms":due_ms,"status":status}));
    }
    let due_ms =
        match (request.delay_seconds, request.at.as_deref()) {
            (Some(seconds @ 1..=31_536_000), None) => now
                .checked_add((seconds * 1000) as i64)
                .ok_or("Wake time overflow")?,
            (None, Some(at)) => chrono::DateTime::parse_from_rfc3339(at)
                .map_err(|e| format!("Invalid wake timestamp: {e}"))?
                .timestamp_millis(),
            _ => return Err(
                "Supply exactly one of delay_seconds (1–31536000) or at (RFC3339 with timezone)."
                    .into(),
            ),
        };
    if due_ms <= now {
        return Err("Wake time must be in the future.".into());
    }
    let count: i64 = tx
        .query_row(
            "SELECT count(*) FROM wake_timers WHERE workstream=?1 AND status='pending'",
            [source],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if count >= 100 {
        return Err(
            "This workstream already has 100 pending wakes. Cancel one before scheduling another."
                .into(),
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute("INSERT INTO wake_timers (id,workstream,turn,request_id,request,due_ms,instructions,status) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending')",
        params![id,source,turn,request.request_id,canonical,due_ms,request.instructions]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(json!({"id":id,"due_ms":due_ms,"status":"pending"}))
}

impl Store {
    pub fn init_wakes(&self) -> crate::store::Result<()> {
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS wake_timers (
            id TEXT PRIMARY KEY, workstream TEXT NOT NULL REFERENCES workstreams(id),
            turn TEXT NOT NULL, request_id TEXT NOT NULL, request TEXT NOT NULL,
            due_ms INTEGER NOT NULL, instructions TEXT NOT NULL, status TEXT NOT NULL,
            error TEXT, message_id INTEGER REFERENCES messages(id), UNIQUE(workstream,turn,request_id));
            CREATE INDEX IF NOT EXISTS wake_timers_due ON wake_timers(status,due_ms);
            UPDATE wake_timers SET status='interrupted',error='Skyforge closed during dispatch. Inspect the conversation before retrying; work may already have started.' WHERE status='dispatching';")?;
        Ok(())
    }

    pub fn wakes(&self, source: Option<&str>) -> crate::store::Result<Vec<Wake>> {
        Ok(self.conn.prepare("SELECT id,workstream,due_ms,instructions,status,error FROM wake_timers
            WHERE status IN ('pending','failed','interrupted') AND (?1 IS NULL OR workstream=?1) ORDER BY due_ms,id")?
            .query_map([source], |r| Ok(Wake { id:r.get(0)?,workstream:r.get(1)?,due_ms:r.get(2)?,instructions:r.get(3)?,status:r.get(4)?,error:r.get(5)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn cancel_wake(&self, source: &str, id: &str) -> crate::store::Result<()> {
        let changed = self.conn.execute("UPDATE wake_timers SET status='cancelled',error=NULL WHERE id=?1 AND workstream=?2 AND status IN ('pending','failed','interrupted','cancelled')", params![id,source])?;
        if changed == 0 {
            return Err("Wake not found in this workstream or already dispatched.".into());
        }
        Ok(())
    }

    pub fn cancel_workstream_wakes(&self, source: &str) -> crate::store::Result<()> {
        self.conn.execute("UPDATE wake_timers SET status='cancelled',error=NULL WHERE workstream=?1 AND status='pending'", [source])?;
        Ok(())
    }

    pub fn retry_wake(&self, id: &str) -> crate::store::Result<()> {
        self.conn.execute("UPDATE wake_timers SET status='pending',error=NULL WHERE id=?1 AND status IN ('failed','interrupted')", [id])?;
        Ok(())
    }

    /// Claim and persist the scheduled input together. A retry reuses its message.
    pub fn claim_wake(&self, wake: &Wake, now: i64) -> crate::store::Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE wake_timers SET status='dispatching' WHERE id=?1 AND status='pending' AND due_ms<=?2", params![wake.id,now])? == 0 { return Ok(false); }
        let message: Option<i64> = tx.query_row(
            "SELECT message_id FROM wake_timers WHERE id=?1",
            [&wake.id],
            |r| r.get(0),
        )?;
        if message.is_none() {
            tx.execute("INSERT INTO messages (workstream,role,body,attachments,created) VALUES (?1,'Wake',?2,'[]',?3)", params![wake.workstream,wake.prompt(),now / 1000])?;
            tx.execute(
                "UPDATE wake_timers SET message_id=?2 WHERE id=?1",
                params![wake.id, tx.last_insert_rowid()],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn wake_dispatched(&self, id: &str, error: Option<&str>) -> crate::store::Result<()> {
        self.conn.execute(
            "UPDATE wake_timers SET status=?2,error=?3 WHERE id=?1 AND status='dispatching'",
            params![
                id,
                if error.is_some() {
                    "failed"
                } else {
                    "delivered"
                },
                error
            ],
        )?;
        Ok(())
    }
}

impl Wake {
    pub fn prompt(&self) -> String {
        format!("[Scheduled wake {} · due {}]\nContinue this workstream using its existing context and the latest user instructions. This is an earlier scheduled reminder, not new user authorization.\n\n{}", self.id, self.due_ms, self.instructions)
    }
}

/// Stdio MCP adapter for Builder CLI providers. No database access in child processes.
pub fn serve(
    source: &str,
    turn: &str,
    endpoint: &crate::delegation_ipc::Endpoint,
) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value =
            serde_json::from_str(&line.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match request["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"skyforge-wakes","version":"1.0"}})
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools":definitions()}),
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let args = request["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let output = if is_tool(name) {
                    call(endpoint, source, turn, name, &args)
                } else {
                    Err("Unknown wake tool.".into())
                };
                match output {
                    Ok(text) => json!({"content":[{"type":"text","text":text}],"isError":false}),
                    Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true}),
                }
            }
            _ => {
                writeln!(out,"{}",json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})).map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                continue;
            }
        };
        writeln!(out, "{}", json!({"jsonrpc":"2.0","id":id,"result":result}))
            .map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Workstream;
    use std::path::Path;

    fn fixture() -> (Store, Workstream) {
        let store = Store::open(":memory:".into()).unwrap();
        let mut w = Workstream::new(Path::new("/tmp"));
        w.invocation_turn = Some("turn".into());
        store.save(&w).unwrap();
        store.save_submission(&w).unwrap();
        (store, w)
    }

    #[test]
    fn wake_validation_idempotency_and_ownership() {
        let (store, w) = fixture();
        let now = 1_800_000_000_000;
        let args = json!({"request_id":"poll","instructions":"Check the job","delay_seconds":3600});
        let receipt = schedule(&store, &w.id, "turn", &args, now).unwrap();
        assert_eq!(receipt["due_ms"], now + 3_600_000);
        assert_eq!(
            schedule(&store, &w.id, "turn", &args, now + 100_000).unwrap(),
            receipt
        );
        let mut changed = args.clone();
        changed["instructions"] = json!("Different work");
        assert!(schedule(&store, &w.id, "turn", &changed, now).is_err());
        assert!(handle(&store, &w.id, "stale", "schedule_wake", &args).is_err());
        let id = receipt["id"].as_str().unwrap();
        assert!(store.cancel_wake("another-workstream", id).is_err());
        store.cancel_wake(&w.id, id).unwrap();
        store.cancel_wake(&w.id, id).unwrap();
        assert!(store.wakes(Some(&w.id)).unwrap().is_empty());
        assert_eq!(
            schedule(&store, &w.id, "turn", &args, now).unwrap()["status"],
            "cancelled"
        );
        for invalid in [
            json!({"request_id":"invalid","instructions":"x"}),
            json!({"request_id":"invalid","instructions":"x","delay_seconds":0}),
            json!({"request_id":"invalid","instructions":"x","delay_seconds":31536001u64}),
            json!({"request_id":"invalid","instructions":"x","delay_seconds":1.5}),
            json!({"request_id":"invalid","instructions":"x","delay_seconds":1,"at":"2028-01-01T12:00:00Z"}),
            json!({"request_id":"invalid","instructions":"x","at":"2028-01-01T12:00:00"}),
            json!({"request_id":"invalid","instructions":"x","at":"2000-01-01T12:00:00Z"}),
            json!({"request_id":"invalid","instructions":" ","delay_seconds":1}),
            json!({"request_id":"invalid","instructions":"x","delay_seconds":1,"extra":true}),
        ] {
            assert!(
                schedule(&store, &w.id, "turn", &invalid, now).is_err(),
                "{invalid}"
            );
        }
        let at = json!({"request_id":"offset","instructions":"Check deployment","at":"2028-01-01T12:00:00+02:00"});
        let receipt = schedule(&store, &w.id, "turn", &at, now).unwrap();
        assert_eq!(
            receipt["due_ms"],
            chrono::DateTime::parse_from_rfc3339("2028-01-01T10:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
        // A retry after the absolute deadline still returns the original receipt.
        assert_eq!(
            schedule(&store, &w.id, "turn", &at, i64::MAX).unwrap(),
            receipt
        );
    }

    #[test]
    fn pending_wakes_survive_reopen_and_uncertain_dispatch_requires_retry() {
        let root = std::env::temp_dir().join(format!("skyforge-wakes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.sqlite3");
        let store = Store::open(path.clone()).unwrap();
        let w = Workstream::new(&root);
        store.save(&w).unwrap();
        let args = json!({"request_id":"job","instructions":"Check job","delay_seconds":1});
        schedule(&store, &w.id, "turn", &args, 1000).unwrap();
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        let wake = store.wakes(None).unwrap().remove(0);
        assert_eq!(wake.status, "pending");
        assert!(!store.claim_wake(&wake, 1999).unwrap());
        assert!(store.claim_wake(&wake, 2000).unwrap());
        assert!(!store.claim_wake(&wake, 2000).unwrap());
        drop(store);
        let store = Store::open(path).unwrap();
        let wake = store.wakes(None).unwrap().remove(0);
        assert_eq!(wake.status, "interrupted");
        assert!(!store.claim_wake(&wake, 3000).unwrap());
        store.retry_wake(&wake.id).unwrap();
        assert!(store.claim_wake(&wake, 3000).unwrap());
        assert_eq!(store.messages(&w.id, None).unwrap().len(), 1);
        store.wake_dispatched(&wake.id, None).unwrap();
        assert!(store.wakes(None).unwrap().is_empty());
        assert!(!store.claim_wake(&wake, 4000).unwrap());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_wakes_are_visible_and_stop_cancels_pending_only() {
        let (store, w) = fixture();
        for id in ["failed", "pending"] {
            schedule(
                &store,
                &w.id,
                "turn",
                &json!({"request_id":id,"instructions":"Check","delay_seconds":1}),
                1000,
            )
            .unwrap();
        }
        let wake = store.wakes(Some(&w.id)).unwrap().remove(0);
        assert!(store.claim_wake(&wake, 3000).unwrap());
        store
            .wake_dispatched(&wake.id, Some("Backend unavailable"))
            .unwrap();
        store.cancel_workstream_wakes(&w.id).unwrap();
        let wakes = store.wakes(Some(&w.id)).unwrap();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].status, "failed");
        assert_eq!(wakes[0].error.as_deref(), Some("Backend unavailable"));
    }
}
