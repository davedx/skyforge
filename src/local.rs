//! OpenAI-compatible local agent: chat completions plus workspace tools.
use crate::{
    runner::{self, AgentConfig, Envelope, Event, RunHandle},
    store::Workstream,
};
use base64::Engine;
use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

// Keep the original ID readable for saved workstreams.
#[path = "local_compaction.rs"]
mod compaction;

pub const MODEL_ID: &str = "local:qwen";
const MODEL_PREFIX: &str = "local:model:";
// Bound every tool result before it reaches the transcript or the next request.
const TOOL_OUTPUT_LIMIT: usize = 12_000;

fn tool_output(text: &str) -> String {
    if text.len() <= TOOL_OUTPUT_LIMIT {
        return text.into();
    }
    let retained = TOOL_OUTPUT_LIMIT - 256;
    let mut head = retained * 3 / 4;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - retained / 4;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[Output truncated: {} bytes omitted. Showing beginning and end; use a targeted command or read_file range for more.]\n{}",
        &text[..head], tail - head, &text[tail..]
    )
}

fn tool_message(name: &str, arguments: &str, output: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or_default();
    if name == "run_command" {
        if let Some(command) = args["command"].as_str() {
            return format!("{name}\n$ {command}\n{output}");
        }
    }
    format!("{name}\n{output}")
}

pub fn is_local(model: &str) -> bool {
    model == MODEL_ID || model.starts_with(MODEL_PREFIX)
}

pub fn selection_id(model: &str) -> String {
    format!("{MODEL_PREFIX}{model}")
}

pub fn canonical_selection(config: &Config, selection: &str) -> String {
    if selection == MODEL_ID && !config.model.trim().is_empty() {
        selection_id(config.model.trim())
    } else {
        selection.to_owned()
    }
}

