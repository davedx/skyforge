use eframe::egui;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_percent: f64,
    window_duration_mins: Option<i64>,
    resets_at: Option<i64>,
}

impl Window {
    fn label(&self) -> String {
        let duration = match self.window_duration_mins {
            Some(10080) => "Weekly".into(),
            Some(n) if n > 0 && n % 1440 == 0 => format!("{}d", n / 1440),
            Some(n) if n > 0 && n % 60 == 0 => format!("{}h", n / 60),
            Some(n) if n > 0 => format!("{n}m"),
            _ => "Usage".into(),
        };
        format!(
            "{duration}: {:.0}% left",
            (100.0 - self.used_percent).clamp(0.0, 100.0)
        )
    }

    fn detail(&self) -> String {
        let reset = self
            .resets_at
            .and_then(|time| chrono::DateTime::from_timestamp(time, 0))
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%a %d %b, %H:%M %Z")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".into());
        format!("{} · resets {reset}", self.label())
    }
}

#[derive(Debug, Deserialize)]
struct Limits {
    primary: Option<Window>,
    secondary: Option<Window>,
}

impl Limits {
    fn windows(&self) -> impl Iterator<Item = &Window> {
        self.primary.iter().chain(self.secondary.iter())
    }

    fn parse(result: &Value) -> Result<Self, String> {
        // Prefer the explicitly named Codex bucket over the compatibility view.
        let bucket = result
            .pointer("/rateLimitsByLimitId/codex")
            .filter(|value| !value.is_null())
            .unwrap_or(&result["rateLimits"]);
        let limits: Self = serde_json::from_value(bucket.clone()).map_err(|_| {
            "Codex did not return usage limits. Sign in to Codex with ChatGPT.".to_owned()
        })?;
        if limits.windows().next().is_none()
            || limits.windows().any(|w| !w.used_percent.is_finite())
        {
            return Err("Codex did not return usage windows for this account.".into());
        }
        Ok(limits)
    }
}

/// A read-only app-server request; never starts a thread or inference turn.
fn read_limits(binary: &str, timeout: Duration) -> Result<Limits, String> {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start Codex: {error}"))?;
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let result = (|| -> Result<Limits, String> {
            writeln!(
                input,
                "{}",
                json!({"id":1,"method":"initialize","params":{
                    "clientInfo":{"name":"skyforge","version":env!("CARGO_PKG_VERSION")}
                }})
            )
            .map_err(|e| e.to_string())?;
            input.flush().map_err(|e| e.to_string())?;
            for line in BufReader::new(output).lines() {
                let line = line.map_err(|e| e.to_string())?;
                let message: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                let id = message["id"].as_u64();
                if !matches!(id, Some(1 | 2)) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    return Err(error["message"]
                        .as_str()
                        .unwrap_or("Codex usage request failed")
                        .into());
                }
                if id == Some(1) {
                    writeln!(input, "{}", json!({"method":"initialized"}))
                        .map_err(|e| e.to_string())?;
                    writeln!(
                        input,
                        "{}",
                        json!({"id":2,"method":"account/rateLimits/read"})
                    )
                    .map_err(|e| e.to_string())?;
                    input.flush().map_err(|e| e.to_string())?;
                } else {
                    return Limits::parse(&message["result"]);
                }
            }
            Err("Codex closed before returning usage limits.".into())
        })();
        let _ = tx.send(result);
    });
    let result = rx
        .recv_timeout(timeout)
        .unwrap_or_else(|_| Err("Codex usage request timed out.".into()));
    // Reap on success, protocol errors, and timeout alike.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

#[derive(Default)]
pub struct Usage {
    binary: String,
    pending: Option<Receiver<Result<Limits, String>>>,
    limits: Option<Limits>,
    error: Option<String>,
    updated: Option<chrono::DateTime<chrono::Local>>,
    due: Option<Instant>,
}

