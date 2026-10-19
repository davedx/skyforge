use crate::store::Workstream;
use base64::Engine;
use eframe::egui;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum Event {
    Session(String),
    Activity {
        id: String,
        body: String,
    },
    Message {
        item: String,
        role: String,
        body: String,
    },
    Finished {
        status: String,
    },
}

pub struct Envelope {
    pub workstream: String,
    pub event: Event,
}

pub struct RunHandle {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) join: Option<thread::JoinHandle<()>>,
}

impl RunHandle {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn finish(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct AgentConfig {
    pub local: crate::local::Config,
    pub binary: String,
    pub claude_binary: String,
    pub search: bool,
    pub writable: bool,
}

pub fn provider(model: &str) -> &'static str {
    if crate::local::is_local(model) {
        "local"
    } else if is_claude(model) {
        "claude"
    } else {
        "codex"
    }
}

pub fn is_claude(model: &str) -> bool {
    model.starts_with("claude:")
}

pub fn model_choices(local: &crate::local::Config) -> Vec<(String, String)> {
    let mut choices = vec![(String::new(), "Codex · Default model".into())];
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".codex")));
    if let Some(cache) = home
        .and_then(|p| std::fs::read(p.join("models_cache.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    {
        if let Some(models) = cache["models"].as_array() {
            for model in models {
                if model["visibility"] == "hide" {
                    continue;
                }
                if let Some(slug) = model["slug"].as_str() {
                    let label = model["display_name"].as_str().unwrap_or(slug);
                    choices.push((slug.into(), format!("Codex · {label}")));
                }
            }
        }
    }
    for (model, label) in [
        ("claude:sonnet", "Anthropic Claude · Sonnet"),
        ("claude:opus", "Anthropic Claude · Opus"),
        ("claude:haiku", "Anthropic Claude · Haiku"),
    ] {
        choices.push((model.into(), label.into()));
    }
    for model in &local.available_models {
        choices.push((
            crate::local::selection_id(model),
            format!("Local · {model}"),
        ));
    }
    let default = local.model.trim();
    if !default.is_empty() && !local.available_models.iter().any(|model| model == default) {
        choices.push((
            crate::local::selection_id(default),
            format!("Local · {default}"),
        ));
    } else if default.is_empty() && local.available_models.is_empty() {
        choices.push((
            crate::local::MODEL_ID.into(),
            "Local · OpenAI compatible model".into(),
        ));
    }
    choices
}

pub fn agent_command(config: &AgentConfig, w: &Workstream) -> Command {
    let thinker = w.mode == crate::store::Mode::Thinker;
    let writable = config.writable && !thinker;
    if is_claude(&w.model) {
        let mut cmd = Command::new(&config.claude_binary);
        cmd.args([
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .args(["--model", w.model.trim_start_matches("claude:")])
        .args([
            "--permission-mode",
            if writable {
                "bypassPermissions"
            } else {
                "dontAsk"
            },
        ]);
        if thinker {
            cmd.env_remove(crate::restart::REQUEST_ENV)
                .env_remove(crate::restart::EXECUTABLE_ENV);
            cmd.args(["--system-prompt", &crate::thinker::instructions()])
                .args([
                    "--tools",
                    if config.search {
                        "WebSearch,WebFetch"
                    } else {
                        ""
                    },
                ])
                .args([
                    "--strict-mcp-config",
                    "--allowedTools",
                    "mcp__harness_thinker__*,WebSearch,WebFetch",
                ]);
            if let Some(path) = &w.instructions_path {
                let profile = path.parent().unwrap();
                cmd.arg("--mcp-config")
                    .arg(profile.join(format!("{}.mcp.json", w.id)));
                cmd.current_dir(profile);
            }
        } else if writable {
            cmd.args(["--settings", r#"{"sandbox":{"enabled":false}}"#]);
        } else {
            // Exclude execution and editing entirely in read-only mode.
            cmd.args(["--tools", "Read,Glob,Grep,WebSearch,WebFetch"]);
        }
        if !config.search {
            cmd.args(["--disallowedTools", "WebSearch,WebFetch"]);
        }
        if let Some(session) = w.session.as_ref().filter(|_| !thinker) {
            cmd.args(["--resume", session]);
        }
        if !thinker {
            add_wake_tools(&mut cmd, w, true);
            cmd.current_dir(&w.project);
        }
        return cmd;
    }
    let mut cmd = Command::new(&config.binary);
    cmd.arg("exec")
        .args(["--json", "--color", "never", "--skip-git-repo-check"])
        .args([
            "--sandbox",
            if writable {
                "danger-full-access"
            } else {
                "read-only"
            },
        ])
        .args(["-c", "approval_policy=\"never\""])
        .args([
            "-c",
            if config.search {
                "web_search=\"live\""
            } else {
                "web_search=\"disabled\""
            },
        ])
        .args(["-c", "shell_environment_policy.inherit=\"all\""]);
    if !w.model.trim().is_empty() {
        cmd.args(["--model", w.model.trim()]);
    }
    if thinker {
        cmd.env_remove(crate::restart::REQUEST_ENV)
            .env_remove(crate::restart::EXECUTABLE_ENV);
        if let Some(path) = &w.instructions_path {
            let profile = path.parent().unwrap();
            cmd.env("CODEX_HOME", profile.join("codex"));
            cmd.args([
                "-c",
                &format!(
                    "model_instructions_file={}",
                    serde_json::to_string(path).unwrap()
                ),
            ]);
            cmd.args([
                "-c",
                &format!(
                    "model_catalog_json={}",
                    serde_json::to_string(&profile.join(format!("{}.models.json", w.id))).unwrap()
                ),
            ]);
            cmd.args(["-c", "developer_instructions=\"\""])
                .args(["-c", "features.shell_tool=false"])
                .args(["-c", "features.unified_exec=false"])
                .args(["-c", "features.apply_patch_freeform=false"])
                .args(["-c", "features.multi_agent=false"])
                .args(["-c", "features.goals=false"])
                .args(["-c", "features.tool_search=false"])
                .args(["-c", "features.skip_host_skill_discovery=true"])
                .args(["-c", "features.apps=false"])
                .args(["-c", "features.plugins=false"])
                .args(["-c", "features.code_mode=false"])
                .args(["-c", "features.shell_snapshot=false"])
                .args(["-c", "features.memories=false"])
                .args(["-c", "features.remote_plugin=false"]);
            cmd.args([
                "-c",
                &format!(
                    "mcp_servers.harness_thinker.command={}",
                    serde_json::to_string(&std::env::current_exe().unwrap()).unwrap()
                ),
            ]);
            cmd.args([
                "-c",
                &format!(
                    "mcp_servers.harness_thinker.args={}",
                    serde_json::json!([
                        "--thinker-tools",
                        w.memory_db,
                        w.project,
                        w.thinker_project.as_deref().unwrap_or(""),
                        w.id,
                        w.invocation_turn.as_deref().unwrap_or(""),
                        serde_json::to_string(&w.delegation_endpoint).unwrap()
                    ])
                ),
            ]);
            cmd.args(["-c", "mcp_servers.harness_thinker.required=true"]);
            cmd.current_dir(profile);
        }
    }
    if !thinker {
        add_wake_tools(&mut cmd, w, false);
    }
    if let Some(session) = w.session.as_ref().filter(|_| !thinker) {
        cmd.arg("resume").arg(session);
    }
    for image in &w.attachments {
        cmd.arg("--image").arg(image);
    }
    cmd.arg("-");
    if !thinker {
        cmd.current_dir(&w.project);
    }
    cmd
}

fn add_wake_tools(cmd: &mut Command, w: &Workstream, claude: bool) {
    let Some(endpoint) = &w.delegation_endpoint else {
        return;
    };
    let args = serde_json::json!([
        "--wake-tools",
        w.id,
        w.invocation_turn.as_deref().unwrap_or(""),
        serde_json::to_string(endpoint).unwrap()
    ]);
    let executable = std::env::current_exe().unwrap();
    if claude {
        cmd.args([
            "--mcp-config",
            &serde_json::json!({"mcpServers":{"harness_wakes":{"command":executable,"args":args}}})
                .to_string(),
        ]);
        cmd.args(["--allowedTools", "mcp__harness_wakes__*"]);
    } else {
        cmd.args([
            "-c",
            &format!(
                "mcp_servers.harness_wakes.command={}",
                serde_json::to_string(&executable).unwrap()
            ),
        ]);
        cmd.args(["-c", &format!("mcp_servers.harness_wakes.args={args}")]);
        cmd.args(["-c", "mcp_servers.harness_wakes.required=true"]);
    }
}

pub fn start_agent(
    mut config: AgentConfig,
    w: &Workstream,
    tx: SyncSender<Envelope>,
    ctx: egui::Context,
    restart_path: Option<&std::path::Path>,
) -> Result<RunHandle, String> {
    let thinker = w.mode == crate::store::Mode::Thinker;
    if thinker && (w.instructions_path.is_none() || w.memory_db.is_none()) {
        return Err("The Chatbot profile has not been prepared. No agent was started.".into());
    }
    if thinker {
        config.writable = false;
    }
    let restart_path = restart_path.filter(|_| !thinker);
    if crate::local::is_local(&w.model) {
        return crate::local::start(config, w, tx, ctx, restart_path);
    }
    let prompt = agent_prompt(w, restart_path.is_some())?;
    let mut command = agent_command(&config, w);
    if let Some(path) = restart_path {
        command.env(crate::restart::REQUEST_ENV, path);
        command.env(
            crate::restart::EXECUTABLE_ENV,
            std::env::current_exe().map_err(|e| e.to_string())?,
        );
    }
    Ok(start(command, Some(prompt), true, w.id.clone(), tx, ctx))
}

fn agent_prompt(w: &Workstream, restart_available: bool) -> Result<String, String> {
    let mut draft = if w.mode == crate::store::Mode::Thinker {
        format!("{}\n\nCurrent user message:\n{}", w.context, w.draft)
    } else if restart_available {
        format!("{}\n\n[Skyforge integration]\nFor user-requested Skyforge restarts, run `\"$HARNESS_EXECUTABLE\" --request-restart`. Skyforge restarts itself after all active runs finish. Finish your task and response normally before exiting.", w.draft)
    } else {
        w.draft.clone()
    };
    if w.delegation_endpoint.is_some() {
        draft.push_str("\n\n[Skyforge wake timers]\n");
        draft.push_str(crate::wakes::INSTRUCTIONS);
    }
    if !is_claude(&w.model) {
        return Ok(draft);
    }
    let mut content = vec![serde_json::json!({"type": "text", "text": draft})];
    for path in &w.attachments {
        let bytes = std::fs::read(path).map_err(|e| format!("Cannot read attachment: {e}"))?;
        let media_type = match image::guess_format(&bytes).map_err(|e| e.to_string())? {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            _ => return Err("Claude supports PNG, JPEG, and WebP attachments.".into()),
        };
        content.push(serde_json::json!({"type":"image", "source": {
            "type":"base64", "media_type":media_type,
            "data":base64::engine::general_purpose::STANDARD.encode(bytes)
        }}));
    }
    Ok(format!(
        "{}\n",
        serde_json::json!({"type":"user", "message": {
            "role":"user", "content":content
        }})
    ))
}

#[cfg(test)]
pub fn start_shell(
    command: &str,
    w: &Workstream,
    tx: SyncSender<Envelope>,
    ctx: egui::Context,
) -> RunHandle {
    let mut cmd = Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()));
    cmd.args(["-lc", command]).current_dir(&w.project);
    start(cmd, None, false, w.id.clone(), tx, ctx)
}

pub(crate) fn send(tx: &SyncSender<Envelope>, ctx: &egui::Context, id: &str, event: Event) {
    // Wake before a bounded send, so a full queue cannot strand a sleeping GUI.
    ctx.request_repaint();
    let _ = tx.send(Envelope {
        workstream: id.into(),
        event,
    });
    ctx.request_repaint();
}

pub(crate) fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[Output truncated]", &text[..end])
}

pub fn parse_event(value: &Value) -> Option<Event> {
    let kind = value["type"].as_str()?;
    if kind == "thread.started" {
        return value["thread_id"]
            .as_str()
            .map(|s| Event::Session(s.into()));
    }
    if kind == "error" || kind == "turn.failed" {
        let message = value["message"]
            .as_str()
            .or_else(|| value["error"]["message"].as_str())
            .unwrap_or("Agent turn failed");
        return Some(Event::Message {
            item: "error".into(),
            role: "Error".into(),
            body: clip(message, 65536),
        });
    }
    if !kind.starts_with("item.") {
        return None;
    }
    let item = &value["item"];
    let id = item["id"].as_str().unwrap_or("activity").to_owned();
    let (role, body) = match item["type"].as_str().unwrap_or("") {
        "agent_message" => ("Agent", item["text"].as_str().unwrap_or("").to_owned()),
        "reasoning" => ("Thinking", item["text"].as_str().unwrap_or("").to_owned()),
        "command_execution" => (
            "Tool",
            format!(
                "$ {}\n\n{}\n\nStatus: {}{}",
                item["command"].as_str().unwrap_or("command"),
                item["aggregated_output"].as_str().unwrap_or(""),
                item["status"].as_str().unwrap_or("running"),
                item["exit_code"]
                    .as_i64()
                    .map(|c| format!(" · exit {c}"))
                    .unwrap_or_default()
            ),
        ),
        "file_change" => (
            "Files",
            format!(
                "File changes\n{}",
                serde_json::to_string_pretty(&item["changes"]).unwrap_or_default()
            ),
        ),
        "web_search" => (
            "Search",
            format!(
                "Web search: {}",
                item["query"].as_str().unwrap_or("Searching the web")
            ),
        ),
        _ => (
            "Tool",
            serde_json::to_string_pretty(item).unwrap_or_default(),
        ),
    };
    let body = clip(&body, 65536);
    if kind == "item.completed" {
        Some(Event::Message {
            item: id,
            role: role.into(),
            body,
        })
    } else {
        Some(Event::Activity { id, body })
    }
}

fn parse_events(value: &Value) -> Vec<Event> {
    match value["type"].as_str().unwrap_or("") {
        "system" if value["subtype"] == "init" => value["session_id"]
            .as_str()
            .map(|s| vec![Event::Session(s.into())])
            .unwrap_or_default(),
        "assistant" | "user" => {
            let mut events = Vec::new();
            let message = &value["message"];
            let id = message["id"].as_str().unwrap_or("claude");
            if let Some(blocks) = message["content"].as_array() {
                for (index, block) in blocks.iter().enumerate() {
                    let item = format!("{id}-{index}");
                    match block["type"].as_str().unwrap_or("") {
                        "text" | "thinking" => {
                            let thinking = block["type"] == "thinking";
                            let key = if thinking { "thinking" } else { "text" };
                            if let Some(body) = block[key].as_str() {
                                events.push(Event::Message {
                                    item,
                                    role: if thinking { "Thinking" } else { "Agent" }.into(),
                                    body: clip(body, 65536),
                                });
                            }
                        }
                        "tool_use" => events.push(Event::Activity {
                            id: block["id"].as_str().unwrap_or(&item).into(),
                            body: clip(
                                &format!(
                                    "{}\n{}",
                                    block["name"].as_str().unwrap_or("Tool"),
                                    block["input"]
                                ),
                                65536,
                            ),
                        }),
                        "tool_result" => {
                            let body = block["content"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| block["content"].to_string());
                            events.push(Event::Message {
                                item: block["tool_use_id"].as_str().unwrap_or(&item).into(),
                                role: if block["is_error"] == true {
                                    "Error"
                                } else {
                                    "Tool"
                                }
                                .into(),
                                body: clip(&body, 65536),
                            });
                        }
                        _ => {}
                    }
                }
            }
            events
        }
        "result" => {
            let mut events = Vec::new();
            if value["is_error"] == true {
                let body = value["result"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value["errors"].to_string());
                events.push(Event::Message {
                    item: "claude-error".into(),
                    role: "Error".into(),
                    body: clip(&body, 65536),
                });
            }
            if let Some(denials) = value["permission_denials"]
                .as_array()
                .filter(|d| !d.is_empty())
            {
                events.push(Event::Message {
                    item: "claude-permissions".into(),
                    role: "Diagnostics".into(),
                    body: clip(
                        &format!(
                            "Claude could not run these tools with the current permissions:\n{}",
                            serde_json::to_string_pretty(denials).unwrap_or_default()
                        ),
                        65536,
                    ),
                });
            }
            events
        }
        _ => parse_event(value).into_iter().collect(),
    }
}

fn start(
    mut cmd: Command,
    prompt: Option<String>,
    json: bool,
    id: String,
    tx: SyncSender<Envelope>,
    ctx: egui::Context,
) -> RunHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let join = thread::spawn(move || {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if prompt.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Give each run its own process group, including shell-tool descendants.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                send(
                    &tx,
                    &ctx,
                    &id,
                    Event::Message {
                        item: "spawn".into(),
                        role: "Error".into(),
                        body: format!(
                            "Cannot start process: {e}. Check the executable in Settings."
                        ),
                    },
                );
                send(
                    &tx,
                    &ctx,
                    &id,
                    Event::Finished {
                        status: "Failed".into(),
                    },
                );
                return;
            }
        };
        let pid = child.id();
        let failed = Arc::new(AtomicBool::new(false));
        let mut readers = vec![];
        // Keep pipe readers separate from supervision and prompt writing. A large prompt
        // or command output must not prevent cancellation or deadlock on a full pipe.
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = prompt.unwrap_or_default();
            readers.push(thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            }));
        }
        for (reader, stderr) in [
            (
                child
                    .stdout
                    .take()
                    .map(|r| Box::new(r) as Box<dyn std::io::Read + Send>),
                false,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|r| Box::new(r) as Box<dyn std::io::Read + Send>),
                true,
            ),
        ] {
            let Some(reader) = reader else {
                continue;
            };
            let (tx, ctx, id, failed) = (tx.clone(), ctx.clone(), id.clone(), failed.clone());
            readers.push(thread::spawn(move || {
                let mut output = String::new();
                let mut last_update = Instant::now() - Duration::from_secs(1);
                // read_line is limited via take on each line to bound malformed JSON/tool output.
                let mut reader = BufReader::new(reader);
                loop {
                    let mut bytes = Vec::new();
                    let result = std::io::Read::by_ref(&mut reader)
                        .take(1024 * 1024)
                        .read_until(b'\n', &mut bytes);
                    match result {
                        Ok(0) | Err(_) => break,
                        _ => {}
                    }
                    let line = String::from_utf8_lossy(&bytes);
                    if json && !stderr {
                        if let Ok(value) = serde_json::from_str::<Value>(&line) {
                            if matches!(value["type"].as_str(), Some("error" | "turn.failed"))
                                || (value["type"] == "result" && value["is_error"] == true)
                            {
                                failed.store(true, Ordering::SeqCst);
                            }
                            for event in parse_events(&value) {
                                send(&tx, &ctx, &id, event);
                            }
                            continue;
                        }
                    }
                    let changed = output.len() < 65536;
                    if changed {
                        output.push_str(&clip(&line, 65536 - output.len()));
                    }
                    if changed && last_update.elapsed() >= Duration::from_millis(100) {
                        last_update = Instant::now();
                        send(
                            &tx,
                            &ctx,
                            &id,
                            Event::Activity {
                                id: if stderr { "stderr" } else { "stdout" }.into(),
                                body: output.clone(),
                            },
                        );
                    }
                }
                if !output.is_empty() {
                    send(
                        &tx,
                        &ctx,
                        &id,
                        Event::Message {
                            item: if stderr { "stderr" } else { "stdout" }.into(),
                            role: if stderr { "Diagnostics" } else { "Tool" }.into(),
                            body: output,
                        },
                    );
                }
            }));
        }
        let status = loop {
            if stop.load(Ordering::SeqCst) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
                let _ = child.kill();
                thread::sleep(Duration::from_millis(200));
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                break "Stopped";
            }
            match child.try_wait() {
                Ok(Some(exit)) => {
                    break if exit.success() && !failed.load(Ordering::SeqCst) {
                        "Complete"
                    } else {
                        "Failed"
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break "Failed";
                }
                Ok(None) => thread::sleep(Duration::from_millis(40)),
            }
        };
        // Reap remaining descendants holding output pipes after the root process exits.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(100));
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        for reader in readers {
            let _ = reader.join();
        }
        send(
            &tx,
            &ctx,
            &id,
            Event::Finished {
                status: status.into(),
            },
        );
    });
    RunHandle {
        cancelled,
        join: Some(join),
    }
}