pub fn selected_config(config: &Config, selection: &str) -> Config {
    let mut config = config.clone();
    if let Some(model) = selection.strip_prefix(MODEL_PREFIX) {
        config.model = model.into();
    }
    config.model = config.model.trim().into();
    config
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplay {
    #[default]
    Auto,
    Omit,
    CurrentTurn,
    PreserveHistory,
}

impl ReasoningReplay {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Omit => "Omit",
            Self::CurrentTurn => "Current turn",
            Self::PreserveHistory => "Preserve history",
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ModelCapabilities {
    pub max_output_tokens: Option<usize>,
    pub reasoning: Option<bool>,
}

pub struct ModelCatalog {
    pub ids: Vec<String>,
    pub capabilities: std::collections::BTreeMap<String, ModelCapabilities>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Config {
    pub host: String,
    pub api_key: String,
    pub model: String,
    /// Context windows in tokens, keyed by exact served model ID.
    #[serde(default)]
    pub context_windows: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub output_limits: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub model_capabilities: std::collections::BTreeMap<String, ModelCapabilities>,
    /// Overrides keyed by normalized host URL and exact served model ID.
    #[serde(default)]
    pub reasoning_replay: std::collections::BTreeMap<String, ReasoningReplay>,
    #[serde(skip)]
    pub history_dir: PathBuf,
    #[serde(skip)]
    pub available_models: Vec<String>,
    #[serde(skip)]
    pub retry: bool,
}

impl Config {
    fn is_deepseek(&self) -> bool {
        self.model.to_lowercase().contains("deepseek")
            || url::Url::parse(self.host.trim())
                .ok()
                .as_ref()
                .and_then(|url| url.host_str())
                == Some("api.deepseek.com")
    }

    pub fn replay_key(&self) -> String {
        self.model_key(&self.model)
    }

    fn model_key(&self, model: &str) -> String {
        let host = url::Url::parse(self.host.trim())
            .map(|url| url.to_string())
            .unwrap_or_else(|_| self.host.trim().to_owned());
        format!("{}|{}", host.trim_end_matches('/'), model.trim())
    }

    pub fn replay_policy(&self) -> ReasoningReplay {
        let policy = self
            .reasoning_replay
            .get(&self.replay_key())
            .copied()
            .unwrap_or_default();
        if policy != ReasoningReplay::Auto {
            return policy;
        }
        let model = self.model.to_lowercase();
        let host = url::Url::parse(self.host.trim()).ok();
        let host = host.as_ref().and_then(|url| url.host_str()).unwrap_or("");
        if host == "api.deepseek.com"
            || host == "api.mistral.ai"
            || model.contains("deepseek")
            || model.contains("magistral")
            || ["qwen3.6", "qwen3.7", "qwen3.8"]
                .iter()
                .any(|name| model.contains(name))
        {
            ReasoningReplay::PreserveHistory
        } else if model.contains("qwen3") {
            ReasoningReplay::CurrentTurn
        } else {
            ReasoningReplay::Omit
        }
    }

    pub fn context_window(&self) -> usize {
        self.context_windows
            .get(&self.model)
            .copied()
            .unwrap_or(32_768)
            .clamp(8192, 2_000_000)
    }

    pub fn output_limit(&self) -> usize {
        let capabilities = self.model_capabilities.get(&self.replay_key());
        let supported = capabilities
            .and_then(|c| c.max_output_tokens)
            .filter(|limit| *limit > 0);
        let requested = self
            .output_limits
            .get(&self.model)
            .copied()
            .unwrap_or_else(|| {
                let model = self.model.to_lowercase();
                let reasoning = capabilities.and_then(|c| c.reasoning).unwrap_or_else(|| {
                    self.replay_policy() != ReasoningReplay::Omit
                        || model.contains("reason")
                        || model.contains("thinking")
                        || model.contains("gpt-oss")
                });
                // Automatic budgets leave substantial input room even on small local windows.
                supported
                    .unwrap_or(if reasoning { 32_768 } else { 16_384 })
                    .min(self.context_window() / 2)
            });
        requested
            .min(supported.unwrap_or(usize::MAX))
            .clamp(1, self.context_window().saturating_sub(512 + 1024))
    }

    pub fn apply_catalog(&mut self, catalog: ModelCatalog) {
        let prefix = self.model_key("");
        self.model_capabilities
            .retain(|key, _| !key.starts_with(&prefix));
        for (model, capabilities) in catalog.capabilities {
            self.model_capabilities
                .insert(self.model_key(&model), capabilities);
        }
        self.available_models = catalog.ids;
    }
}

fn completion_payload(config: &Config, messages: Vec<Value>, tools: &[Value]) -> Value {
    let mut payload = json!({"model":config.model,"messages":messages,"tools":tools,"tool_choice":"auto","stream":false,"max_tokens":config.output_limit()});
    let model = config.model.to_lowercase();
    let policy = config.replay_policy();
    let url = url::Url::parse(config.host.trim()).ok();
    let host = url.as_ref().and_then(|url| url.host_str()).unwrap_or("");
    if ["qwen3.6", "qwen3.7", "qwen3.8"]
        .iter()
        .any(|name| model.contains(name))
    {
        let preserve = policy == ReasoningReplay::PreserveHistory;
        if host == "dashscope.aliyuncs.com"
            || host.ends_with(".aliyuncs.com")
            || host.ends_with(".qwencloud.com")
        {
            payload["preserve_thinking"] = json!(preserve);
        } else {
            payload["chat_template_kwargs"] = json!({"preserve_thinking":preserve});
        }
    }
    if host == "api.deepseek.com" && policy == ReasoningReplay::Omit {
        payload["thinking"] = json!({"type":"disabled"});
    }
    payload
}

pub fn load(store: &crate::store::Store, project: &Path) -> Config {
    // A private, ignored bootstrap file lets the GUI import supplied credentials.
    let seed_path = project.join(".harness/local-model.json");
    if let Ok(bytes) = std::fs::read(&seed_path) {
        if let Ok(seed) = serde_json::from_slice::<Config>(&bytes) {
            let saved = [
                ("local_host", seed.host),
                ("local_api_key", seed.api_key),
                ("local_model", seed.model),
                (
                    "local_context_windows",
                    serde_json::to_string(&seed.context_windows).unwrap_or_default(),
                ),
                (
                    "local_output_limits",
                    serde_json::to_string(&seed.output_limits).unwrap_or_default(),
                ),
                (
                    "local_model_capabilities",
                    serde_json::to_string(&seed.model_capabilities).unwrap_or_default(),
                ),
                (
                    "local_reasoning_replay",
                    serde_json::to_string(&seed.reasoning_replay).unwrap_or_default(),
                ),
            ]
            .iter()
            .all(|(k, v)| store.set_setting(k, v).is_ok());
            if saved {
                let _ = std::fs::remove_file(seed_path);
            }
        }
    }
    Config {
        host: store
            .setting("local_host")
            .unwrap_or_else(|| "http://192.168.1.35:8000/v1".into()),
        api_key: store
            .setting("local_api_key")
            .unwrap_or_else(|| std::env::var("OPENAI_API_KEY").unwrap_or_default()),
        model: store.setting("local_model").unwrap_or_default(),
        context_windows: store
            .setting("local_context_windows")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        output_limits: store
            .setting("local_output_limits")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        model_capabilities: store
            .setting("local_model_capabilities")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        reasoning_replay: store
            .setting("local_reasoning_replay")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        history_dir: store.path.with_file_name("local-sessions"),
        available_models: vec![],
        retry: false,
    }
}

fn endpoint(host: &str) -> Result<String, String> {
    let host = host.trim().trim_end_matches('/');
    if !(host.starts_with("http://") || host.starts_with("https://"))
        || host.contains(['\n', '\r', '"'])
    {
        return Err("Local host must be an http:// or https:// URL.".into());
    }
    Ok(format!(
        "{}/chat/completions",
        if host.ends_with("/v1") {
            host.to_owned()
        } else {
            format!("{host}/v1")
        }
    ))
}

pub fn validate(config: &Config) -> Result<(), String> {
    endpoint(&config.host)?;
    if config.model.trim().is_empty() {
        return Err(
            "Choose a local model in the composer or set its exact model name in Agent settings."
                .into(),
        );
    }
    if config.api_key.contains(['\n', '\r']) {
        return Err("API key must be a single line.".into());
    }
    Ok(())
}

pub fn start(
    mut config: AgentConfig,
    w: &Workstream,
    tx: SyncSender<Envelope>,
    ctx: egui::Context,
    restart: Option<&Path>,
) -> Result<RunHandle, String> {
    if w.mode == crate::store::Mode::Thinker {
        config.writable = false;
    }
    config.local = selected_config(&config.local, &w.model);
    validate(&config.local)?;
    let project = Path::new(&w.project)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let session = w
        .session
        .as_deref()
        .filter(|s| w.mode != crate::store::Mode::Thinker && s.starts_with("local:"))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("local:{}", uuid::Uuid::new_v4()));
    let session_id = session.trim_start_matches("local:");
    uuid::Uuid::parse_str(session_id).map_err(|_| "Invalid local session ID".to_string())?;
    let history_path = config.local.history_dir.join(format!("{session_id}.json"));
    let w = w.clone();
    let restart = restart.map(Path::to_owned);
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let join = thread::spawn(move || {
        let emit = |event| runner::send(&tx, &ctx, &w.id, event);
        emit(Event::Session(session));
        let result = run(
            &config,
            &w,
            &project,
            &history_path,
            restart.as_deref(),
            &stop,
            &emit,
        );
        if let Err(error) = &result {
            if !stop.load(Ordering::SeqCst) {
                emit(Event::Message {
                    item: "local-error".into(),
                    role: "Error".into(),
                    body: error.replace(
                        &config.local.api_key,
                        if config.local.api_key.is_empty() {
                            ""
                        } else {
                            "[redacted]"
                        },
                    ),
                });
            }
        }
        emit(Event::Activity {
            id: "local-request".into(),
            body: String::new(),
        });
        emit(Event::Finished {
            status: if stop.load(Ordering::SeqCst) {
                "Stopped"
            } else if result.is_ok() {
                "Ready"
            } else {
                "Failed"
            }
            .into(),
        });
    });
    Ok(RunHandle {
        cancelled,
        join: Some(join),
    })
}

fn run(
    config: &AgentConfig,
    w: &Workstream,
    project: &Path,
    history_path: &Path,
    restart: Option<&Path>,
    stop: &AtomicBool,
    emit: &impl Fn(Event),
) -> Result<(), String> {
    let writable = config.writable && w.mode != crate::store::Mode::Thinker;
    let mut messages: Vec<Value> = match std::fs::read(history_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("Cannot restore local session: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(format!("Cannot read local session: {e}")),
    };
    for message in &mut messages {
        omit_empty_tool_call_content(message);
        if message["role"] == "tool" {
            if let Some(text) = message["content"].as_str() {
                message["content"] = json!(tool_output(text));
            }
        }
    }
    recover_tools(&mut messages);
    let system = if config.local.is_deepseek() {
        json!({"role":"system","content":"You are a helpful software engineer assistant."})
    } else if w.mode == crate::store::Mode::Thinker {
        json!({"role":"system","content":crate::thinker::instructions()})
    } else {
        json!({"role":"system","content":format!(
            "You are a coding agent inside Skyforge. Work in {}.\n\n\
             Use the available tools when the request requires inspecting or changing the workspace. \
             Read applicable project AGENTS.md instructions and relevant files before editing. \
             Prefer edit_file for targeted changes; use write_file for new files or complete rewrites. \
             Verify changes with checks appropriate to the task.\n\n\
             Satisfy the user's requested outcome with the smallest necessary change. \
             If the requested behavior already exists, verify it and report that finding; no edit is required. \
             Keep exploration and verification proportional to the task. \
             Once the outcome is achieved and relevant checks pass, finish. \
             Continue only when concrete evidence shows an unmet requirement or unresolved failure.\n\n\
             For summaries and documentation, distinguish information reported in earlier conversation \
             from facts you verified during this run. Do not present earlier tests, builds, or restart \
             requests as newly performed. Do not claim actions you have not performed.\n\n\
             Return user-facing answers as ordinary text or Markdown. Do not print internal reasoning, \
             channel labels, or model protocol delimiters. Invoke tools through the provided \
             function-calling interface; do not print tool requests as prose or JSON in the answer.\n\n\
             Keep responses concise and show file paths clearly. When finished, briefly state the result, \
             verification performed, and any unresolved limitations.\n\n\
             Filesystem access is {}. Use only tools available in this run.{}",
            project.display(),
            if writable { "full read/write with the user account's permissions" } else { "read-only project tools" },
            if restart.is_some() && config.writable {
                " For user-requested Skyforge restarts, use the request_restart tool after building and verifying changes. Finish your final response normally; the host restarts after active runs finish."
            } else {
                ""
            }
        )})
    };
    if messages.first().is_some_and(|m| m["role"] == "system") {
        messages[0] = system;
    } else {
        messages.insert(0, system);
    }
    let mut draft = if w.mode == crate::store::Mode::Thinker {
        format!("{}\n\nCurrent user message:\n{}", w.context, w.draft)
    } else {
        w.draft.clone()
    };
    if w.delegation_endpoint.is_some() {
        draft.push_str("\n\n[Skyforge wake timers]\n");
        draft.push_str(crate::wakes::INSTRUCTIONS);
    }
    let mut content = vec![json!({"type":"text","text":draft})];
    for path in &w.attachments {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let mime = match image::guess_format(&bytes).map_err(|e| e.to_string())? {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            _ => return Err("Local attachments must be PNG, JPEG or WebP.".into()),
        };
        content.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(bytes))}}));
    }
    let user = json!({"role":"user","content": if content.len() == 1 { json!(draft) } else { json!(content) }});
    if !config.local.retry || messages.iter().rev().find(|m| m["role"] == "user") != Some(&user) {
        messages.push(user);
    }
    save_history(history_path, &messages)?;
    let mut compactor = compaction::Compactor::default();
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut repair_attempts = 0;
    let mut definitions = tools(writable, restart.is_some() && !config.local.is_deepseek());
    if w.delegation_endpoint.is_some() {
        definitions.extend(crate::wakes::definitions().into_iter().map(|tool| json!({"type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]}})));
    }
    if w.mode == crate::store::Mode::Thinker {
        definitions.extend(crate::thinker::tool_definitions().into_iter().map(|tool| json!({"type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]}})));
        if config.search {
            definitions.extend(crate::research::definitions());
        }
    }
    for step in 0..64 {
        if stop.load(Ordering::SeqCst) {
            return Err("Stopped".into());
        }
        let request_messages =
            compactor.prepare(&config.local, &messages, &definitions, stop, emit)?;
        emit(Event::Activity {
            id: "local-request".into(),
            body: format!("Waiting for {}…", config.local.model),
        });
        let request_id = uuid::Uuid::new_v4().to_string();
        let telemetry = ToolTelemetry {
            config: &config.local,
            context: json!({"schema_version":1,"model":config.local.model,"endpoint":endpoint(&config.local.host)?,"run_id":run_id,"request_id":request_id,"workstream_id":w.id,"session_id":history_path.file_stem().and_then(|s| s.to_str()),"mode":w.mode,"step":step + 1}),
            error: Mutex::new(None),
        };
        let record_response = |message: &Value, status: &str, error: Option<&str>| {
            let calls: Vec<_> = message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|call| json!({"id":call["id"],"name":call["function"]["name"]}))
                .collect();
            telemetry.record(json!({"event":"model_tool_use","status":status,"tool_call_count":calls.len(),"tool_calls":calls,"available_tools":definitions.iter().map(|tool| &tool["function"]["name"]).collect::<Vec<_>>(),"error":error}));
            telemetry.report_error(emit);
        };
        let response = request_with_id(
            &config.local,
            &completion_payload(&config.local, request_messages, &definitions),
            stop,
            emit,
            &request_id,
        )?;
        if let Some(error) = response.get("error") {
            record_response(&Value::Null, "api_error", error["message"].as_str());
            emit_prompt_usage(&response, "", emit);
            return Err(server_error(&config.local, error));
        }
        let Some(choice) = response["choices"].get(0) else {
            record_response(&Value::Null, "invalid_response", Some("No choices"));
            emit_prompt_usage(&response, "", emit);
            return Err("Local server returned no choices.".into());
        };
        let raw_message = &choice["message"];
        if choice["finish_reason"] == "length" {
            record_response(raw_message, "truncated", None);
            emit_prompt_usage(&response, "", emit);
            return Err("Local model reached its response limit. Shorten the request or use a model with a larger output limit.".into());
        }
        let message = match normalize_with_tools(raw_message, &definitions) {
            Ok(message) => message,
            Err(error) => {
                record_response(raw_message, "rejected", Some(&error));
                emit(Event::Message {
                    item: uuid::Uuid::new_v4().to_string(),
                    role: "Diagnostics".into(),
                    body: format!("Could not process the local model response: {error}"),
                });
                emit_prompt_usage(&response, "", emit);
                if repair_attempts >= 2 {
                    return Err(format!("Could not recover the local response after two correction requests: {error}; no tool from this response was executed. Use Retry request."));
                }
                repair_attempts += 1;
                emit(Event::Activity {
                    id: "local-request".into(),
                    body: "Asking the model to correct its response…".into(),
                });
                // Corrections are transient. Rejected calls never enter durable history and
                // cannot cause completed tools to be replayed on a later Retry request.
                messages.push(json!({"role":"user","content":format!("Your last response could not be processed: {error}. No tools from it were executed. Return the answer as ordinary Markdown, or invoke the available tools using structured function calls with valid JSON object arguments. Do not print protocol delimiters. Correct this response: {}", runner::clip(&raw_message.to_string(), 8000))}));
                continue;
            }
        };
        record_response(&message, "accepted", None);
        // Drop transient correction requests before saving the accepted answer.
        for _ in 0..repair_attempts {
            messages.pop();
        }
        repair_attempts = 0;
        let calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(body) = message["content"].as_str().filter(|s| !s.trim().is_empty()) {
            emit(Event::Message {
                item: uuid::Uuid::new_v4().to_string(),
                role: "Agent".into(),
                body: body.into(),
            });
        }
        emit_prompt_usage(&response, "", emit);
        messages.push(message.clone());
        save_history(history_path, &messages)?;
        if calls.is_empty() {
            if message["content"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
            {
                return Err("Local model returned no text or tool calls.".into());
            }
            save_history(history_path, &messages)?;
            return Ok(());
        }
        let execute = |call: &Value| {
            let name = call["function"]["name"].as_str().unwrap_or("");
            telemetry.record(json!({"event":"tool_execution","phase":"started","tool_call_id":call["id"],"tool":name}));
            let started = Instant::now();
            let result = serde_json::from_str::<Value>(
                call["function"]["arguments"].as_str().unwrap_or("{}"),
            )
            .map_err(|e| format!("Invalid tool arguments: {e}"))
            .and_then(|args| {
                if crate::wakes::is_tool(name) {
                    return crate::wakes::call(
                        w.delegation_endpoint
                            .as_ref()
                            .ok_or("Missing Skyforge timer endpoint")?,
                        &w.id,
                        w.invocation_turn
                            .as_deref()
                            .ok_or("Missing Skyforge turn identity")?,
                        name,
                        &args,
                    );
                }
                if w.mode == crate::store::Mode::Thinker && name == "spawn_builder_workstream" {
                    return crate::delegation_ipc::call(
                        w.delegation_endpoint
                            .as_ref()
                            .ok_or("Missing Skyforge handoff endpoint")?,
                        &w.id,
                        w.invocation_turn
                            .as_deref()
                            .ok_or("Missing Thinker request identity")?,
                        &args,
                    );
                }
                if w.mode == crate::store::Mode::Thinker
                    && config.search
                    && matches!(name, "web_search" | "fetch_url")
                {
                    return crate::research::tool(name, &args, stop);
                }
                if w.mode == crate::store::Mode::Thinker
                    && matches!(
                        name,
                        "search_conversations" | "read_conversation" | "search_memories"
                    )
                {
                    crate::thinker::read_tool(
                        name,
                        &args,
                        w.memory_db.as_deref().ok_or("Missing memory database")?,
                        w.thinker_project.as_deref(),
                    )
                } else {
                    tool(name, &args, project, writable, restart, stop)
                }
            });
            let status = match &result {
                Err(error) if error == "Stopped" => "cancelled",
                Err(_) => "failed",
                Ok(output)
                    if output.starts_with("Tool error:")
                        || (name == "run_command"
                            && !output.starts_with("Command succeeded\n")) =>
                {
                    "failed"
                }
                Ok(output) if name == "read_file" && output.contains("\nTool error:") => {
                    "partial_failure"
                }
                Ok(_) => "succeeded",
            };
            telemetry.record(json!({"event":"tool_execution","phase":"finished","tool_call_id":call["id"],"tool":name,"status":status,"duration_ms":started.elapsed().as_secs_f64() * 1000.0,"output_bytes":result.as_ref().ok().map(|s| s.len()),"error":result.as_ref().err().map(|s| runner::clip(s, 2000))}));
            result
        };
        let execution = execute_tool_calls(
            &calls,
            stop,
            &execute,
            |call| {
                emit(Event::Activity {
                    id: call["id"].as_str().unwrap_or("").into(),
                    body: format!(
                        "{}\n{}",
                        call["function"]["name"].as_str().unwrap_or(""),
                        call["function"]["arguments"].as_str().unwrap_or("")
                    ),
                });
            },
            |call, result| {
                let id = call["id"].as_str().ok_or("Local tool call has no ID.")?;
                let name = call["function"]["name"]
                    .as_str()
                    .ok_or("Local tool call has no name.")?;
                let output = tool_output(&match result {
                    Ok(s) => s,
                    Err(e) => format!("Tool error: {e}"),
                });
                emit(Event::Message {
                    item: id.into(),
                    role: "Tool".into(),
                    body: tool_message(
                        name,
                        call["function"]["arguments"].as_str().unwrap_or("{}"),
                        &output,
                    ),
                });
                messages.push(json!({"role":"tool","tool_call_id":id,"content":output}));
                save_history(history_path, &messages)
            },
        );
        telemetry.report_error(emit);
        execution?;
        save_history(history_path, &messages)?;
    }
    Err("Local agent stopped after 64 steps. Send another message to continue.".into())
}

// Only explicitly known read-only tools may overlap. Commands, edits, handoffs,
// and future tools form barriers so reads observe preceding mutations.
fn parallel_read(call: &Value) -> bool {
    matches!(
        call["function"]["name"].as_str().unwrap_or(""),
        "read_file"
            | "list_files"
            | "search_conversations"
            | "read_conversation"
            | "search_memories"
    )
}

fn execute_tool_calls(
    calls: &[Value],
    stop: &AtomicBool,
    execute: &(impl Fn(&Value) -> Result<String, String> + Sync),
    started: impl Fn(&Value),
    mut completed: impl FnMut(&Value, Result<String, String>) -> Result<(), String>,
) -> Result<(), String> {
    const MAX_PARALLEL_READS: usize = 8;
    let mut index = 0;
    while index < calls.len() {
        if stop.load(Ordering::SeqCst) {
            return Err("Stopped".into());
        }
        let count = if parallel_read(&calls[index]) {
            calls[index..]
                .iter()
                .take(MAX_PARALLEL_READS)
                .take_while(|call| parallel_read(call))
                .count()
        } else {
            1
        };
        let batch = &calls[index..index + count];
        for call in batch {
            started(call);
        }
        if count == 1 {
            completed(&batch[0], execute(&batch[0]))?;
        } else {
            thread::scope(|scope| {
                let pending: Vec<_> = batch
                    .iter()
                    .map(|call| {
                        scope.spawn(move || {
                            if stop.load(Ordering::SeqCst) {
                                Err("Stopped".into())
                            } else {
                                execute(call)
                            }
                        })
                    })
                    .collect();
                // Join every worker, including after a transcript write fails. Results
                // and UI messages retain call order; workers never write the history.
                let mut failure = None;
                for (call, worker) in batch.iter().zip(pending) {
                    let result = worker
                        .join()
                        .unwrap_or_else(|_| Err("Read tool worker panicked.".into()));
                    if failure.is_none() {
                        failure = completed(call, result).err();
                    }
                }
                failure.map_or(Ok(()), Err)
            })?;
        }
        index += count;
    }
    Ok(())
}

// Repair syntax only: never guess missing values, tool names, or code contents.
fn relaxed_json(input: &str) -> Result<Value, String> {
    let mut text = input.trim();
    if let Some(rest) = text.strip_prefix("```") {
        text = rest.strip_prefix("json").unwrap_or(rest).trim();
    }
    text = text.strip_suffix("```").unwrap_or(text).trim();
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut repaired = String::new();
    let (mut quoted, mut escaped) = (false, false);
    for (i, &ch) in chars.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
                repaired.push(ch);
            } else if ch == '\\' {
                escaped = true;
                repaired.push(ch);
            } else if ch == '"' {
                quoted = false;
                repaired.push(ch);
            } else if ch.is_control() {
                // JSON requires escaping literal newlines/tabs within strings.
                let encoded = serde_json::to_string(&ch.to_string()).unwrap();
                repaired.push_str(&encoded[1..encoded.len() - 1]);
            } else {
                repaired.push(ch);
            }
        } else if ch == '"' {
            quoted = true;
            repaired.push(ch);
        } else if ch == ','
            && chars[i + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
            // Ignore trailing commas outside string literals only.
        } else {
            repaired.push(ch);
        }
    }
    serde_json::from_str(&repaired).map_err(|e| format!("Invalid JSON: {e}"))
}

#[cfg(test)]
fn normalize_message(raw: &Value, writable: bool, restart: bool) -> Result<Value, String> {
    normalize_with_tools(raw, &tools(writable, restart))
}

fn normalize_with_tools(raw: &Value, definitions: &[Value]) -> Result<Value, String> {
    if !raw.is_object() || raw["role"].as_str().is_some_and(|r| r != "assistant") {
        return Err("Invalid assistant message".into());
    }
    let mut message = raw.clone();
    message["role"] = json!("assistant");
    let mut calls = match raw.get("tool_calls").filter(|v| !v.is_null()) {
        Some(Value::Array(calls)) => calls.clone(),
        Some(Value::Object(_)) => vec![raw["tool_calls"].clone()],
        Some(Value::String(s)) => {
            let value = relaxed_json(s)?;
            value.as_array().cloned().unwrap_or_else(|| vec![value])
        }
        Some(_) => return Err("Invalid tool_calls".into()),
        None => vec![],
    };
    if let Some(function) = raw.get("function_call").filter(|v| !v.is_null()) {
        if !calls.is_empty() {
            return Err("Conflicting tool call formats".into());
        }
        calls.push(json!({"function":function}));
        message.as_object_mut().unwrap().remove("function_call");
    }
    let mut body = match raw.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(body)) => body.clone(),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                if part["type"] == "thinking" {
                    message["_skyforge_content"] = raw["content"].clone();
                    continue;
                }
                if part["type"] != "text" {
                    return Err("Unsupported content block".into());
                }
                text.push_str(part["text"].as_str().ok_or("Invalid text block")?);
            }
            text
        }
        Some(_) => return Err("Invalid assistant content".into()),
    };
    if let Some(thought) = body.trim_start().strip_prefix("<think>") {
        let (reasoning, answer) = thought
            .split_once("</think>")
            .ok_or("Unclosed reasoning block")?;
        if message.get("reasoning_content").is_none_or(Value::is_null) {
            message["reasoning_content"] = json!(reasoning);
        }
        body = answer.trim().into();
    }
    // Recover Harmony text without ever discarding a tool recipient. Structured
    // calls can accompany a complete analysis block before any final answer exists.
    if body.trim_start().starts_with("<|channel|>") || body.trim_start().starts_with("<|start|>") {
        if body.contains("to=") || body.contains("<tool_call>") {
            return Err("Unparsed protocol tool request".into());
        }
        let marker = "<|channel|>final<|message|>";
        let analysis = body
            .trim()
            .strip_prefix("<|start|>assistant")
            .unwrap_or(body.trim())
            .strip_prefix("<|channel|>analysis<|message|>")
            .and_then(|text| text.strip_suffix("<|end|>"));
        if !calls.is_empty() && analysis.is_some_and(|text| !text.contains("<|")) {
            body.clear();
        } else if body.matches(marker).count() != 1 {
            return Err("Missing or ambiguous final channel".into());
        } else {
            let final_text = body.split_once(marker).unwrap().1.trim();
            body = final_text
                .trim_end_matches("<|fim_suffix|>")
                .trim_end_matches("<|end|>")
                .trim()
                .into();
        }
    }
    let text = body.trim();
    let xml = text.strip_prefix("<tool_call>");
    let candidate = xml.map(|s| s.strip_suffix("</tool_call>").unwrap_or(s));
    let parsed = relaxed_json(candidate.unwrap_or(text));
    let looks_like_tool = |value: &Value| {
        value.get("tool_calls").is_some()
            || value.get("function").is_some()
            || (value.get("name").is_some() && value.get("arguments").is_some())
    };
    if let Ok(value) = parsed {
        let tool_shaped = looks_like_tool(&value)
            || value
                .as_array()
                .is_some_and(|calls| !calls.is_empty() && calls.iter().all(looks_like_tool));
        if tool_shaped || xml.is_some() {
            if !calls.is_empty() {
                return Err("Conflicting text and structured tool calls".into());
            }
            let value = value.get("tool_calls").cloned().unwrap_or(value);
            calls = value.as_array().cloned().unwrap_or_else(|| vec![value]);
            body.clear();
        }
    } else if xml.is_some()
        || ((text.starts_with('{') || text.starts_with('[') || text.starts_with("```"))
            && (text.contains("tool_calls")
                || (text.contains("name") && text.contains("arguments"))))
    {
        return Err("Malformed text tool request".into());
    }
    if unparsed_protocol(&body) {
        return Err("Unparsed model protocol".into());
    }
    let mut normalized = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for call in calls {
        let function = call.get("function").unwrap_or(&call);
        let name = function["name"]
            .as_str()
            .ok_or("Tool call has no name")?
            .trim_start_matches("functions.");
        let definition = definitions
            .iter()
            .find(|d| d["function"]["name"] == name)
            .ok_or_else(|| format!("Unavailable tool: {name}"))?;
        let args = match function.get("arguments") {
            Some(Value::String(s)) => relaxed_json(s)?,
            Some(value) if value.is_object() => value.clone(),
            None if definition["function"]["parameters"]["required"]
                .as_array()
                .unwrap()
                .is_empty() =>
            {
                json!({})
            }
            _ => return Err(format!("{name} needs JSON object arguments")),
        };
        let object = args.as_object().ok_or("Tool arguments must be an object")?;
        let schema = &definition["function"]["parameters"];
        for key in schema["required"].as_array().unwrap() {
            if !object.contains_key(key.as_str().unwrap()) {
                return Err(format!("{name} missing {key}"));
            }
        }
        for (key, value) in object {
            let kind = schema["properties"][key]["type"]
                .as_str()
                .ok_or_else(|| format!("Unknown {name} argument: {key}"))?;
            if !(match kind {
                "string" => value.is_string(),
                "integer" => value.as_u64().is_some(),
                "array" => value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string)),
                _ => false,
            }) {
                return Err(format!("Invalid {name} argument type: {key}"));
            }
        }
        if name == "read_file" {
            read_file_paths(&args)?;
        }
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !ids.insert(id.clone()) {
            return Err("Duplicate tool call ID".into());
        }
        normalized.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}));
    }
    message["content"] = json!(body);
    if normalized.is_empty() && body.trim().is_empty() {
        return Err("Local model returned no text or tool calls".into());
    }
    message.as_object_mut().unwrap().remove("tool_calls");
    if !normalized.is_empty() {
        message["tool_calls"] = json!(normalized);
    }
    omit_empty_tool_call_content(&mut message);
    Ok(message)
}