impl Usage {
    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            limits: Some(
                Limits::parse(&json!({"rateLimits":{
                    "primary":{"usedPercent":39,"windowDurationMins":300},
                    "secondary":{"usedPercent":19,"windowDurationMins":10080}
                }}))
                .unwrap(),
            ),
            ..Default::default()
        }
    }

    pub fn update(&mut self, ctx: &egui::Context, binary: &str, visible: bool) {
        if self.binary != binary {
            *self = Self {
                binary: binary.into(),
                ..Default::default()
            };
        }
        if let Some(result) = self.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.pending = None;
            self.due = Some(Instant::now() + REFRESH);
            match result {
                Ok(limits) => {
                    self.limits = Some(limits);
                    self.updated = Some(chrono::Local::now());
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if !visible {
            return;
        }
        if self.pending.is_none() && self.due.is_none_or(|due| Instant::now() >= due) {
            let (tx, rx) = mpsc::channel();
            self.pending = Some(rx);
            let binary = binary.to_owned();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(read_limits(&binary, Duration::from_secs(15)));
                ctx.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let mut detail = "Codex account usage limits (shared across workstreams).\nRemaining quota percentages, not a token count.\n".to_owned();
        let label = if let Some(limits) = &self.limits {
            for window in limits.windows() {
                detail.push_str(&format!("\n{}", window.detail()));
            }
            if let Some(updated) = self.updated {
                detail.push_str(&format!(
                    "\nUpdated {}. Refreshes every minute. Click to refresh.",
                    updated.format("%H:%M:%S")
                ));
            }
            let label = limits
                .windows()
                .map(Window::label)
                .collect::<Vec<_>>()
                .join(" · ");
            if self.error.is_some() {
                format!("{label} (stale)")
            } else {
                label
            }
        } else if self.error.is_some() {
            "Usage unavailable".into()
        } else {
            "Usage loading…".into()
        };
        if let Some(error) = &self.error {
            detail.push_str(&format!("\nRefresh failed: {error}\nClick to retry."));
        }
        if ui
            .add(
                egui::Label::new(
                    egui::RichText::new(label)
                        .small()
                        .color(crate::theme::MUTED),
                )
                .sense(egui::Sense::click()),
            )
            .on_hover_text(detail)
            .clicked()
        {
            self.due = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_windows_and_compatibility_responses() {
        let window = json!({"usedPercent":39,"windowDurationMins":300,"resetsAt":1792663810});
        let limits = Limits::parse(&json!({"rateLimits":{"primary":window,"secondary":{
            "usedPercent":19,"windowDurationMins":10080,"resetsAt":1793167517
        }}}))
        .unwrap();
        assert_eq!(limits.primary.unwrap().label(), "5h: 61% left");
        assert_eq!(limits.secondary.unwrap().label(), "Weekly: 81% left");
        let limits = Limits::parse(&json!({"rateLimits":{"primary":{"usedPercent":0}},
            "rateLimitsByLimitId":{"codex":{"primary":window}}}))
        .unwrap();
        assert_eq!(limits.primary.unwrap().label(), "5h: 61% left");
        for value in [
            json!({}),
            json!({"rateLimits":null}),
            json!({"rateLimits":{"primary":null,"secondary":null}}),
        ] {
            assert!(Limits::parse(&value).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn app_server_handshake_errors_and_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("skyforge-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let binary = directory.join("codex");
        let write = |script: &str| {
            std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        write(
            r#"
[ "$1" = app-server ] || exit 1
IFS= read -r request
case "$request" in *'"method":"initialize"'*) ;; *) exit 2 ;; esac
echo '{"id":1,"result":{}}'
IFS= read -r request
case "$request" in *'"method":"initialized"'*) ;; *) exit 3 ;; esac
IFS= read -r request
case "$request" in *'"method":"account/rateLimits/read"'*) ;; *) exit 4 ;; esac
echo '{"method":"unrelated/notification"}'
echo '{"id":2,"result":{"rateLimits":{"secondary":{"usedPercent":100,"windowDurationMins":10080}}}}'
"#,
        );
        let limits = read_limits(binary.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        assert_eq!(limits.secondary.unwrap().label(), "Weekly: 0% left");
        write("read request\necho '{\"id\":1,\"error\":{\"message\":\"Please sign in\"}}'");
        assert_eq!(
            read_limits(binary.to_str().unwrap(), Duration::from_secs(2)).unwrap_err(),
            "Please sign in"
        );
        write("sleep 30");
        let start = Instant::now();
        assert!(
            read_limits(binary.to_str().unwrap(), Duration::from_millis(50))
                .unwrap_err()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "reads the locally signed-in Codex account; run explicitly"]
    fn installed_codex_returns_limits() {
        let limits = read_limits(&crate::runner::find_codex(), Duration::from_secs(15)).unwrap();
        assert!(limits.windows().next().is_some());
    }
}
