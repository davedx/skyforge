//! Read Claude subscription quotas without starting an inference turn.
//! This endpoint is also used by Claude Code's interactive /usage command.
use crate::codex_usage::{Limits, Window};
use serde_json::Value;
use std::{path::PathBuf, process::Command, sync::atomic::AtomicBool, time::Duration};

const SIGN_IN: &str =
    "Claude usage requires a Claude subscription login. Sign in with Claude Code and retry.";

fn access_token(bytes: &[u8]) -> Result<String, String> {
    let credentials: Value = serde_json::from_slice(bytes).map_err(|_| SIGN_IN.to_owned())?;
    let oauth = &credentials["claudeAiOauth"];
    if oauth["expiresAt"]
        .as_i64()
        .is_some_and(|expires| expires <= chrono::Utc::now().timestamp_millis())
    {
        return Err(
            "Claude Code login has expired. Open Claude Code to refresh it, then retry.".into(),
        );
    }
    // Restrict to bearer-token characters so credentials cannot inject curl config.
    oauth["accessToken"]
        .as_str()
        .filter(|token| {
            !token.is_empty()
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
        })
        .map(str::to_owned)
        .ok_or_else(|| SIGN_IN.into())
}

fn config_dir() -> Result<PathBuf, String> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .ok_or_else(|| "Could not locate Claude Code configuration.".into())
}

fn read_token() -> Result<String, String> {
    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        return access_token(
            &serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{"accessToken":token}}))
                .unwrap(),
        );
    }
    let directory = config_dir()?;
    // On macOS Claude Code uses Keychain, with a file fallback on other systems.
    #[cfg(target_os = "macos")]
    {
        let mut service = "Claude Code-credentials".to_owned();
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            let mut hash = Command::new("/usr/bin/shasum");
            hash.args(["-a", "256"]);
            let (ok, output, _) = crate::local::execute(
                hash,
                Some(directory.to_string_lossy().into_owned()),
                &AtomicBool::new(false),
                Duration::from_secs(5),
                1024,
            )?;
            let output = String::from_utf8_lossy(&output);
            let digest = output.split_whitespace().next().unwrap_or_default();
            if !ok || digest.len() != 64 {
                return Err("Could not locate Claude Code Keychain credentials.".into());
            }
            service = format!("Claude Code-{}-credentials", &digest[..8]);
        }
        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-s", &service, "-w"]);
        if let Ok((true, output, _)) = crate::local::execute(
            command,
            None,
            &AtomicBool::new(false),
            Duration::from_secs(10),
            65536,
        ) {
            return access_token(&output);
        }
    }
    let bytes =
        std::fs::read(directory.join(".credentials.json")).map_err(|_| SIGN_IN.to_owned())?;
    access_token(&bytes)
}

fn parse_limits(bytes: &[u8]) -> Result<Limits, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "Claude returned an invalid usage response.".to_owned())?;
    let window = |key: &str, minutes| -> Result<Option<Window>, String> {
        let Some(value) = value.get(key).filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let used_percent = value["utilization"]
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| "Claude returned an invalid usage percentage.".to_owned())?;
        let resets_at = value["resets_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|time| time.timestamp());
        Ok(Some(Window {
            used_percent,
            window_duration_mins: Some(minutes),
            resets_at,
        }))
    };
    let limits = Limits {
        primary: window("five_hour", 300)?,
        secondary: window("seven_day", 10080)?,
    };
    if limits.primary.is_none() && limits.secondary.is_none() {
        return Err("Claude did not return subscription usage windows for this account.".into());
    }
    Ok(limits)
}