// Apply to both new responses and restored history without revalidating old tools.
fn omit_empty_tool_call_content(message: &mut Value) {
    if message["role"] == "assistant"
        && message["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        && match message.get("content") {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.trim().is_empty(),
            _ => false,
        }
    {
        message.as_object_mut().unwrap().remove("content");
    }
}

fn unparsed_protocol(body: &str) -> bool {
    let markers = [
        "<|channel|>",
        "<|message|>",
        "<|start|>",
        "<|im_start|>",
        "<tool_call>",
    ];
    if markers.iter().any(|s| body.trim_start().starts_with(s))
        || markers.iter().filter(|s| body.contains(**s)).count() >= 2
    {
        return true;
    }
    let text = body
        .trim()
        .strip_prefix("```json")
        .and_then(|s| s.trim().strip_suffix("```"))
        .unwrap_or(body)
        .trim();
    serde_json::from_str::<Value>(text).is_ok_and(|v| {
        let known_tool = |name: &Value| {
            name.as_str().is_some_and(|name| {
                matches!(
                    name.trim_start_matches("functions."),
                    "list_files"
                        | "read_file"
                        | "write_file"
                        | "edit_file"
                        | "run_command"
                        | "request_restart"
                )
            })
        };
        v.get("tool_calls").is_some()
            || (known_tool(&v["name"]) && v.get("arguments").is_some())
            || known_tool(&v["function"]["name"])
    })
}

fn server_error(config: &Config, error: &Value) -> String {
    let message = error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .unwrap_or("Request rejected");
    format!("Local server: {message}\n\nModel: {}\nEndpoint: {}\nServer details: {}\n\nThe server supplied these details; Skyforge has no access to its internal logs. Use Retry request to retry the original task with the same model.",
        config.model, endpoint(&config.host).unwrap_or_default(), runner::clip(&error.to_string(), 4000))
}

fn recover_tools(messages: &mut Vec<Value>) {
    let Some(index) = messages.iter().rposition(|m| m["role"] == "assistant") else {
        return;
    };
    if messages[index + 1..].iter().any(|m| m["role"] != "tool") {
        return;
    }
    let calls = messages[index]["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for call in calls {
        if let Some(id) = call["id"].as_str() {
            if !messages[index + 1..]
                .iter()
                .any(|m| m["tool_call_id"] == id)
            {
                messages.push(json!({"role":"tool","tool_call_id":id,"content":"This tool was interrupted. Its result is unknown. Inspect the workspace before retrying an edit or command."}));
            }
        }
    }
}

fn save_history(path: &Path, messages: &[Value]) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid local history directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, messages).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

pub(crate) const READ_FILE_DESCRIPTION: &str = "Read UTF-8 text from one or several files. Supply either path (one file) or paths (1–20 files); prefer paths when reading several files to save model round trips. Relative paths use the project directory; absolute paths are supported in writable runs. Line numbers are one-based; start_line and max_lines apply to every file. Batch results are labeled by path, report errors per file, and share the output budget; use smaller batches or line ranges for large files.";

pub(crate) fn read_file_properties() -> Value {
    json!({"path":{"type":"string"},"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":20},"start_line":{"type":"integer"},"max_lines":{"type":"integer"}})
}

fn read_file_paths(args: &Value) -> Result<Vec<&str>, String> {
    match (args.get("path"), args.get("paths")) {
        (Some(path), None) => Ok(vec![path
            .as_str()
            .ok_or("Invalid read_file argument type: path")?]),
        (None, Some(paths)) => {
            let paths = paths
                .as_array()
                .ok_or("Invalid read_file argument type: paths")?;
            if paths.is_empty() || paths.len() > 20 {
                return Err("read_file paths must contain 1–20 files.".into());
            }
            paths
                .iter()
                .map(|p| {
                    p.as_str()
                        .ok_or_else(|| "read_file paths must contain strings.".into())
                })
                .collect()
        }
        _ => Err("read_file requires exactly one of path or paths.".into()),
    }
}

fn read_file(input: &str, args: &Value, root: &Path, writable: bool) -> Result<String, String> {
    let path = if writable {
        root.join(input)
    } else {
        workspace_path(root, input, false)?
    };
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(2_000_001)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 2_000_000 {
        return Err("File is too large to read; use a targeted command.".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "File is not UTF-8 text")?;
    let start = args["start_line"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["max_lines"].as_u64().unwrap_or(300).clamp(1, 600) as usize;
    Ok(runner::clip(
        &text
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(limit)
            .map(|(i, line)| format!("{}: {line}", i + 1))
            .collect::<Vec<_>>()
            .join("\n"),
        64_000,
    ))
}

fn tools(writable: bool, restart: bool) -> Vec<Value> {
    let mut defs = vec![
        (
            "list_files",
            "Recursively list files, excluding ignored files. Optional path selects a directory; omitted or empty path uses the project directory. Relative paths use the project directory; absolute paths outside the project are supported in writable runs. Results use project-relative paths inside the project and absolute paths outside it.",
            json!({"path":{"type":"string"}}),
            vec![],
        ),
        (
            "read_file",
            READ_FILE_DESCRIPTION,
            read_file_properties(),
            vec![],
        ),
    ];
    if writable {
        defs.extend([
        ("write_file", "Create or overwrite a UTF-8 file. Paths may be absolute or relative to the project.", json!({"path":{"type":"string"},"content":{"type":"string"}}), vec!["path","content"]),
        ("edit_file", "Replace one exact, unique occurrence of old_text in a file. Paths may be absolute or relative to the project.", json!({"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}}), vec!["path","old_text","new_text"]),
        ("run_command", "Run a shell command from the project directory with full filesystem access under the user account. Use for builds and tests. 120-second limit.", json!({"command":{"type":"string"}}), vec!["command"]),
    ]);
    }
    if restart && writable {
        defs.push((
            "request_restart",
            "Queue a Skyforge restart after this run finishes.",
            json!({}),
            vec![],
        ));
    }
    defs.into_iter().map(|(name,description,properties,required)| json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})).collect()
}

fn workspace_path(root: &Path, input: &str, create: bool) -> Result<PathBuf, String> {
    let candidate = root.join(input);
    let mut ancestor = candidate.as_path();
    let mut tail = vec![];
    while match std::fs::symlink_metadata(ancestor) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e.to_string()),
    } {
        if !create {
            return Err("File does not exist.".into());
        }
        tail.push(ancestor.file_name().ok_or("Invalid path")?.to_owned());
        ancestor = ancestor.parent().ok_or("Invalid path")?;
    }
    let mut resolved = ancestor.canonicalize().map_err(|e| e.to_string())?;
    if !resolved.starts_with(root) {
        return Err("Path is outside the project.".into());
    }
    for part in tail.into_iter().rev() {
        if part == ".." || part == "." {
            return Err("Invalid path component.".into());
        }
        resolved.push(part);
    }
    if candidate
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err("Parent traversal is not allowed.".into());
    }
    Ok(resolved)
}
pub(crate) fn tool(
    name: &str,
    args: &Value,
    root: &Path,
    writable: bool,
    restart: Option<&Path>,
    stop: &AtomicBool,
) -> Result<String, String> {
    let string = |key| args[key].as_str().ok_or_else(|| format!("Missing {key}"));
    match name {
        "list_files" => {
            let input = match args.get("path") {
                None => "",
                Some(value) => value
                    .as_str()
                    .ok_or("Invalid list_files argument type: path")?,
            };
            let directory = if writable {
                root.join(input).canonicalize().map_err(|e| e.to_string())?
            } else {
                workspace_path(root, input, false)?
            };
            if !directory.is_dir() {
                return Err("list_files path must be a directory.".into());
            }
            let files = ignore::WalkBuilder::new(&directory)
                .require_git(false)
                .build()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .take(1500)
                .map(|e| {
                    e.path()
                        .strip_prefix(root)
                        .unwrap_or(e.path())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            Ok(runner::clip(&files.join("\n"), 64_000))
        }
        "read_file" => {
            let paths = read_file_paths(args)?;
            if args.get("paths").is_none() {
                return read_file(paths[0], args, root, writable);
            }
            let budget = (TOOL_OUTPUT_LIMIT - 256) / paths.len();
            let mut sections = Vec::with_capacity(paths.len());
            for path in paths {
                if stop.load(Ordering::Relaxed) {
                    return Err("Stopped".into());
                }
                let output = match read_file(path, args, root, writable) {
                    Ok(text) => text,
                    Err(error) => format!("Tool error: {error}"),
                };
                let header = format!("=== {} ===\n", runner::clip(path, 200));
                sections.push(format!(
                    "{header}{}",
                    runner::clip(&output, budget.saturating_sub(header.len() + 32))
                ));
            }
            Ok(sections.join("\n\n"))
        }

        "write_file" | "edit_file" if writable => {
            let path = root.join(string("path")?);
            let content = if name == "edit_file" {
                let old = string("old_text")?;
                let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                if old.is_empty() || text.matches(old).count() != 1 {
                    return Err("old_text must match exactly once.".into());
                }
                text.replacen(old, string("new_text")?, 1)
            } else {
                string("content")?.into()
            };
            std::fs::create_dir_all(path.parent().ok_or("Invalid path")?)
                .map_err(|e| e.to_string())?;
            std::fs::write(&path, content).map_err(|e| e.to_string())?;
            Ok(format!(
                "Saved {}",
                path.strip_prefix(root).unwrap_or(&path).display()
            ))
        }
        "run_command" if writable => shell(string("command")?, root, restart, stop),
        "request_restart" if writable => {
            let path = restart.ok_or("Skyforge restart is unavailable")?;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.to_string()),
            }
            Ok(
                "Skyforge will restart after active runs finish. Finish your response normally."
                    .into(),
            )
        }
        _ => Err("Tool is unavailable in this run.".into()),
    }
}

pub fn discover_models(config: &Config) -> Result<ModelCatalog, String> {
    let url = endpoint(&config.host)?.replace("/chat/completions", "/models");
    if config.api_key.contains(['\n', '\r']) {
        return Err("API key must be a single line.".into());
    }
    let response = http_request(config, &url, None, &AtomicBool::new(false), 15, None, None)?;
    if let Some(error) = response.get("error") {
        return Err(format!(
            "Local server: {}",
            error["message"]
                .as_str()
                .unwrap_or("Model discovery failed")
        ));
    }
    let data = response["data"]
        .as_array()
        .ok_or("Local server returned no model list.")?;
    let mut models = Vec::new();
    let mut capabilities = std::collections::BTreeMap::new();
    for model in data {
        if let Some(id) = model["id"].as_str().filter(|id| !id.trim().is_empty()) {
            if !models.iter().any(|m| m == id) {
                models.push(id.to_owned());
            }
            let max_output_tokens = [
                &model["max_output_tokens"],
                &model["max_completion_tokens"],
                &model["limits"]["output"],
                &model["top_provider"]["max_completion_tokens"],
            ]
            .into_iter()
            .filter_map(Value::as_u64)
            .filter_map(|limit| usize::try_from(limit).ok())
            .find(|limit| *limit > 0);
            let reasoning = model["reasoning"]
                .as_bool()
                .or_else(|| model["capabilities"]["reasoning"].as_bool());
            if max_output_tokens.is_some() || reasoning.is_some() {
                capabilities.insert(
                    id.to_owned(),
                    ModelCapabilities {
                        max_output_tokens,
                        reasoning,
                    },
                );
            }
        }
    }
    if models.is_empty() {
        return Err("Local server advertises no models.".into());
    }
    Ok(ModelCatalog {
        ids: models,
        capabilities,
    })
}

fn emit_prompt_usage(response: &Value, label: &str, emit: &impl Fn(Event)) {
    let usage = &response["usage"];
    let hit = usage["prompt_cache_hit_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64())
        .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64());
    let miss = usage["prompt_cache_miss_tokens"].as_u64().or_else(|| {
        usage["prompt_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens"].as_u64())?
            .checked_sub(hit?)
    });
    let display = |count: Option<u64>| {
        count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unavailable".into())
    };
    emit(Event::Message {
        item: uuid::Uuid::new_v4().to_string(),
        role: "Usage".into(),
        body: format!(
            "{label}Prompt cache hit: {} tokens · Prompt cache miss: {} tokens",
            display(hit),
            display(miss)
        ),
    });
}

fn request(
    config: &Config,
    payload: &Value,
    stop: &AtomicBool,
    emit: &impl Fn(Event),
) -> Result<Value, String> {
    request_with_id(
        config,
        payload,
        stop,
        emit,
        &uuid::Uuid::new_v4().to_string(),
    )
}

fn request_with_id(
    config: &Config,
    payload: &Value,
    stop: &AtomicBool,
    emit: &impl Fn(Event),
    request_id: &str,
) -> Result<Value, String> {
    http_request(
        config,
        &endpoint(&config.host)?,
        Some(payload),
        stop,
        300,
        Some(emit),
        Some(request_id),
    )
}