pub fn find_codex() -> String {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("codex");
            if p.is_file() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    // Finder-launched apps have a limited PATH. Locate the existing extension's CLI
    // as a prototype convenience; users can choose a standalone CLI in Settings.
    if let Some(home) = dirs::home_dir() {
        let root = home.join(".vscode/extensions");
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut candidates = entries
                .flatten()
                .filter_map(|entry| {
                    if !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("openai.chatgpt-")
                    {
                        return None;
                    }
                    let arch = if cfg!(target_arch = "aarch64") {
                        "macos-aarch64"
                    } else {
                        "macos-x86_64"
                    };
                    let p = entry.path().join("bin").join(arch).join("codex");
                    p.is_file().then_some(p)
                })
                .collect::<Vec<_>>();
            candidates.sort();
            if let Some(p) = candidates.pop() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    "codex".into()
}

pub fn find_claude() -> String {
    let mut directories = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        directories.extend([home.join(".local/bin"), home.join(".npm-global/bin")]);
    }
    directories.extend(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
    directories
        .into_iter()
        .map(|p| p.join("claude"))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "claude".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_cli_wake_tools_are_scoped_and_preserve_read_only_permissions() {
        let mut w = Workstream::new(std::path::Path::new("/tmp"));
        w.invocation_turn = Some("trusted-turn".into());
        w.delegation_endpoint = Some(crate::delegation_ipc::Endpoint {
            address: "127.0.0.1:12345".parse().unwrap(),
            token: "test-capability".into(),
        });
        w.session = Some("existing-session".into());
        let config = AgentConfig {
            binary: "codex".into(),
            claude_binary: "claude".into(),
            local: Default::default(),
            search: false,
            writable: false,
        };
        let command = agent_command(&config, &w);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|a| a == ["--sandbox", "read-only"]));
        let raw = args
            .iter()
            .find_map(|a| a.strip_prefix("mcp_servers.harness_wakes.args="))
            .unwrap();
        let tool_args: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(tool_args[0], "--wake-tools");
        assert_eq!(tool_args[1], w.id);
        assert_eq!(tool_args[2], "trusted-turn");
        assert!(args.windows(2).any(|a| a == ["resume", "existing-session"]));
        assert!(agent_prompt(&w, false)
            .unwrap()
            .contains(crate::wakes::INSTRUCTIONS));
        w.model = "claude:sonnet".into();
        let command = agent_command(&config, &w);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|a| a == ["--tools", "Read,Glob,Grep,WebSearch,WebFetch"]));
        assert!(args
            .windows(2)
            .any(|a| a == ["--permission-mode", "dontAsk"]));
        assert!(args
            .windows(2)
            .any(|a| a == ["--allowedTools", "mcp__harness_wakes__*"]));
        let raw = args.windows(2).find(|a| a[0] == "--mcp-config").unwrap();
        let config: Value = serde_json::from_str(&raw[1]).unwrap();
        assert_eq!(config["mcpServers"]["harness_wakes"]["args"], tool_args);
    }

    #[test]
    fn local_model_choices_share_one_entry_with_the_configured_default() {
        let mut local = crate::local::Config {
            model: " deepseek-first ".into(),
            available_models: vec!["deepseek-first".into(), "deepseek-second".into()],
            ..Default::default()
        };
        let local_choices = |config: &crate::local::Config| {
            model_choices(config)
                .into_iter()
                .filter(|(id, _)| crate::local::is_local(id))
                .collect::<Vec<_>>()
        };
        let choices = local_choices(&local);
        assert_eq!(choices.len(), 2);
        let canonical = crate::local::canonical_selection(&local, crate::local::MODEL_ID);
        assert_eq!(choices.iter().filter(|(id, _)| id == &canonical).count(), 1);
        assert_eq!(
            crate::local::selected_config(&local, &canonical).model,
            "deepseek-first"
        );

        local.available_models.clear();
        assert_eq!(
            local_choices(&local),
            vec![(canonical, "Local · deepseek-first".into())]
        );
        local.available_models.push("deepseek-second".into());
        assert_eq!(local_choices(&local).len(), 2); // A manual, undiscovered default remains usable.
        local.model.clear();
        assert_eq!(local_choices(&local).len(), 1);
        local.available_models.clear();
        assert_eq!(local_choices(&local)[0].0, crate::local::MODEL_ID);
    }

    #[test]
    fn builder_has_full_access_and_codex_read_only_remains_restricted() {
        let mut w = Workstream::new(std::path::Path::new("/tmp"));
        let mut config = AgentConfig {
            binary: "codex".into(),
            claude_binary: "claude".into(),
            local: Default::default(),
            search: true,
            writable: true,
        };
        let args = |config: &AgentConfig, w: &Workstream| {
            agent_command(config, w)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert!(args(&config, &w)
            .windows(2)
            .any(|a| a == ["--sandbox", "danger-full-access"]));
        config.writable = false;
        assert!(args(&config, &w)
            .windows(2)
            .any(|a| a == ["--sandbox", "read-only"]));
        config.writable = true;
        w.model = "claude:sonnet".into();
        let builder = args(&config, &w);
        assert!(builder
            .windows(2)
            .any(|a| a == ["--permission-mode", "bypassPermissions"]));
        assert!(builder
            .windows(2)
            .any(|a| a == ["--settings", r#"{"sandbox":{"enabled":false}}"#]));
        config.writable = false;
        assert!(args(&config, &w)
            .windows(2)
            .any(|a| a == ["--permission-mode", "dontAsk"]));
    }

    #[test]
    fn thinker_cli_profiles_replace_instructions_and_exclude_builder_sessions_and_tools() {
        let mut w = Workstream::new(std::path::Path::new("/tmp"));
        w.mode = crate::store::Mode::Thinker;
        w.context = "Saved conversational context".into();
        w.draft = "Discuss options".into();
        w.session = Some("builder-session-must-not-resume".into());
        w.instructions_path = Some("/tmp/chatbot/instructions.md".into());
        w.memory_db = Some("/tmp/history.sqlite3".into());
        let config = AgentConfig {
            binary: "codex".into(),
            claude_binary: "claude".into(),
            local: Default::default(),
            search: true,
            writable: true,
        };
        let command = agent_command(&config, &w);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"read-only".into()));
        for required in [
            "features.shell_tool=false",
            "features.unified_exec=false",
            "features.multi_agent=false",
            "features.apps=false",
            "features.plugins=false",
        ] {
            assert!(args.iter().any(|arg| arg == required));
        }
        assert!(args.iter().any(|a| a.starts_with("model_catalog_json=")));
        assert!(!args.contains(&"resume".into()));
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/tmp/chatbot"))
        );
        let prompt = agent_prompt(&w, true).unwrap();
        assert!(prompt.contains("Saved conversational context"));
        assert!(prompt.contains("Discuss options"));
        assert!(!prompt.contains("request-restart"));
        w.model = "claude:sonnet".into();
        let command = agent_command(&config, &w);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--strict-mcp-config".into()));
        assert!(args
            .iter()
            .any(|a| a.starts_with(crate::thinker::SYSTEM_PROMPT)));
        assert!(!args.contains(&"--resume".into()));
        assert!(!args.contains(&"Read,Glob,Grep,Edit,Write".into()));
        assert!(args.contains(&"WebSearch,WebFetch".into()));
    }

    #[test]
    #[cfg(unix)]
    fn claude_run_sends_images_resumes_and_reports_streamed_events() {
        use std::os::unix::fs::PermissionsExt;
        let data = std::env::temp_dir().join(format!("harness-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let executable = data.join("claude-fixture");
        std::fs::write(&executable, r#"#!/bin/sh
printf '%s\n' "$@" > arguments.txt
cat > input.json
cat <<'JSON'
{"type":"system","subtype":"init","session_id":"claude-session"}
{"type":"assistant","message":{"id":"msg-1","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"notes.md"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"File contents"}]}}
{"type":"assistant","message":{"id":"msg-2","content":[{"type":"text","text":"Claude response"}]}}
{"type":"result","is_error":false,"result":"Claude response"}
JSON
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let image_path = data.join("image.png");
        image::RgbaImage::new(1, 1).save(&image_path).unwrap();
        let mut w = Workstream::new(&data);
        w.model = "claude:sonnet".into();
        w.draft = "private prompt".into();
        w.session = Some("claude-session".into());
        w.attachments = vec![image_path.to_string_lossy().into_owned()];
        let config = AgentConfig {
            local: Default::default(),
            binary: "must-not-use-codex".into(),
            claude_binary: executable.to_string_lossy().into_owned(),
            writable: true,
            search: true,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let run = start_agent(config, &w, tx, egui::Context::default(), None).unwrap();
        let (mut session, mut tool, mut response) = (false, false, 0);
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap().event {
                Event::Session(id) => session = id == "claude-session",
                Event::Message { role, body, .. } if role == "Agent" => {
                    assert_eq!(body, "Claude response");
                    response += 1;
                }
                Event::Message { item, role, body } if role == "Tool" => {
                    tool = item == "tool-1" && body == "File contents";
                }
                Event::Finished { status } => {
                    assert_eq!(status, "Complete");
                    break;
                }
                _ => {}
            }
        }
        run.finish();
        assert!(session && tool);
        assert_eq!(
            response, 1,
            "The final result must not duplicate the assistant message"
        );
        let arguments = std::fs::read_to_string(data.join("arguments.txt")).unwrap();
        assert!(arguments.contains("--resume\nclaude-session"));
        assert!(arguments.contains("--model\nsonnet"));
        assert!(!arguments.contains("private prompt"));
        assert!(arguments.contains("--permission-mode\nbypassPermissions"));
        assert!(arguments.contains(r#"{"sandbox":{"enabled":false}}"#));
        let input: Value =
            serde_json::from_slice(&std::fs::read(data.join("input.json")).unwrap()).unwrap();
        assert_eq!(input["message"]["content"][0]["text"], "private prompt");
        let source = &input["message"]["content"][1]["source"];
        assert_eq!(source["media_type"], "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(source["data"].as_str().unwrap())
                .unwrap(),
            std::fs::read(&image_path).unwrap()
        );
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn claude_read_only_excludes_execution_and_errors_are_visible() {
        let mut w = Workstream::new(std::path::Path::new("/tmp"));
        w.model = "claude:opus".into();
        let command = agent_command(
            &AgentConfig {
                local: Default::default(),
                binary: "codex".into(),
                claude_binary: "claude".into(),
                search: false,
                writable: false,
            },
            &w,
        );
        let args = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|a| a == ["--tools", "Read,Glob,Grep,WebSearch,WebFetch"]));
        assert!(args
            .windows(2)
            .any(|a| a == ["--disallowedTools", "WebSearch,WebFetch"]));
        assert!(!args
            .iter()
            .any(|a| a.contains("Bash") || a.contains("Edit") || a.contains("Write")));
        let events = parse_events(
            &serde_json::json!({"type":"result","is_error":true,"errors":["Authentication required"]}),
        );
        assert!(
            matches!(&events[0], Event::Message { role, body, .. } if role == "Error" && body.contains("Authentication required"))
        );
    }

    #[test]
    fn shell_streams_both_pipes_and_reports_nonzero_exit() {
        let w = Workstream::new(std::path::Path::new("/tmp"));
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let run = start_shell(
            "printf 'stdout-marker\\n'; printf 'stderr-marker\\n' >&2; exit 7",
            &w,
            tx,
            egui::Context::default(),
        );
        let mut bodies = String::new();
        loop {
            let envelope = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Shell must finish without blocking on pipes");
            match envelope.event {
                Event::Message { body, .. } => bodies.push_str(&body),
                Event::Finished { status } => {
                    assert_eq!(status, "Failed");
                    break;
                }
                _ => {}
            }
        }
        run.finish();
        assert!(bodies.contains("stdout-marker") && bodies.contains("stderr-marker"));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_stops_shell_and_child_process() {
        let w = Workstream::new(std::path::Path::new("/tmp"));
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let run = start_shell(
            "sleep 30 & printf 'CHILD:%s\\n' $!; wait",
            &w,
            tx,
            egui::Context::default(),
        );
        let child_pid = loop {
            let envelope = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if let Event::Activity { body, .. } = envelope.event {
                if let Some(pid) = body
                    .lines()
                    .find_map(|l| l.strip_prefix("CHILD:"))
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    break pid;
                }
            }
        };
        run.cancel();
        loop {
            if let Event::Finished { status } =
                rx.recv_timeout(Duration::from_secs(5)).unwrap().event
            {
                assert_eq!(status, "Stopped");
                break;
            }
        }
        run.finish();
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(child_pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            unsafe { libc::kill(child_pid, 0) },
            -1,
            "Descendant must not keep running after Stop"
        );
    }

    #[test]
    fn translates_real_cli_events_and_keeps_tool_status() {
        let event = parse_event(&serde_json::json!({"type":"item.completed","item": {
            "id":"item_1","type":"command_execution","command":"cargo test","aggregated_output":"passed",
            "exit_code":0,"status":"completed"}})).unwrap();
        assert!(
            matches!(event, Event::Message { role, body, .. } if role=="Tool" && body.contains("passed") && body.contains("exit 0"))
        );
        assert!(
            matches!(parse_event(&serde_json::json!({"type":"thread.started","thread_id":"abc"})), Some(Event::Session(s)) if s=="abc")
        );
    }

    #[test]
    fn resume_uses_exact_workstream_session_and_keeps_prompt_off_command_line() {
        let mut w = Workstream::new(std::path::Path::new("/tmp"));
        w.session = Some("thread-abc".into());
        w.draft = "private prompt".into();
        w.attachments = vec!["/tmp/image.png".into()];
        let cmd = agent_command(
            &AgentConfig {
                local: Default::default(),
                binary: "codex".into(),
                claude_binary: "claude".into(),
                search: true,
                writable: true,
            },
            &w,
        );
        let args = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|a| a == ["resume", "thread-abc"]));
        assert!(!args.iter().any(|a| a.contains("private prompt")));
        assert_eq!(args.last().unwrap(), "-");
        assert!(!args.iter().any(|a| a.contains("dangerously")));
    }
}