fn request_limits(mut command: Command, token: &str) -> Result<Limits, String> {
    // Keep credentials off argv; disable curlrc and do not follow redirects.
    command.args([
        "--disable",
        "--silent",
        "--connect-timeout",
        "10",
        "--max-time",
        "15",
        "--max-filesize",
        "65536",
        "--write-out",
        "\n%{http_code}",
        "--config",
        "-",
    ]);
    let input = format!("url = \"https://api.anthropic.com/api/oauth/usage\"\nheader = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\n");
    let (ok, output, _) = crate::local::execute(
        command,
        Some(input),
        &AtomicBool::new(false),
        Duration::from_secs(20),
        65536,
    )?;
    if !ok {
        return Err("Could not reach Claude usage service. Click to retry.".into());
    }
    let split = output
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or("Claude returned an invalid HTTP response.")?;
    let (body, status) = (&output[..split], &output[split + 1..]);
    match status {
        b"200" => parse_limits(body),
        b"401" | b"403" => Err(
            "Claude usage access was refused. Open Claude Code to refresh your login, then retry."
                .into(),
        ),
        b"429" => Err(
            "Claude usage service is rate limited. Automatic refresh will retry in five minutes."
                .into(),
        ),
        _ => Err("Claude usage service is temporarily unavailable.".into()),
    }
}

pub(crate) fn read_limits() -> Result<Limits, String> {
    request_limits(Command::new("/usr/bin/curl"), &read_token()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscription_windows_and_missing_data() {
        let limits = parse_limits(br#"{"five_hour":{"utilization":39,"resets_at":"2026-11-02T15:30:00Z"},"seven_day":{"utilization":19},"extra_usage":{"is_enabled":false}}"#).unwrap();
        assert_eq!(limits.primary.unwrap().resets_at, Some(1793633400));
        assert_eq!(limits.secondary.unwrap().used_percent, 19.0);
        assert!(parse_limits(br#"{"five_hour":null,"seven_day":{"utilization":0}}"#).is_ok());
        for value in [
            json!({}),
            json!({"five_hour":null}),
            json!({"five_hour":{"utilization":"39"}}),
            json!({"five_hour":{"utilization":-1}}),
        ] {
            assert!(parse_limits(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn credentials_are_validated_without_exposing_them() {
        assert_eq!(
            access_token(br#"{"claudeAiOauth":{"accessToken":"test-token_123"}}"#).unwrap(),
            "test-token_123"
        );
        for value in [
            json!({}),
            json!({"claudeAiOauth":{"accessToken":""}}),
            json!({"claudeAiOauth":{"accessToken":"secret\"\nurl = malicious"}}),
            json!({"claudeAiOauth":{"accessToken":"secret", "expiresAt":1}}),
        ] {
            let error = access_token(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn http_success_failures_and_private_authentication() {
        let command = |response: &str, status: &str| {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                r#"
case "$*" in *test-token*) exit 2 ;; esac
input=$(cat)
case "$input" in *'Authorization: Bearer test-token'*) ;; *) exit 3 ;; esac
case "$input" in *'https://api.anthropic.com/api/oauth/usage'*) ;; *) exit 4 ;; esac
printf '%s\n%s' "$1" "$2"
"#,
                "mock-curl",
                response,
                status,
            ]);
            command
        };
        let limits = request_limits(
            command(r#"{"five_hour":{"utilization":25}}"#, "200"),
            "test-token",
        )
        .unwrap();
        assert_eq!(limits.primary.unwrap().used_percent, 25.0);
        for (body, status, message) in [
            ("sensitive server body", "401", "login"),
            ("sensitive server body", "403", "login"),
            ("sensitive server body", "429", "five minutes"),
            ("sensitive server body", "500", "temporarily unavailable"),
            ("not-json", "200", "invalid usage response"),
            ("{}", "200", "subscription usage windows"),
        ] {
            let error = request_limits(command(body, status), "test-token").unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(!error.contains(body));
        }
    }

    #[test]
    #[ignore = "reads the locally signed-in Claude account; run explicitly"]
    fn installed_claude_returns_limits() {
        assert!(read_limits().unwrap().primary.is_some());
    }
}