fn http_request(
    config: &Config,
    url: &str,
    payload: Option<&Value>,
    stop: &AtomicBool,
    timeout: u64,
    emit: Option<&dyn Fn(Event)>,
    request_id: Option<&str>,
) -> Result<Value, String> {
    // Keep credentials off argv and out of shell/environment history; curl reads its
    // config from stdin, and the JSON body from a private temporary file.
    let temp = TempDir::new()?;
    let body = temp.0.join("request.json");
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut input = format!("url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\n", quote(url), quote(&config.api_key));
    let request_id = request_id
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let log = |direction: &str, body: &str, status: Option<&str>| {
        if payload.is_some() {
            if let Err(error) = log_http_json(config, &request_id, direction, url, body, status) {
                if let Some(emit) = emit {
                    emit(Event::Message {
                        item: "local-log-error".into(),
                        role: "Diagnostics".into(),
                        body: format!("Could not write the local-model log: {error}"),
                    });
                }
            }
        }
    };
    if let Some(payload) = payload {
        let json = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        std::fs::write(&body, &json).map_err(|e| e.to_string())?;
        log("request", &json, None);
        input.push_str(&format!(
            "data-binary = \"@{}\"\n",
            quote(&body.to_string_lossy())
        ));
    }
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--fail-with-body",
        "--connect-timeout",
        "10",
        "--max-time",
        &timeout.to_string(),
        "--max-filesize",
        "16777216",
        "--write-out",
        "\nHARNESS_HTTP_STATUS:%{http_code}",
        "--config",
        "-",
    ]);
    let (success, output, errors) = execute(
        cmd,
        Some(input),
        stop,
        Duration::from_secs(timeout + 5),
        16_777_216,
    )
    .inspect_err(|error| log("transport_error", error, None))?;
    let output_text = String::from_utf8_lossy(&output);
    let (body, status) = output_text
        .rsplit_once("\nHARNESS_HTTP_STATUS:")
        .unwrap_or((&output_text, "unknown"));
    log("response", body, Some(status.trim()));
    let mut parsed: Value = relaxed_json(body).map_err(|_| {
        format!(
            "Local server request failed (HTTP {status}): {}",
            runner::clip(
                &if body.is_empty() {
                    String::from_utf8_lossy(&errors)
                } else {
                    body.into()
                },
                2000
            )
        )
    })?;
    if let Some(error) = parsed.get_mut("error") {
        if !error.is_object() {
            *error = json!({"message": error.as_str().unwrap_or("Request rejected")});
        }
        error["http_status"] = json!(status.trim());
    }
    if !success && parsed.get("error").is_none() {
        return Err(format!(
            "Local server HTTP request failed (HTTP {status}). {}",
            runner::clip(body, 2000)
        ));
    }
    Ok(parsed)
}

// Serialize appends from parallel workstreams so each JSONL record stays intact.
static HTTP_LOG_LOCK: Mutex<()> = Mutex::new(());

fn http_log_path(config: &Config) -> PathBuf {
    config
        .history_dir
        .with_file_name("logs")
        .join("local-model")
        .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")))
}

fn log_http_json(
    config: &Config,
    request_id: &str,
    direction: &str,
    url: &str,
    body: &str,
    status: Option<&str>,
) -> Result<(), String> {
    let record = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "request_id": request_id,
        "direction": direction,
        "model": config.model,
        "endpoint": url,
        "http_status": status,
        // Retain the wire body before parsing, formatting recovery, or error metadata.
        // Authorization headers are never included.
        "body": body,
    });
    append_model_log(config, &record)
}

struct ToolTelemetry<'a> {
    config: &'a Config,
    context: Value,
    error: Mutex<Option<String>>,
}

impl ToolTelemetry<'_> {
    fn record(&self, details: Value) {
        let mut record = self.context.clone();
        record
            .as_object_mut()
            .unwrap()
            .extend(details.as_object().unwrap().clone());
        record["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        if let Err(error) = append_model_log(self.config, &record) {
            let mut first = self.error.lock().unwrap();
            if first.is_none() {
                *first = Some(error);
            }
        }
    }

    fn report_error(&self, emit: &impl Fn(Event)) {
        if let Some(error) = self.error.lock().unwrap().take() {
            emit(Event::Message {
                item: uuid::Uuid::new_v4().to_string(),
                role: "Diagnostics".into(),
                body: format!("Could not write tool-use telemetry: {error}"),
            });
        }
    }
}

fn append_model_log(config: &Config, record: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let path = http_log_path(config);
    let _guard = HTTP_LOG_LOCK.lock().map_err(|e| e.to_string())?;
    let directory = path.parent().ok_or("Invalid log directory")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(&bytes).map_err(|e| e.to_string())
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("harness-local-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder
                .mode(0o700)
                .create(&path)
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new(if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/sh"
    });
    // GUI launches can inherit a minimal PATH. Load the user's login setup so
    // installed development tools are discoverable just as in a login shell.
    cmd.args(["-lc", command]);
    cmd
}

fn shell(
    command: &str,
    root: &Path,
    restart: Option<&Path>,
    stop: &AtomicBool,
) -> Result<String, String> {
    let temp = TempDir::new()?;
    let mut cmd = shell_command(command);
    cmd.current_dir(root)
        .env("TMPDIR", &temp.0)
        .env_remove("OPENAI_API_KEY");
    if let Some(path) = restart {
        cmd.env(crate::restart::REQUEST_ENV, path);
        cmd.env(
            crate::restart::EXECUTABLE_ENV,
            std::env::current_exe().map_err(|e| e.to_string())?,
        );
    }
    let (ok, mut output, errors) = execute(cmd, None, stop, Duration::from_secs(120), 64_000)?;
    // Keep stderr available to the result limiter even when stdout fills its
    // capture buffer, so noisy commands do not hide their error diagnostics.
    if !errors.is_empty() && !output.ends_with(b"\n") {
        output.push(b'\n');
    }
    output.extend_from_slice(&errors);
    Ok(format!(
        "{}\n{}",
        if ok {
            "Command succeeded"
        } else {
            "Command failed"
        },
        String::from_utf8_lossy(&output)
    ))
}
pub(crate) fn execute(
    mut cmd: Command,
    input: Option<String>,
    stop: &AtomicBool,
    timeout: Duration,
    limit: usize,
) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Cannot start local agent helper: {e}"))?;
    let writer = child.stdin.take().map(|mut stdin| {
        thread::spawn(move || {
            let _ = stdin.write_all(input.unwrap_or_default().as_bytes());
        })
    });
    let stdout = child.stdout.take().ok_or("Missing stdout")?;
    let stderr = child.stderr.take().ok_or("Missing stderr")?;
    let reader = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut output = vec![];
            let mut buf = [0; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let keep = n.min(limit.saturating_sub(output.len()));
                output.extend_from_slice(&buf[..keep]);
            }
            output
        })
    };
    let out = reader(Box::new(stdout));
    let err = reader(Box::new(stderr));
    let start = Instant::now();
    let status = loop {
        if stop.load(Ordering::SeqCst) || start.elapsed() > timeout {
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        thread::sleep(Duration::from_millis(40));
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let output = out.join().map_err(|_| "Output reader failed")?;
    let errors = err.join().map_err(|_| "Error reader failed")?;
    if stop.load(Ordering::SeqCst) {
        return Err("Stopped".into());
    }
    if start.elapsed() > timeout {
        return Err("Local agent helper timed out.".into());
    }
    Ok((status.success(), output, errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    #[test]
    fn tool_output_bounds_single_lines_and_unicode_preserving_status_and_tail() {
        for filler in ["x", "🦀", "é"] {
            let text = format!("Command failed\n{}\nfinal error", filler.repeat(20_000));
            let output = tool_output(&text);
            assert!(output.len() <= TOOL_OUTPUT_LIMIT);
            assert!(output.starts_with("Command failed\n"));
            assert!(output.contains("[Output truncated:"));
            assert!(output.ends_with("final error"));
            assert_eq!(tool_output(&output), output);
        }
        for text in [String::new(), "x".repeat(TOOL_OUTPUT_LIMIT)] {
            assert_eq!(tool_output(&text), text);
        }
    }

    #[test]
    fn completed_command_message_includes_exact_command_and_output() {
        let command = "rg 'tool_calls' src\nprintf 'done\\n'";
        let args = json!({"command":command}).to_string();
        assert_eq!(
            tool_message("run_command", &args, "Command succeeded\nresult"),
            format!("run_command\n$ {command}\nCommand succeeded\nresult")
        );
        assert_eq!(
            tool_message("read_file", "{}", "contents"),
            "read_file\ncontents"
        );
    }

    #[test]
    fn oversized_tool_results_are_bounded_before_display_save_and_next_request() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let history_path = root.join("history.json");
        let old_output = format!("Command succeeded\n{}", "old".repeat(10_000));
        save_history(&history_path, &[
            json!({"role":"assistant","tool_calls":[{"id":"old","type":"function","function":{"name":"run_command","arguments":"{\"command\":\"old command\"}"}}]}),
            json!({"role":"tool","tool_call_id":"old","content":old_output}),
        ]).unwrap();
        let command = "printf 'start\\n'; printf '%20000s' ''; printf '\\nend\\n'";
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let old = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["tool_call_id"] == "old")
                .unwrap();
            assert!(old["content"].as_str().unwrap().len() <= TOOL_OUTPUT_LIMIT);
            assert!(old["content"]
                .as_str()
                .unwrap()
                .contains("Output truncated"));
            respond(
                &mut socket,
                json!({"role":"assistant","tool_calls":[{"id":"large","type":"function","function":{"name":"run_command","arguments":json!({"command":command}).to_string()}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let output = request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(output.len() <= TOOL_OUTPUT_LIMIT);
            assert!(output.starts_with("Command succeeded\nstart\n"));
            assert!(output.contains("Output truncated"));
            assert!(output.ends_with("\nend\n"));
            respond(&mut socket, json!({"role":"assistant","content":"Done."}));
        });
        let mut w = Workstream::new(&root);
        w.draft = "run command".into();
        let events = std::cell::RefCell::new(Vec::new());
        run(
            &cfg,
            &w,
            &root,
            &history_path,
            None,
            &AtomicBool::new(false),
            &|event| events.borrow_mut().push(event),
        )
        .unwrap();
        fixture.join().unwrap();
        let saved: Vec<Value> =
            serde_json::from_slice(&std::fs::read(history_path).unwrap()).unwrap();
        for message in saved.iter().filter(|m| m["role"] == "tool") {
            assert!(message["content"].as_str().unwrap().len() <= TOOL_OUTPUT_LIMIT);
        }
        let output = saved.iter().find(|m| m["tool_call_id"] == "large").unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(events.borrow().iter().any(|event| matches!(event,
            Event::Message { item, role, body } if item == "large" && role == "Tool" && body == &format!("run_command\n$ {command}\n{output}")
        )));
    }

    #[test]
    fn compaction_batches_summaries_and_preserves_saved_history_and_latest_tools() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        std::fs::write(root.join("README.md"), "latest file contents").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        cfg.local
            .context_windows
            .insert(cfg.local.model.clone(), 8192);
        let history_path = root.join("history.json");
        let mut history = vec![
            json!({"role":"system","content":"old system"}),
            json!({"role":"user","content":"original request"}),
        ];
        for i in 0..20 {
            history.push(json!({"role":"assistant","content":format!("Output {i}: {}", "x".repeat(4000)), "reasoning_content":"private reasoning"}));
        }
        save_history(&history_path, &history).unwrap();
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let summary = receive(&mut socket);
            assert!(summary.get("tools").is_none());
            let inputs: Vec<String> =
                serde_json::from_str(summary["messages"][1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(inputs.len(), 13);
            assert!(inputs[0].starts_with("Output 0:"));
            assert!(inputs[3].starts_with("Output 10:"));
            let lines: Vec<_> = (0..13).map(|i| format!("Concise result {i}")).collect();
            respond(
                &mut socket,
                json!({"role":"assistant", "content":serde_json::to_string(&lines).unwrap()}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let messages = request["messages"].as_array().unwrap();
            assert_eq!(messages.iter().filter(|m| m["role"] == "user").count(), 2);
            assert!(!request.to_string().contains("private reasoning"));
            assert!(!request.to_string().contains(&"x".repeat(3000)));
            assert!(request.to_string().contains("Concise result 12"));
            respond(
                &mut socket,
                json!({"role":"assistant", "tool_calls":[{"id":"latest","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let messages = request["messages"].as_array().unwrap();
            assert_eq!(
                messages[messages.len() - 2]["tool_calls"][0]["id"],
                "latest"
            );
            assert_eq!(messages[messages.len() - 1]["tool_call_id"], "latest");
            assert!(messages[messages.len() - 1]["content"]
                .as_str()
                .unwrap()
                .contains("latest file contents"));
            respond(&mut socket, json!({"role":"assistant","content":"Done."}));
        });
        let mut w = Workstream::new(&root);
        w.draft = "follow-up correction".into();
        run(
            &cfg,
            &w,
            &root,
            &history_path,
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        let saved: Vec<Value> =
            serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
        assert_eq!(saved[1..22], history[1..22]);
        assert_eq!(saved[22]["content"], "follow-up correction");
        assert_eq!(saved.last().unwrap()["content"], "Done.");
        assert!(!saved.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.starts_with("Historical recap"))));
    }

    #[test]
    fn compaction_summary_failure_uses_excerpts() {
        let temp = TempDir::new().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = config(&temp.0, format!("http://{}", server.local_addr().unwrap()));
        cfg.local
            .context_windows
            .insert(cfg.local.model.clone(), 8192);
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let payload = receive(&mut socket);
            assert!(payload["messages"].to_string().len().div_ceil(3) + 1536 < 8192);
            respond(
                &mut socket,
                json!({"role":"assistant","content":"invalid summary"}),
            );
        });
        let history = vec![
            json!({"role":"user","content":"original"}),
            json!({"role":"assistant","content":"\"\n".repeat(50_000)}),
            json!({"role":"user","content":"follow-up"}),
        ];
        let events = std::cell::RefCell::new(Vec::new());
        let projected = compaction::Compactor::default()
            .prepare(
                &cfg.local,
                &history,
                &[],
                &AtomicBool::new(false),
                &|event| events.borrow_mut().push(event),
            )
            .unwrap();
        fixture.join().unwrap();
        assert!(projected[1]["content"]
            .as_str()
            .unwrap()
            .contains("Excerpt:"));
        assert!(events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Message { role, .. } if role == "Diagnostics")));
    }

    #[test]
    fn local_context_settings_round_trip_per_model_and_accept_legacy_config() {
        let legacy: Config = serde_json::from_value(
            json!({"host":"http://localhost","api_key":"", "model":"first"}),
        )
        .unwrap();
        assert!(legacy.context_windows.is_empty());
        let temp = TempDir::new().unwrap();
        let store = crate::store::Store::open(temp.0.join("settings.sqlite3")).unwrap();
        store
            .set_setting("local_context_windows", r#"{"first":8192,"second":65536}"#)
            .unwrap();
        let restored = load(&store, &temp.0);
        let selected = selected_config(&restored, &selection_id("second"));
        assert_eq!(selected.context_windows[&selected.model], 65536);
        assert_eq!(selected.context_windows["first"], 8192);
    }

    #[test]
    fn reasoning_replays_after_tools_and_resume_without_reaching_ui() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        std::fs::write(root.join("README.md"), "contents").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        cfg.local.model = "deepseek-fixture".into();
        cfg.local
            .output_limits
            .insert(cfg.local.model.clone(), 8192);
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let first = receive(&mut socket);
            assert_eq!(
                first["messages"][0]["content"],
                "You are a helpful software engineer assistant."
            );
            assert!(first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["function"]["name"] != "request_restart"));
            assert_eq!(first["max_tokens"], 8192);
            respond(
                &mut socket,
                json!({"role":"assistant","content":"","reasoning_content":"private tool reasoning","tool_calls":[{"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let second = receive(&mut socket);
            assert_eq!(
                second["messages"][2]["reasoning_content"],
                "private tool reasoning"
            );
            assert_eq!(second["messages"][2]["content"], "");
            assert_eq!(second["messages"][3]["tool_call_id"], "read");
            respond(
                &mut socket,
                json!({"role":"assistant","content":"<think>private final reasoning</think>Done."}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let third = receive(&mut socket);
            assert_eq!(
                third["messages"][0]["content"],
                "You are a helpful software engineer assistant."
            );
            assert_eq!(
                third["messages"][2]["reasoning_content"],
                "private tool reasoning"
            );
            assert_eq!(
                third["messages"][4]["reasoning_content"],
                "private final reasoning"
            );
            assert_eq!(third["messages"][4]["content"], "Done.");
            respond(
                &mut socket,
                json!({"role":"assistant","content":"Confirmed."}),
            );
        });
        let mut w = Workstream::new(&root);
        w.draft = "Read README.md".into();
        let history_path = root.join("history.json");
        let events = std::cell::RefCell::new(Vec::new());
        let restart_path = root.join("restart.request");
        for draft in ["Read README.md", "Confirm"] {
            w.draft = draft.into();
            run(
                &cfg,
                &w,
                &root,
                &history_path,
                Some(&restart_path),
                &AtomicBool::new(false),
                &|event| events.borrow_mut().push(event),
            )
            .unwrap();
        }
        fixture.join().unwrap();
        assert!(std::fs::read_to_string(history_path)
            .unwrap()
            .contains("private final reasoning"));
        for event in events.into_inner() {
            if let Event::Message { body, .. } = event {
                assert!(!body.contains("private tool reasoning"));
                assert!(!body.contains("private final reasoning"));
            }
        }
    }

    #[test]
    fn automatic_output_defaults_follow_capabilities_and_preserve_overrides() {
        let mut cfg = Config {
            model: "plain-model".into(),
            host: "http://localhost:8000/v1".into(),
            ..Default::default()
        };
        cfg.context_windows.insert(cfg.model.clone(), 131_072);
        assert_eq!(cfg.output_limit(), 16_384);
        cfg.model = "Qwen/Qwen3.8-27B".into();
        cfg.context_windows.insert(cfg.model.clone(), 131_072);
        assert_eq!(cfg.output_limit(), 32_768);
        cfg.model_capabilities.insert(
            cfg.replay_key(),
            ModelCapabilities {
                max_output_tokens: Some(65_536),
                reasoning: Some(true),
            },
        );
        assert_eq!(cfg.output_limit(), 65_536);
        cfg.output_limits.insert(cfg.model.clone(), 8192);
        assert_eq!(cfg.output_limit(), 8192);
        cfg.model_capabilities
            .get_mut(&cfg.replay_key())
            .unwrap()
            .max_output_tokens = Some(4096);
        assert_eq!(cfg.output_limit(), 4096);
        cfg.output_limits.clear();
        cfg.model_capabilities
            .get_mut(&cfg.replay_key())
            .unwrap()
            .max_output_tokens = None;
        cfg.model_capabilities
            .get_mut(&cfg.replay_key())
            .unwrap()
            .reasoning = Some(false);
        assert_eq!(cfg.output_limit(), 16_384);
        cfg.context_windows.insert(cfg.model.clone(), 8192);
        assert_eq!(cfg.output_limit(), 4096);
        cfg.host = "http://other-server/v1".into();
        cfg.context_windows.insert(cfg.model.clone(), 131_072);
        assert_eq!(cfg.output_limit(), 32_768);
    }

    #[test]
    fn replay_defaults_overrides_and_output_limits_round_trip() {
        let temp = TempDir::new().unwrap();
        let store = crate::store::Store::open(temp.0.join("history.sqlite3")).unwrap();
        let mut cfg = Config::default();
        for (host, model, expected) in [
            (
                "https://api.deepseek.com/v1",
                "alias",
                ReasoningReplay::PreserveHistory,
            ),
            (
                "http://localhost:8000/v1",
                "Qwen/Qwen3-8B",
                ReasoningReplay::CurrentTurn,
            ),
            (
                "http://localhost:8000/v1",
                "Qwen/Qwen3.8-27B",
                ReasoningReplay::PreserveHistory,
            ),
            ("http://localhost:8000/v1", "unknown", ReasoningReplay::Omit),
        ] {
            cfg.host = host.into();
            cfg.model = model.into();
            assert_eq!(cfg.replay_policy(), expected);
        }
        cfg.reasoning_replay
            .insert(cfg.replay_key(), ReasoningReplay::PreserveHistory);
        cfg.context_windows.insert(cfg.model.clone(), 8192);
        cfg.output_limits.insert(cfg.model.clone(), 65536);
        cfg.model_capabilities.insert(
            cfg.replay_key(),
            ModelCapabilities {
                max_output_tokens: Some(65_536),
                reasoning: Some(true),
            },
        );
        assert_eq!(cfg.output_limit(), 6656);
        store.set_setting("local_host", &cfg.host).unwrap();
        store.set_setting("local_model", &cfg.model).unwrap();
        store
            .set_setting(
                "local_context_windows",
                &serde_json::to_string(&cfg.context_windows).unwrap(),
            )
            .unwrap();
        store
            .set_setting(
                "local_output_limits",
                &serde_json::to_string(&cfg.output_limits).unwrap(),
            )
            .unwrap();
        store
            .set_setting(
                "local_model_capabilities",
                &serde_json::to_string(&cfg.model_capabilities).unwrap(),
            )
            .unwrap();
        store
            .set_setting(
                "local_reasoning_replay",
                &serde_json::to_string(&cfg.reasoning_replay).unwrap(),
            )
            .unwrap();
        let mut restored = load(&store, &temp.0);
        assert_eq!(restored.replay_policy(), ReasoningReplay::PreserveHistory);
        assert_eq!(restored.output_limit(), 6656);
        assert_eq!(
            restored.model_capabilities[&restored.replay_key()].max_output_tokens,
            Some(65_536)
        );
        restored.host = "http://other-server/v1".into();
        assert_eq!(restored.replay_policy(), ReasoningReplay::Omit);
        assert_eq!(Config::default().output_limit(), 16_384);
    }

    #[test]
    fn provider_payloads_use_their_reasoning_controls() {
        let mut cfg = Config {
            model: "Qwen/Qwen3.8-27B".into(),
            host: "http://localhost:8000/v1".into(),
            ..Default::default()
        };
        assert_eq!(
            completion_payload(&cfg, vec![], &[])["chat_template_kwargs"]["preserve_thinking"],
            true
        );
        cfg.host = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1".into();
        assert_eq!(
            completion_payload(&cfg, vec![], &[])["preserve_thinking"],
            true
        );
        cfg.host = "https://api.deepseek.com/v1".into();
        cfg.model = "deepseek-flash".into();
        cfg.reasoning_replay
            .insert(cfg.replay_key(), ReasoningReplay::Omit);
        assert_eq!(
            completion_payload(&cfg, vec![], &[])["thinking"]["type"],
            "disabled"
        );
    }

    #[test]
    fn mistral_thinking_chunks_are_saved_separately_from_visible_text() {
        let raw = json!({"role":"assistant","content":[{"type":"thinking","thinking":[{"type":"text","text":"private thought"}]},{"type":"text","text":"Visible answer"}]});
        let normalized = normalize_with_tools(&raw, &[]).unwrap();
        assert_eq!(normalized["content"], "Visible answer");
        assert_eq!(normalized["_skyforge_content"], raw["content"]);
        let cfg = Config {
            model: "magistral-small".into(),
            ..Default::default()
        };
        let projected = compaction::Compactor::default()
            .prepare(&cfg, &[normalized], &[], &AtomicBool::new(false), &|_| {})
            .unwrap();
        assert_eq!(projected[0]["content"], raw["content"]);
        assert!(projected[0].get("_skyforge_content").is_none());
    }

    #[test]
    fn direct_responses_emit_cache_usage_after_text_without_polluting_history() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        std::fs::write(root.join("README.md"), "file contents").unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let responses = [
                json!({"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]}}],"usage":{"prompt_cache_hit_tokens":100,"prompt_cache_miss_tokens":25}}),
                json!({"choices":[{"message":{"role":"assistant","content":"Done."}}],"usage":{"prompt_tokens":200,"prompt_tokens_details":{"cached_tokens":150}}}),
            ];
            for response in responses {
                let (mut socket, _) = server.accept().unwrap();
                let request = receive(&mut socket);
                assert!(!request.to_string().contains("Prompt cache hit"));
                let body = response.to_string();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let mut w = Workstream::new(&root);
        w.draft = "Read README.md".into();
        let history_path = root.join("history.json");
        let events = std::cell::RefCell::new(Vec::new());
        run(
            &cfg,
            &w,
            &root,
            &history_path,
            None,
            &AtomicBool::new(false),
            &|event| events.borrow_mut().push(event),
        )
        .unwrap();
        fixture.join().unwrap();
        let events = events.into_inner();
        let messages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Message { role, body, .. } => Some((role.as_str(), body.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            messages.iter().map(|(role, _)| *role).collect::<Vec<_>>(),
            ["Usage", "Tool", "Agent", "Usage"]
        );
        assert_eq!(
            messages[0].1,
            "Prompt cache hit: 100 tokens · Prompt cache miss: 25 tokens"
        );
        assert_eq!(
            messages[3].1,
            "Prompt cache hit: 150 tokens · Prompt cache miss: 50 tokens"
        );
        assert!(!std::fs::read_to_string(history_path)
            .unwrap()
            .contains("Prompt cache hit"));
        let log = std::fs::read_to_string(http_log_path(&cfg.local)).unwrap();
        let records: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let responses: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "model_tool_use")
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["tool_call_count"], 1);
        assert_eq!(responses[1]["tool_call_count"], 0);
        let executions: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "tool_execution")
            .collect();
        assert_eq!(executions.len(), 2);
        assert_eq!(executions[0]["phase"], "started");
        assert_eq!(executions[1]["phase"], "finished");
        assert_eq!(executions[1]["status"], "succeeded");
        assert_eq!(executions[1]["tool"], "read_file");
        assert!(executions[1]["duration_ms"].as_f64().unwrap() >= 0.0);
        for record in responses.iter().chain(executions.iter()) {
            assert_eq!(record["model"], cfg.local.model);
            assert_eq!(record["workstream_id"], w.id);
            assert_eq!(record["run_id"], responses[0]["run_id"]);
            assert!(records.iter().any(|wire| wire["direction"] == "response"
                && wire["request_id"] == record["request_id"]));
        }
        assert_eq!(executions[1]["request_id"], responses[0]["request_id"]);
        assert!(!log.contains("fixture-key"));
    }

    #[test]
    fn tool_telemetry_distinguishes_rejected_calls_from_failed_executions() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            for message in [
                json!({"role":"assistant","tool_calls":[{"id":"invalid","function":{"name":"unavailable_tool","arguments":"{}"}}]}),
                json!({"role":"assistant","tool_calls":[{"id":"command","function":{"name":"run_command","arguments":"{\"command\":\"exit 7\"}"}}]}),
                json!({"role":"assistant","content":"The command failed."}),
            ] {
                let (mut socket, _) = server.accept().unwrap();
                receive(&mut socket);
                respond(&mut socket, message);
            }
        });
        let mut w = Workstream::new(&root);
        w.draft = "Run a command".into();
        run(
            &cfg,
            &w,
            &root,
            &root.join("history.json"),
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        let records: Vec<Value> = std::fs::read_to_string(http_log_path(&cfg.local))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let decisions: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "model_tool_use")
            .collect();
        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[0]["status"], "rejected");
        assert_eq!(decisions[0]["tool_calls"][0]["name"], "unavailable_tool");
        assert_eq!(decisions[1]["status"], "accepted");
        assert_eq!(decisions[2]["tool_call_count"], 0);
        let executions: Vec<_> = records
            .iter()
            .filter(|record| record["event"] == "tool_execution")
            .collect();
        assert_eq!(executions.len(), 2);
        assert!(executions
            .iter()
            .all(|record| record["tool"] == "run_command"));
        assert_eq!(executions[1]["status"], "failed");
    }

    #[test]
    fn cache_usage_does_not_invent_counts_when_provider_omits_them() {
        for (response, expected) in [
            (
                json!({"usage":{"prompt_tokens":200}}),
                "Prompt cache hit: unavailable tokens · Prompt cache miss: unavailable tokens",
            ),
            (
                json!({"usage":{"input_tokens":200,"input_tokens_details":{"cached_tokens":0}}}),
                "Prompt cache hit: 0 tokens · Prompt cache miss: 200 tokens",
            ),
            (
                json!({"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":200}}}),
                "Prompt cache hit: 200 tokens · Prompt cache miss: unavailable tokens",
            ),
        ] {
            emit_prompt_usage(&response, "", &|event| {
                let Event::Message { role, body, .. } = event else {
                    panic!("Expected usage")
                };
                assert_eq!(role, "Usage");
                assert_eq!(body, expected);
            });
        }
    }

    #[test]
    fn local_http_logs_preserve_wire_json_without_ui_messages() {
        let temp = TempDir::new().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&temp.0, format!("http://{}", server.local_addr().unwrap()));
        let payload =
            json!({"model":"fixture-qwen","messages":[{"role":"user","content":"hello"}]});
        let responses = [
            ("200 OK", "{ \"choices\": [], \"extra\": \"untouched\" }\n"),
            (
                "422 Unprocessable Entity",
                "{\n \"error\": {\"message\": \"failed\"}\n}\n",
            ),
            ("422 Unprocessable Entity", "not JSON\n```\nraw error\n"),
        ];
        let expected = payload.clone();
        let fixture = thread::spawn(move || {
            for (status, body) in responses {
                let (mut socket, _) = server.accept().unwrap();
                assert_eq!(receive(&mut socket), expected);
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        for (index, (status, raw)) in responses.iter().enumerate() {
            let events = std::cell::RefCell::new(Vec::new());
            let result = request(&cfg.local, &payload, &AtomicBool::new(false), &|event| {
                events.borrow_mut().push(event)
            });
            assert!(
                events.into_inner().is_empty(),
                "Wire JSON must stay out of the UI"
            );
            let text = std::fs::read_to_string(http_log_path(&cfg.local)).unwrap();
            assert!(!text.contains("Authorization"));
            assert!(!text.contains("fixture-key"));
            let records: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records.len(), (index + 1) * 2);
            let request = &records[index * 2];
            let response = &records[index * 2 + 1];
            assert_eq!(request["direction"], "request");
            assert_eq!(request["body"], payload.to_string());
            assert_eq!(response["direction"], "response");
            assert_eq!(response["request_id"], request["request_id"]);
            assert_eq!(response["http_status"], &status[..3]);
            assert_eq!(response["body"], *raw);
            if index == 1 {
                assert_eq!(result.unwrap()["error"]["http_status"], "422");
                assert!(!response["body"].as_str().unwrap().contains("http_status"));
            } else if index == 2 {
                assert!(result.is_err());
            } else {
                assert!(result.is_ok());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(http_log_path(&cfg.local))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fixture.join().unwrap();
    }

    #[test]
    fn json_repairs_preserve_code_and_reject_guessed_values() {
        for input in [
            "```json\n{\"content\":\"line1\nline2\",}",
            "{\"content\":\"line1\\nline2\",}\n```",
        ] {
            assert_eq!(
                relaxed_json(input).unwrap(),
                json!({"content":"line1\nline2"})
            );
        }
        let code = "const x = ',}';\nconsole.log(\"a\");";
        let input = format!("{{\"content\":{},}}", serde_json::to_string(code).unwrap());
        assert_eq!(relaxed_json(&input).unwrap()["content"], code);
        for invalid in [
            "{'path':'x'}",
            "{path: 'x'}",
            "{\"path\":",
            "{\"path\":\"x\"",
        ] {
            assert!(relaxed_json(invalid).is_err());
        }
    }

    #[test]
    fn response_recovery_handles_common_tool_formats_and_final_channels() {
        let expected = json!({"path":"x","content":"hello"});
        for raw in [
            json!({"role":"assistant","content":"```json\n{\"name\":\"functions.write_file\",\"arguments\":{\"path\":\"x\",\"content\":\"hello\",},}"}),
            json!({"role":"assistant","content":"<tool_call>{\"name\":\"write_file\",\"arguments\":{\"path\":\"x\",\"content\":\"hello\"}}</tool_call>"}),
            json!({"role":"assistant","function_call":{"name":"write_file","arguments":expected}}),
            json!({"role":"assistant","content":"[{\"name\":\"write_file\",\"arguments\":{\"path\":\"x\",\"content\":\"hello\"}}]"}),
            json!({"tool_calls":{"function":{"name":"write_file","arguments":"```json\n{\"path\":\"x\",\"content\":\"hello\",}\n```"}}}),
        ] {
            let message = normalize_message(&raw, true, false).unwrap();
            let call = &message["tool_calls"][0];
            assert_eq!(call["function"]["name"], "write_file");
            assert_eq!(
                relaxed_json(call["function"]["arguments"].as_str().unwrap()).unwrap(),
                expected
            );
            assert!(!call["id"].as_str().unwrap().is_empty());
            assert!(message.get("function_call").is_none());
        }
        for content in [
            "<|channel|>analysis<|message|>Thinking<|end|><|start|>assistant<|channel|>final<|message|>Done.<|fim_suffix|>",
            "<think>Thinking</think>Done.",
        ] {
            let message = normalize_message(&json!({"role":"assistant","content":content}), true, false).unwrap();
            assert_eq!(message["content"], "Done.");
        }
        let text = "Example:\n```json\n{\"name\":\"write_file\",\"arguments\":{}}\n```";
        let message =
            normalize_message(&json!({"role":"assistant","content":text}), true, false).unwrap();
        assert_eq!(message["content"], text);
        assert!(message.get("tool_calls").is_none());
        assert!(normalize_message(
            &json!({"role":"assistant","content":"{name: 'write_file', arguments: {path: 'x'}}"}),
            true,
            false
        )
        .is_err());
    }

    #[test]
    fn assistant_tool_call_serialization_omits_only_empty_content() {
        let calls = json!([{"id":"call1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]);
        for content in [json!(""), json!(" \n\t"), Value::Null, json!([])] {
            let raw = json!({"role":"assistant","content":content,"tool_calls":calls});
            let message = normalize_message(&raw, false, false).unwrap();
            let serialized: Value = serde_json::from_str(&message.to_string()).unwrap();
            assert!(serialized.get("content").is_none());
            assert_eq!(serialized["tool_calls"], calls);
        }
        let message = normalize_message(
            &json!({"role":"assistant","tool_calls":calls}),
            false,
            false,
        )
        .unwrap();
        assert!(message.get("content").is_none());

        let text = "  I’ll read the file.\n";
        let raw = json!({"role":"assistant","content":text,"tool_calls":calls});
        assert_eq!(normalize_message(&raw, false, false).unwrap(), raw);
        let raw = json!({"role":"assistant","content":"Done."});
        assert_eq!(normalize_message(&raw, false, false).unwrap(), raw);
    }

    #[test]
    fn history_content_normalization_preserves_other_messages() {
        let calls = json!([{"id":"call1"}]);
        for mut message in [
            json!({"role":"assistant","content":""}),
            json!({"role":"assistant","content":null}),
            json!({"role":"assistant"}),
            json!({"role":"assistant","content":"","tool_calls":[]}),
            json!({"role":"assistant","content":"","tool_calls":null}),
            json!({"role":"assistant","content":"  Real text.\n","tool_calls":calls}),
            json!({"role":"tool","content":"","tool_call_id":"call1"}),
            json!({"role":"tool","content":"result","tool_call_id":"call1"}),
            json!({"role":"user","content":"","tool_calls":calls}),
            json!({"role":"assistant","content":[{"type":"text","text":"Real text."}],"tool_calls":calls}),
        ] {
            let original = message.clone();
            omit_empty_tool_call_content(&mut message);
            assert_eq!(message, original);
        }
    }

    #[test]
    fn restored_multi_turn_tool_history_omits_empty_assistant_content() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let history_path = root.join("history.json");
        let mut history = vec![
            json!({"role":"user","content":"Read two files."}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"old1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"one.txt\"}"}}]}),
            json!({"role":"tool","tool_call_id":"old1","content":"one"}),
            json!({"role":"assistant","content":"Read the first file."}),
            json!({"role":"user","content":"Continue."}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"old2","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"two.txt\"}"}}]}),
            json!({"role":"tool","tool_call_id":"old2","content":""}),
            json!({"role":"assistant","content":"Both files read."}),
        ];
        save_history(&history_path, &history).unwrap();
        // The request and persisted history must differ only in these two fields.
        history[1].as_object_mut().unwrap().remove("content");
        history[5].as_object_mut().unwrap().remove("content");
        let expected = history.clone();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let messages = request["messages"].as_array().unwrap();
            assert_eq!(messages[0]["role"], "system");
            assert_eq!(messages[1..9], expected);
            assert_eq!(messages[9], json!({"role":"user","content":"Summarize."}));
            respond(&mut socket, json!({"role":"assistant","content":"Done."}));
        });
        let mut w = Workstream::new(&root);
        w.draft = "Summarize.".into();
        run(
            &cfg,
            &w,
            &root,
            &history_path,
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        let saved: Vec<Value> =
            serde_json::from_slice(&std::fs::read(history_path).unwrap()).unwrap();
        assert_eq!(saved[1..9], history);
        assert_eq!(saved[10], json!({"role":"assistant","content":"Done."}));
    }

    #[test]
    fn response_recovery_accepts_analysis_with_structured_calls() {
        let call = json!({"function":{"arguments":"{\"command\":\"mkdir -p docs\"}","name":"run_command"},"id":"call_67ECDC68497E4AD3B6969D83","type":"function"});
        let content = "<|channel|>analysis<|message|>We need to create docs folder then write file. Use run_command to mkdir. Then write_file.<|end|>";
        for content in [content.to_owned(), format!("<|start|>assistant{content}")] {
            let raw = json!({"role":"assistant","content":content,"tool_calls":[call.clone()]});
            let message = normalize_message(&raw, true, false).unwrap();
            assert!(message.get("content").is_none());
            assert_eq!(message["tool_calls"], json!([call.clone()]));
            assert!(normalize_message(&raw, false, false).is_err());
            assert!(
                normalize_message(&json!({"role":"assistant","content":content}), true, false)
                    .is_err()
            );
            for invalid in [
                json!({"function":{"name":"unknown","arguments":{}}}),
                json!({"function":{"name":"run_command","arguments":{}}}),
                json!({"function":{"name":"run_command","arguments":{"command":42}}}),
            ] {
                assert!(normalize_message(
                    &json!({"role":"assistant","content":content,"tool_calls":[call.clone(),invalid]}),
                    true, false
                ).is_err());
            }
        }
        for content in [
            "<|channel|>analysis<|message|>Unclosed",
            "<|channel|>analysis<|message|>Thinking<|end|>Extra text",
            "<|channel|>analysis<|message|>Thinking<|end|><|channel|>analysis<|message|>More<|end|>",
            "<|start|>assistant to=functions.run_command<|channel|>analysis<|message|>Thinking<|end|>",
            "<|channel|>analysis<|message|><tool_call>{}</tool_call><|end|>",
        ] {
            assert!(normalize_message(
                &json!({"role":"assistant","content":content,"tool_calls":[call.clone()]}),
                true, false
            ).is_err());
        }
    }

    #[test]
    fn response_recovery_validates_entire_batches_and_permissions() {
        let valid = json!({"id":"a","function":{"name":"write_file","arguments":{"path":"x","content":"hello"}}});
        for invalid in [
            json!({"function":{"name":"unknown","arguments":{}}}),
            json!({"function":{"name":"write_file","arguments":{"path":"x"}}}),
            json!({"function":{"name":"read_file","arguments":{"path":42}}}),
            json!({"function":{"name":"read_file","arguments":{"path":"x","extra":true}}}),
            valid.clone(),
        ] {
            assert!(normalize_message(
                &json!({"role":"assistant","tool_calls":[valid.clone(),invalid]}),
                true,
                false
            )
            .is_err());
        }
        assert!(normalize_message(
            &json!({"role":"assistant","tool_calls":[valid]}),
            false,
            false
        )
        .is_err());
        assert!(normalize_message(
            &json!({"role":"assistant","tool_calls":[{"name":"request_restart","arguments":{}}]}),
            true,
            false
        )
        .is_err());
        assert!(normalize_message(&json!({"role":"assistant","content":"<|start|>assistant to=functions.write_file<|channel|>final<|message|>Done"}), true, false).is_err());
    }

    #[test]
    fn corrected_tool_response_executes_once_and_saves_clean_history() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            receive(&mut socket);
            respond(
                &mut socket,
                json!({"role":"assistant","tool_calls":[
                    {"name":"write_file","arguments":{"path":"must-not-exist","content":"bad"}},
                    {"name":"write_file","arguments":{"path":"x"}}
                ]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            assert_eq!(request["messages"].as_array().unwrap().len(), 3);
            assert!(request["messages"][2]["content"]
                .as_str()
                .unwrap()
                .contains("No tools from it were executed"));
            respond(
                &mut socket,
                json!({"role":"assistant","content":"{\"name\":\"write_file\",\"arguments\":{\"path\":\"x\",\"content\":\"hello\",}}"}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let messages = request["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(
                messages[3]["tool_call_id"],
                messages[2]["tool_calls"][0]["id"]
            );
            respond(
                &mut socket,
                json!({"role":"assistant","content":"<|channel|>final<|message|>Done.<|fim_suffix|>"}),
            );
        });
        let mut w = Workstream::new(&root);
        w.draft = "Write a greeting".into();
        run(
            &cfg,
            &w,
            &root,
            &root.join("history.json"),
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        assert!(!root.join("must-not-exist").exists());
        assert_eq!(std::fs::read_to_string(root.join("x")).unwrap(), "hello");
        let history: Vec<Value> =
            serde_json::from_slice(&std::fs::read(root.join("history.json")).unwrap()).unwrap();
        assert_eq!(history.len(), 5);
        assert_eq!(history[4]["content"], "Done.");
        assert_eq!(history.iter().filter(|m| m["role"] == "tool").count(), 1);
    }
    #[test]
    fn failed_inference_and_unparsed_protocol_retry_without_duplicate_user_messages() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            for attempt in 0..5 {
                let (mut socket, _) = server.accept().unwrap();
                let payload = receive(&mut socket);
                let messages = payload["messages"].as_array().unwrap();
                assert_eq!(
                    messages.iter().filter(|m| m["role"] == "user").count(),
                    if (2..=3).contains(&attempt) {
                        attempt
                    } else {
                        1
                    }
                );
                assert_eq!(
                    messages[1]["content"],
                    "Document what we implemented so far"
                );
                assert_eq!(payload["model"], "fixture-qwen");
                if attempt == 0 {
                    let body = json!({"error":{"message":"Inference generation failed","type":"server_error","code":"fixture_code"}}).to_string();
                    write!(socket, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                } else if attempt <= 3 {
                    respond(
                        &mut socket,
                        json!({"role":"assistant","content":"<|channel|>analysis<|message|>Thinking<|start|>assistant to=functions.list_files<|message|>{\"path\":\"\"}"}),
                    );
                } else {
                    respond(
                        &mut socket,
                        json!({"role":"assistant","content":"Documentation complete."}),
                    );
                }
            }
        });
        let mut w = Workstream::new(&root);
        w.model = MODEL_ID.into();
        w.draft = "Document what we implemented so far".into();
        for attempt in 0..3 {
            cfg.local.retry = attempt > 0;
            let (tx, rx) = std::sync::mpsc::sync_channel(256);
            let handle = start(cfg.clone(), &w, tx, egui::Context::default(), None).unwrap();
            let mut error_seen = false;
            let mut diagnostic_seen = false;
            loop {
                match rx.recv_timeout(Duration::from_secs(15)).unwrap().event {
                    Event::Session(session) => w.session = Some(session),
                    Event::Message { role, body, .. } if role == "Error" => {
                        error_seen = true;
                        if attempt == 0 {
                            for detail in [
                                "Inference generation failed",
                                "500",
                                "fixture_code",
                                "fixture-qwen",
                                "Retry request",
                            ] {
                                assert!(body.contains(detail), "Missing {detail}: {body}");
                            }
                        } else {
                            assert!(body.contains("no tool from this response was executed"));
                        }
                    }
                    Event::Message { role, .. } if role == "Diagnostics" => diagnostic_seen = true,
                    Event::Message { role, body, .. } if role == "Agent" => {
                        assert_eq!(attempt, 2);
                        assert_eq!(body, "Documentation complete.");
                    }
                    Event::Finished { status } => {
                        assert_eq!(status, if attempt < 2 { "Failed" } else { "Ready" });
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(error_seen, attempt < 2);
            assert_eq!(diagnostic_seen, attempt == 1);
            handle.finish();
        }
        fixture.join().unwrap();
    }

    #[test]
    fn protocol_detection_keeps_ordinary_text_and_json() {
        assert!(!unparsed_protocol("The documentation is complete."));
        assert!(!unparsed_protocol(r#"{"result":"complete"}"#));
        assert!(unparsed_protocol(
            r#"{"name":"read_file","arguments":{"path":"README.md"}}"#
        ));
    }
    fn config(root: &Path, host: String) -> AgentConfig {
        AgentConfig {
            binary: "unused".into(),
            claude_binary: "unused".into(),
            search: false,
            writable: true,
            local: Config {
                host,
                api_key: "fixture-key".into(),
                model: "fixture-qwen".into(),
                context_windows: Default::default(),
                output_limits: Default::default(),
                model_capabilities: Default::default(),
                reasoning_replay: Default::default(),
                history_dir: root.join("sessions"),
                available_models: vec![],
                retry: false,
            },
        }
    }

    #[test]
    fn wake_tools_use_the_same_host_service_for_deepseek_and_other_models() {
        use std::net::TcpListener;
        for model in ["deepseek-chat", "fixture-qwen"] {
            let temp = TempDir::new().unwrap();
            let root = temp.0.canonicalize().unwrap();
            let store = crate::store::Store::open(root.join("history.sqlite3")).unwrap();
            let mut w = Workstream::new(&root);
            w.model = MODEL_ID.into();
            w.draft = "Check later".into();
            w.invocation_turn = Some("timer-turn".into());
            store.save(&w).unwrap();
            store.save_submission(&w).unwrap();
            let host = crate::delegation_ipc::Host::new(egui::Context::default()).unwrap();
            w.delegation_endpoint = Some(host.endpoint());
            let path = store.path.clone();
            let dispatcher = thread::spawn(move || {
                let db = crate::store::Store::tool_connection(path).unwrap();
                for _ in 0..3 {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let request = loop {
                        match host.try_recv() {
                            Ok(request) => break request,
                            Err(_) if Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(1))
                            }
                            Err(_) => panic!("Missing timer IPC request"),
                        }
                    };
                    let result = crate::wakes::handle(
                        &db,
                        &request.source,
                        &request.turn,
                        request.args["wake_tool"].as_str().unwrap(),
                        &request.args["arguments"],
                    );
                    request.reply.send(result).unwrap();
                }
            });
            let server = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
            cfg.local.model = model.into();
            let fixture = thread::spawn(move || {
                let mut timer = String::new();
                for step in 0..4 {
                    let (mut socket, _) = server.accept().unwrap();
                    let request = receive(&mut socket);
                    for name in ["schedule_wake", "list_wakes", "cancel_wake"] {
                        assert!(request["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t["function"]["name"] == name));
                    }
                    assert!(request["messages"][1]["content"]
                        .as_str()
                        .unwrap()
                        .contains(crate::wakes::INSTRUCTIONS));
                    let (name, args) = match step {
                        0 => (
                            "schedule_wake",
                            json!({"request_id":"job","instructions":"Check job","delay_seconds":86400}),
                        ),
                        1 => {
                            let last = request["messages"].as_array().unwrap().last().unwrap();
                            assert_eq!(last["tool_call_id"], "timer-0");
                            let receipt: Value =
                                serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
                            assert_eq!(receipt["status"], "pending");
                            timer = receipt["id"].as_str().unwrap().into();
                            ("list_wakes", json!({}))
                        }
                        2 => {
                            let last = request["messages"].as_array().unwrap().last().unwrap();
                            let timers: Value =
                                serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
                            assert_eq!(timers[0]["id"], timer);
                            ("cancel_wake", json!({"id":timer}))
                        }
                        _ => {
                            let last = request["messages"].as_array().unwrap().last().unwrap();
                            let receipt: Value =
                                serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
                            assert_eq!(receipt["status"], "cancelled");
                            respond(
                                &mut socket,
                                json!({"role":"assistant","content":"Timer cancelled.","reasoning_content":"Done"}),
                            );
                            break;
                        }
                    };
                    respond(
                        &mut socket,
                        json!({"role":"assistant","reasoning_content":"Manage the requested timer","tool_calls":[{"id":format!("timer-{step}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
                    );
                }
            });
            run(
                &cfg,
                &w,
                &root,
                &root.join("session.json"),
                None,
                &AtomicBool::new(false),
                &|_| {},
            )
            .unwrap();
            fixture.join().unwrap();
            dispatcher.join().unwrap();
            assert!(store.wakes(None).unwrap().is_empty());
            let history: Value =
                serde_json::from_slice(&std::fs::read(root.join("session.json")).unwrap()).unwrap();
            assert_eq!(
                history
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .count(),
                3
            );
        }
    }

    #[test]
    fn thinker_local_profile_uses_conversation_and_memory_tools_without_editing() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let db = crate::store::Store::open(root.join("history.sqlite3")).unwrap();
        let mut w = Workstream::new(&root);
        w.mode = crate::store::Mode::Thinker;
        w.model = MODEL_ID.into();
        w.draft = "What style do I prefer?".into();
        db.save(&w).unwrap();
        db.remember("", "answer style", "Prefer short answers", None)
            .unwrap();
        w.context = db.thinker_context(&w).unwrap();
        w.memory_db = Some(db.path.clone());
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            assert!(request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with(crate::thinker::SYSTEM_PROMPT));
            assert!(request["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("Prefer short answers"));
            let tools = request["tools"].as_array().unwrap();
            for blocked in ["web_search", "fetch_url"] {
                assert!(
                    !tools.iter().any(|tool| tool["function"]["name"] == blocked),
                    "Disabling web research must remove its tools"
                );
            }
            for blocked in ["write_file", "edit_file", "run_command", "request_restart"] {
                assert!(!tools.iter().any(|tool| tool["function"]["name"] == blocked));
            }
            assert!(tools
                .iter()
                .any(|tool| tool["function"]["name"] == "search_memories"));
            respond(
                &mut socket,
                json!({"role":"assistant","tool_calls":[{"id":"memory-read","type":"function","function":{"name":"search_memories","arguments":"{\"query\":\"style\"}"}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let last = request["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["role"], "tool");
            assert!(last["content"]
                .as_str()
                .unwrap()
                .contains("Prefer short answers"));
            respond(
                &mut socket,
                json!({"role":"assistant","content":"You prefer short answers."}),
            );
        });
        // Even a writable Builder configuration cannot grant a Thinker run edits.
        assert!(cfg.writable);
        run(
            &cfg,
            &w,
            &root,
            &root.join("session.json"),
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
    }
    #[test]
    fn thinker_function_call_delegates_and_replays_without_builder_permissions() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let db = crate::store::Store::open(root.join("history.sqlite3")).unwrap();
        db.save_project(&crate::store::ThinkerProject {
            id: "project".into(),
            name: "Project".into(),
            instructions: String::new(),
            directory: Some(root.to_string_lossy().into()),
        })
        .unwrap();
        let mut w = Workstream::new(&root);
        w.mode = crate::store::Mode::Thinker;
        w.model = MODEL_ID.into();
        w.thinker_project = Some("project".into());
        w.invocation_turn = Some("trusted-request".into());
        w.draft = "Execute the plan in Builder".into();
        w.memory_db = Some(db.path.clone());
        db.save(&w).unwrap();
        db.save_submission(&w).unwrap();
        let host = crate::delegation_ipc::Host::new(egui::Context::default()).unwrap();
        w.delegation_endpoint = Some(host.endpoint());
        let host_db = db.path.clone();
        let dispatcher = thread::spawn(move || {
            let store = crate::store::Store::tool_connection(host_db).unwrap();
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let request = loop {
                    match host.try_recv() {
                        Ok(request) => break request,
                        Err(_) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(_) => panic!("Thinker must request delegation over IPC"),
                    }
                };
                let result = crate::delegation::create(
                    &request.args,
                    &store,
                    &request.source,
                    &request.turn,
                );
                request.reply.send(result).unwrap();
            }
        });
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let tools = request["tools"].as_array().unwrap();
            assert!(tools
                .iter()
                .any(|t| t["function"]["name"] == "spawn_builder_workstream"));
            assert!(!tools.iter().any(|t| t["function"]["name"] == "run_command"));
            let args = json!({"title":"Build feature","task":"Execute requested plan"}).to_string();
            respond(
                &mut socket,
                json!({"role":"assistant","tool_calls":[
                    {"id":"handoff","type":"function","function":{"name":"spawn_builder_workstream","arguments":args}},
                    {"id":"replayed-handoff","type":"function","function":{"name":"spawn_builder_workstream","arguments":args}}
                ]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            let results = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .map(|m| serde_json::from_str::<Value>(m["content"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0]["id"], results[1]["id"]);
            assert_eq!(results[0]["status"], "Queued");
            respond(
                &mut socket,
                json!({"role":"assistant","content":"Builder is queued; use Open Builder to inspect it."}),
            );
        });
        run(
            &cfg,
            &w,
            &root,
            &root.join("session.json"),
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        dispatcher.join().unwrap();
        let queued = db.queued_builders().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].mode, crate::store::Mode::Builder);
        assert_eq!(queued[0].project, w.project);
        assert_eq!(db.linked_builders(&w.id).unwrap()[0].id, queued[0].id);
        assert_eq!(db.messages(&queued[0].id, None).unwrap().len(), 1);
    }

    #[test]
    fn chosen_local_model_reads_web_sources_and_receives_citable_results() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let page = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/source", page.local_addr().unwrap());
        let page_fixture = thread::spawn(move || {
            let (mut socket, _) = page.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("GET /source "));
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let body = "<p>Reference evidence for the discussion.</p>";
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        cfg.search = true;
        cfg.local.model = "selected-research-model".into();
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            assert_eq!(request["model"], "selected-research-model");
            for name in ["web_search", "fetch_url"] {
                assert!(request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == name));
            }
            respond(
                &mut socket,
                json!({"role":"assistant","tool_calls":[{"id":"source-read","type":"function","function":{"name":"fetch_url","arguments":json!({"url":url}).to_string()}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let request = receive(&mut socket);
            assert_eq!(request["model"], "selected-research-model");
            let last = request["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["role"], "tool");
            let source: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
            assert_eq!(source["url"], url);
            assert!(source["text"]
                .as_str()
                .unwrap()
                .contains("Reference evidence"));
            respond(
                &mut socket,
                json!({"role":"assistant","content":format!("The evidence supports the discussion. [Source]({url})")}),
            );
        });
        let mut w = Workstream::new(&root);
        w.mode = crate::store::Mode::Thinker;
        w.draft = "Read the source and discuss the evidence.".into();
        run(
            &cfg,
            &w,
            &root,
            &root.join("session.json"),
            None,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        fixture.join().unwrap();
        page_fixture.join().unwrap();
    }

    fn receive(socket: &mut std::net::TcpStream) -> Value {
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        let mut length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                length = n.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }
    fn respond(socket: &mut std::net::TcpStream, message: Value) {
        let body = json!({"choices":[{"message":message,"finish_reason":"stop"}]}).to_string();
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    }
    fn scheduled_call(id: usize, name: &str) -> Value {
        json!({"id":id.to_string(),"function":{"name":name,"arguments":"{}"}})
    }

    #[test]
    fn tool_scheduler_overlaps_reads_with_bounded_concurrency_and_ordered_results() {
        use std::sync::Condvar;
        let calls: Vec<_> = (0..10).map(|i| scheduled_call(i, "read_file")).collect();
        let gates = [
            (Mutex::new(0), Condvar::new()),
            (Mutex::new(0), Condvar::new()),
        ];
        let mut results = Vec::new();
        execute_tool_calls(
            &calls,
            &AtomicBool::new(false),
            &|call| {
                let id: usize = call["id"].as_str().unwrap().parse().unwrap();
                let (lock, ready) = &gates[usize::from(id >= 8)];
                let target = if id < 8 { 8 } else { 2 };
                let mut arrived = lock.lock().unwrap();
                *arrived += 1;
                ready.notify_all();
                let (_arrived, timeout) = ready
                    .wait_timeout_while(arrived, Duration::from_secs(3), |n| *n < target)
                    .unwrap();
                if timeout.timed_out() {
                    return Err("Read calls did not overlap in the expected batches.".into());
                }
                Ok(id.to_string())
            },
            |_| {},
            |call, result| {
                results.push((call["id"].as_str().unwrap().to_owned(), result?));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            results,
            (0..10)
                .map(|i| (i.to_string(), i.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tool_scheduler_keeps_mutations_as_barriers_and_continues_after_read_errors() {
        let names = [
            "write_file",
            "read_file",
            "list_files",
            "edit_file",
            "search_memories",
            "read_conversation",
            "run_command",
            "read_file",
            "request_restart",
            "spawn_builder_workstream",
            "unknown",
        ];
        let calls: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| scheduled_call(i, name))
            .collect();
        let phase = Mutex::new(0);
        let completed_reads = std::sync::atomic::AtomicUsize::new(0);
        let mut results = Vec::new();
        execute_tool_calls(
            &calls,
            &AtomicBool::new(false),
            &|call| {
                let id: usize = call["id"].as_str().unwrap().parse().unwrap();
                let mut phase = phase.lock().unwrap();
                match id {
                    0 => {
                        assert_eq!(*phase, 0);
                        *phase = 1;
                    }
                    1 | 2 => {
                        assert_eq!(*phase, 1);
                        completed_reads.fetch_add(1, Ordering::SeqCst);
                    }
                    3 => {
                        assert_eq!(completed_reads.load(Ordering::SeqCst), 2);
                        *phase = 2;
                    }
                    4 | 5 => {
                        assert_eq!(*phase, 2);
                        completed_reads.fetch_add(1, Ordering::SeqCst);
                    }
                    6 => {
                        assert_eq!(completed_reads.load(Ordering::SeqCst), 4);
                        *phase = 3;
                    }
                    7 => {
                        assert_eq!(*phase, 3);
                        *phase = 4;
                    }
                    8..=10 => {
                        assert_eq!(*phase, id - 4);
                        *phase += 1;
                    }
                    _ => unreachable!(),
                }
                if id == 1 {
                    Err("missing file".into())
                } else {
                    Ok(id.to_string())
                }
            },
            |_| {},
            |call, result| {
                results.push((call["id"].as_str().unwrap().to_owned(), result));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            results.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            (0..names.len()).map(|i| i.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(results[1].1, Err("missing file".into()));
        assert!(results[2..].iter().all(|(_, r)| r.is_ok()));
    }

    #[test]
    fn tool_scheduler_cancellation_skips_pending_reads_and_mutations() {
        let calls = [
            scheduled_call(0, "read_file"),
            scheduled_call(1, "list_files"),
            scheduled_call(2, "write_file"),
        ];
        let stop = AtomicBool::new(false);
        let mut results = Vec::new();
        let result = execute_tool_calls(
            &calls,
            &stop,
            &|_| panic!("Cancelled tools must not execute"),
            |_| {
                stop.store(true, Ordering::SeqCst);
            },
            |_, result| {
                results.push(result);
                Ok(())
            },
        );
        assert_eq!(result, Err("Stopped".into()));
        assert_eq!(results, vec![Err("Stopped".into()), Err("Stopped".into())]);
    }

    #[test]
    fn local_agent_executes_tools_and_restores_conversation() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let first = receive(&mut socket);
            assert_eq!(first["model"], "fixture-qwen");
            assert_eq!(first["messages"][1]["content"], "Create a greeting");
            assert!(first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "write_file"));
            respond(
                &mut socket,
                json!({"role":"assistant","content":"<|channel|>analysis<|message|>Write the greeting, then read it back.<|end|>","tool_calls":[{"id":"call1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"greeting.txt\",\"content\":\"hello\"}"}},{"id":"call2","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"greeting.txt\"}"}}]}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let second = receive(&mut socket);
            assert!(second["messages"][2].get("content").is_none());
            assert_eq!(second["messages"][3]["tool_call_id"], "call1");
            assert!(second["messages"][4]["content"]
                .as_str()
                .unwrap()
                .contains("hello"));
            respond(
                &mut socket,
                json!({"role":"assistant","content":"Created greeting.txt."}),
            );
            let (mut socket, _) = server.accept().unwrap();
            let resumed = receive(&mut socket);
            assert!(resumed["messages"][2].get("content").is_none());
            assert_eq!(resumed["messages"][3]["tool_call_id"], "call1");
            assert_eq!(resumed["messages"][4]["tool_call_id"], "call2");
            assert_eq!(resumed["messages"][5]["content"], "Created greeting.txt.");
            assert_eq!(resumed["messages"][6]["content"], "What did you create?");
            respond(
                &mut socket,
                json!({"role":"assistant","content":"greeting.txt"}),
            );
        });
        let mut w = Workstream::new(&root);
        w.model = MODEL_ID.into();
        w.draft = "Create a greeting".into();
        for prompt in ["Create a greeting", "What did you create?"] {
            w.draft = prompt.into();
            let (tx, rx) = std::sync::mpsc::sync_channel(256);
            let handle = start(cfg.clone(), &w, tx, egui::Context::default(), None).unwrap();
            loop {
                match rx.recv_timeout(Duration::from_secs(15)).unwrap().event {
                    Event::Session(s) => w.session = Some(s),
                    Event::Finished { status } => {
                        assert_eq!(status, "Ready");
                        break;
                    }
                    Event::Message { role, body, .. } if role == "Error" => panic!("{body}"),
                    _ => {}
                }
            }
            handle.finish();
        }
        fixture.join().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("greeting.txt")).unwrap(),
            "hello"
        );
    }
    #[test]
    fn server_models_are_discovered_and_selection_overrides_default() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "GET /v1/models HTTP/1.1\r\n");
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let body = json!({"data":[{"id":"gemma-4-26b-qat-4bit"},{"id":"gpt-oss-20b","max_output_tokens":6000,"capabilities":{"reasoning":true}},{"id":"gpt-oss-20b"}]}).to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            let (mut socket, _) = server.accept().unwrap();
            let payload = receive(&mut socket);
            assert_eq!(payload["model"], "gpt-oss-20b");
            assert_eq!(payload["max_tokens"], 6000);
            respond(&mut socket, json!({"role":"assistant","content":"OK"}));
        });
        let mut cfg = cfg;
        let catalog = discover_models(&cfg.local).unwrap();
        cfg.local.apply_catalog(catalog);
        assert_eq!(
            selected_config(&cfg.local, &selection_id("gpt-oss-20b")).output_limit(),
            6000
        );
        assert_eq!(
            cfg.local.available_models,
            vec!["gemma-4-26b-qat-4bit", "gpt-oss-20b"]
        );
        let choices = runner::model_choices(&cfg.local);
        for model in &cfg.local.available_models {
            assert!(choices.iter().any(|(id, _)| id == &selection_id(model)));
            assert_eq!(runner::provider(&selection_id(model)), "local");
        }
        let mut w = Workstream::new(&root);
        w.model = selection_id("gpt-oss-20b");
        w.draft = "Hi".into();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let handle = runner::start_agent(cfg, &w, tx, egui::Context::default(), None).unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(15)).unwrap().event {
                Event::Finished { status } => {
                    assert_eq!(status, "Ready");
                    break;
                }
                Event::Message { role, body, .. } if role == "Error" => panic!("{body}"),
                _ => {}
            }
        }
        handle.finish();
        fixture.join().unwrap();
    }

    #[test]
    fn interrupted_tool_batches_are_repaired_without_repeating_completed_tools() {
        let mut messages = vec![
            json!({"role":"assistant","tool_calls":[{"id":"done"},{"id":"interrupted"}]}),
            json!({"role":"tool","tool_call_id":"done","content":"Saved file"}),
        ];
        recover_tools(&mut messages);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["tool_call_id"], "interrupted");
        recover_tools(&mut messages);
        assert_eq!(messages.len(), 3);
    }
    #[test]
    fn local_api_errors_are_reported_and_credentials_are_redacted() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = config(&root, format!("http://{}", server.local_addr().unwrap()));
        let fixture = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            receive(&mut socket);
            let body = json!({"error":{"message":"fixture-key: model not available"}}).to_string();
            write!(socket,"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let mut w = Workstream::new(&root);
        w.model = MODEL_ID.into();
        w.draft = "Hi".into();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let handle = start(cfg, &w, tx, egui::Context::default(), None).unwrap();
        let mut error_seen = false;
        loop {
            match rx.recv_timeout(Duration::from_secs(15)).unwrap().event {
                Event::Message { role, body, .. } if role == "Error" => {
                    assert!(!body.contains("fixture-key"));
                    assert!(body.contains("model not available"));
                    error_seen = true;
                }
                Event::Finished { status } => {
                    assert_eq!(status, "Failed");
                    break;
                }
                _ => {}
            }
        }
        handle.finish();
        fixture.join().unwrap();
        assert!(error_seen);
    }
    #[test]
    fn supplied_configuration_is_imported_once_and_saved_privately() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let project = temp.0.canonicalize().unwrap();
        let seed = project.join(".harness/local-model.json");
        std::fs::create_dir_all(seed.parent().unwrap()).unwrap();
        std::fs::write(
            &seed,
            json!({"host":"http://192.168.1.35:8000/v1","api_key":"fixture-key","model":"Qwen-3"})
                .to_string(),
        )
        .unwrap();
        let store = crate::store::Store::open(project.join("state/history.sqlite3")).unwrap();
        let cfg = load(&store, &project);
        assert_eq!(cfg.model, "Qwen-3");
        assert_eq!(cfg.api_key, "fixture-key");
        assert!(!seed.exists());
        assert_eq!(load(&store, &project).model, "Qwen-3");
        assert_eq!(
            std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(runner::provider(MODEL_ID), "local");
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn local_shell_loads_login_path_from_a_minimal_gui_environment() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let bin = temp.0.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let executable = bin.join("skyforge-shell-fixture");
        std::fs::write(&executable, "#!/bin/sh\nprintf login-path-loaded").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            temp.0.join(".zprofile"),
            "export PATH=\"$ZDOTDIR/bin:$PATH\"\n",
        )
        .unwrap();
        let mut command = shell_command("skyforge-shell-fixture");
        command
            .env("ZDOTDIR", &temp.0)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .current_dir(&temp.0);
        let (ok, output, errors) = execute(
            command,
            None,
            &AtomicBool::new(false),
            Duration::from_secs(10),
            1024,
        )
        .unwrap();
        assert!(ok, "{}", String::from_utf8_lossy(&errors));
        assert_eq!(String::from_utf8_lossy(&output), "login-path-loaded");
    }

    #[test]
    fn list_files_accepts_optional_directory_and_excludes_ignored_files() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("README.md"), "project").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "source").unwrap();
        std::fs::write(root.join("src/nested/helper.rs"), "source").unwrap();
        std::fs::write(root.join("src/ignored.txt"), "ignored").unwrap();
        let stop = AtomicBool::new(false);
        for writable in [false, true] {
            for args in [json!({}), json!({"path":""}), json!({"path":"."})] {
                let output = tool("list_files", &args, &root, writable, None, &stop).unwrap();
                assert!(output.lines().any(|p| p == "README.md"));
                assert!(output.lines().any(|p| p == "src/main.rs"));
                assert!(!output.contains("ignored.txt"));
            }
            for path in [root.join("src"), PathBuf::from("src")] {
                let args = json!({"path":path});
                let message = json!({"role":"assistant","tool_calls":[{"function":{"name":"list_files","arguments":args.to_string()}}]});
                let normalized = normalize_with_tools(&message, &tools(writable, false)).unwrap();
                let accepted: Value = serde_json::from_str(
                    normalized["tool_calls"][0]["function"]["arguments"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let output = tool("list_files", &accepted, &root, writable, None, &stop).unwrap();
                let files: std::collections::BTreeSet<_> = output.lines().collect();
                assert_eq!(
                    files,
                    ["src/main.rs", "src/nested/helper.rs"]
                        .into_iter()
                        .collect()
                );
            }
            for path in ["missing", "README.md"] {
                assert!(tool(
                    "list_files",
                    &json!({"path":path}),
                    &root,
                    writable,
                    None,
                    &stop
                )
                .is_err());
            }
        }
        let invalid = json!({"role":"assistant","tool_calls":[{"function":{"name":"list_files","arguments":"{\"path\":42}"}}]});
        assert!(normalize_with_tools(&invalid, &tools(true, false)).is_err());
    }

    #[test]
    fn list_files_respects_workspace_access_rules() {
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        let outside = TempDir::new().unwrap();
        let directory = outside.0.canonicalize().unwrap();
        let file = directory.join("outside.txt");
        std::fs::write(&file, "outside").unwrap();
        let stop = AtomicBool::new(false);
        let args = json!({"path":directory});
        assert_eq!(
            tool("list_files", &args, &root, true, None, &stop).unwrap(),
            file.to_string_lossy()
        );
        assert!(tool("list_files", &args, &root, false, None, &stop).is_err());
        assert!(tool(
            "list_files",
            &json!({"path":".."}),
            &root,
            false,
            None,
            &stop
        )
        .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&directory, root.join("escape")).unwrap();
            assert!(tool(
                "list_files",
                &json!({"path":"escape"}),
                &root,
                false,
                None,
                &stop
            )
            .is_err());
            assert_eq!(
                tool(
                    "list_files",
                    &json!({"path":"escape"}),
                    &root,
                    true,
                    None,
                    &stop
                )
                .unwrap(),
                file.to_string_lossy()
            );
        }
    }

    #[test]
    fn read_file_batch_validates_model_arguments() {
        let normalize = |args: Value| {
            normalize_message(
                &json!({"role":"assistant","tool_calls":[{"function":{"name":"read_file","arguments":args}}]}),
                false,
                false,
            )
        };
        for args in [
            json!({"path":"a"}),
            json!({"paths":["a","b"],"start_line":2,"max_lines":3}),
        ] {
            assert!(normalize(args).is_ok());
        }
        for args in [
            json!({}),
            json!({"path":"a","paths":["b"]}),
            json!({"paths":[]}),
            json!({"paths":"a"}),
            json!({"paths":["a",42]}),
            json!({"paths":vec!["a";21]}),
        ] {
            assert!(normalize(args.clone()).is_err(), "{args}");
            assert!(tool(
                "read_file",
                &args,
                Path::new("/"),
                false,
                None,
                &AtomicBool::new(false)
            )
            .is_err());
        }
    }

    #[test]
    fn read_file_batch_preserves_ranges_errors_and_permissions() {
        let root = TempDir::new().unwrap();
        let root_path = root.0.canonicalize().unwrap();
        let outside = TempDir::new().unwrap();
        let external = outside.0.join("external.txt");
        std::fs::write(&external, "outside\nsecret").unwrap();
        std::fs::write(root_path.join("a.txt"), "first\nsecond\nthird").unwrap();
        std::fs::write(root_path.join("b.txt"), "one\ntwo\nthree").unwrap();
        std::fs::write(root_path.join("binary"), [0xff]).unwrap();
        let args = json!({"paths":["a.txt","missing","binary",external,"b.txt"],"start_line":2,"max_lines":1});
        let stop = AtomicBool::new(false);
        let output = tool("read_file", &args, &root_path, false, None, &stop).unwrap();
        assert!(output.starts_with("=== a.txt ===\n2: second"));
        assert!(output.contains("=== missing ===\nTool error:"));
        assert!(output.contains("=== binary ===\nTool error: File is not UTF-8 text"));
        assert!(output.contains("Path is outside the project."));
        assert!(!output.contains("secret"));
        assert!(output.ends_with("=== b.txt ===\n2: two"));
        assert!(tool("read_file", &args, &root_path, true, None, &stop)
            .unwrap()
            .contains("2: secret"));
        assert_eq!(
            tool(
                "read_file",
                &json!({"path":"a.txt","start_line":2,"max_lines":1}),
                &root_path,
                false,
                None,
                &stop
            )
            .unwrap(),
            "2: second"
        );
    }

    #[test]
    fn read_file_batch_retains_every_file_within_output_budget() {
        let root = TempDir::new().unwrap();
        let paths: Vec<_> = (0..20).map(|i| format!("file{i}.txt")).collect();
        for path in &paths {
            std::fs::write(root.0.join(path), "é".repeat(40_000)).unwrap();
        }
        let output = tool(
            "read_file",
            &json!({"paths":paths}),
            &root.0,
            true,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.len() <= TOOL_OUTPUT_LIMIT);
        assert_eq!(tool_output(&output), output);
        for path in &paths {
            assert!(output.contains(&format!("=== {path} ===")));
        }
        assert!(output.contains("[Output truncated]"));
    }

    #[test]
    fn builder_tools_access_files_outside_the_project() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = outside.0.join("builder-output.txt");
        let stop = AtomicBool::new(false);
        tool(
            "write_file",
            &json!({"path":path,"content":"before"}),
            &root.0,
            true,
            None,
            &stop,
        )
        .unwrap();
        tool(
            "edit_file",
            &json!({"path":path,"old_text":"before","new_text":"after"}),
            &root.0,
            true,
            None,
            &stop,
        )
        .unwrap();
        let read = tool(
            "read_file",
            &json!({"path":path}),
            &root.0,
            true,
            None,
            &stop,
        )
        .unwrap();
        assert!(read.contains("after"));
        assert!(tool(
            "read_file",
            &json!({"path":path}),
            &root.0,
            false,
            None,
            &stop
        )
        .is_err());
        assert!(tool(
            "edit_file",
            &json!({"path":path,"old_text":"after","new_text":"blocked"}),
            &root.0,
            false,
            None,
            &stop
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
    }

    #[test]
    #[cfg(unix)]
    fn builder_shell_writes_outside_the_project_and_reports_errors() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = outside.0.join("shell-output.txt");
        // The generated temporary path contains no shell metacharacters.
        let command = format!("printf verified > '{}'", path.display());
        let stop = AtomicBool::new(false);
        let output = tool(
            "run_command",
            &json!({"command":command}),
            &root.0,
            true,
            None,
            &stop,
        )
        .unwrap();
        assert!(output.starts_with("Command succeeded"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "verified");
        assert!(tool(
            "run_command",
            &json!({"command":"exit 0"}),
            &root.0,
            false,
            None,
            &stop
        )
        .is_err());
        let output = shell("exit 7", &root.0, None, &stop).unwrap();
        assert!(output.starts_with("Command failed"));
    }

    #[test]
    fn workspace_tools_reject_escape_and_read_only_edits() {
        let root = TempDir::new().unwrap();
        let root_path = root.0.canonicalize().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.0.join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(&outside.0, root_path.join("escape")).unwrap();
        assert!(workspace_path(&root_path, "escape/secret", false).is_err());
        assert!(workspace_path(&root_path, "escape/new-file", true).is_err());
        std::os::unix::fs::symlink(outside.0.join("missing"), root_path.join("dangling")).unwrap();
        assert!(workspace_path(&root_path, "dangling", true).is_err());
        assert!(workspace_path(&root_path, "nested/../../outside", true).is_err());
        let stop = AtomicBool::new(false);
        assert!(tool(
            "write_file",
            &json!({"path":"new.txt","content":"no"}),
            &root_path,
            false,
            None,
            &stop
        )
        .is_err());
        assert!(!root_path.join("new.txt").exists());
        assert!(tools(false, false)
            .iter()
            .all(|t| t["function"]["name"] != "run_command"));
        assert!(endpoint("file:///tmp").is_err());
        assert_eq!(
            endpoint("http://localhost:8000/v1/").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
    }
    #[test]
    fn local_helpers_can_be_cancelled_without_pipe_deadlock() {
        let stop = Arc::new(AtomicBool::new(false));
        let timer = stop.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            timer.store(true, Ordering::SeqCst);
        });
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30 & wait"]);
        let began = Instant::now();
        assert!(execute(cmd, None, &stop, Duration::from_secs(60), 1024).is_err());
        trigger.join().unwrap();
        assert!(began.elapsed() < Duration::from_secs(3));
    }
    #[test]
    #[ignore = "Requires the user's LAN server and a private configuration file"]
    fn live_local_agent_reads_workspace_file() {
        let seed: Config = serde_json::from_slice(
            &std::fs::read(std::env::var("HARNESS_LOCAL_TEST_CONFIG").unwrap()).unwrap(),
        )
        .unwrap();
        let temp = TempDir::new().unwrap();
        let root = temp.0.canonicalize().unwrap();
        std::fs::write(root.join("probe.txt"), "HARNESS_LOCAL_AGENT_OK").unwrap();
        let mut cfg = config(&root, seed.host);
        cfg.local.api_key = seed.api_key;
        let served_model = seed.model;
        cfg.local.model = "deliberately-unavailable-default".into();
        let mut w = Workstream::new(&root);
        w.model = selection_id(&served_model);
        w.draft = "Use read_file to read probe.txt and return its contents exactly.".into();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let handle = start(cfg, &w, tx, egui::Context::default(), None).unwrap();
        let mut tool_seen = false;
        let mut answer_seen = false;
        loop {
            match rx.recv_timeout(Duration::from_secs(310)).unwrap().event {
                Event::Message { role, body, .. } if role == "Tool" => {
                    tool_seen |= body.contains("HARNESS_LOCAL_AGENT_OK");
                }
                Event::Message { role, body, .. } if role == "Agent" => {
                    answer_seen |= body.contains("HARNESS_LOCAL_AGENT_OK");
                }
                Event::Message { role, body, .. } if role == "Error" => panic!("{body}"),
                Event::Finished { status } => {
                    assert_eq!(status, "Ready");
                    break;
                }
                _ => {}
            }
        }
        handle.finish();
        assert!(tool_seen && answer_seen);
    }
}
