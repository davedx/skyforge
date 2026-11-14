use regex_automata::meta::Regex;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(test)]
use url::Url;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const EXA_RESULTS_LIMIT: usize = 1_100_000;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"web_search","description":"Search the web for sources. Returns up to about eight numbered source results, each with its title, URL and a short snippet drawn from the page. Open useful sources with fetch_url before relying on them, and cite their URLs.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"fetch_url","description":"Read an HTTP(S) source page without executing scripts or changing files. Returns its URL and readable text; some sites require a browser and cannot be read.","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false}}}),
    ]
}

pub fn tool(name: &str, args: &Value, stop: &AtomicBool) -> Result<String, String> {
    match name {
        "web_search" => {
            let query = args["query"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("Search needs a query")?;
            if query.chars().count() > 1000 {
                return Err("Search query is too long".into());
            }
            call_exa_mcp_search(query, stop)
        }
        "fetch_url" => {
            let url = args["url"].as_str().ok_or("Missing source URL")?;
            let page = get(url, stop)?;
            let html =
                page.content_type.contains("html") || page.body.trim_start().starts_with('<');
            let text = if html {
                readable_text(&page.body)
            } else if page.content_type.starts_with("text/")
                || page.content_type.contains("json")
                || page.content_type.contains("xml")
            {
                page.body
            } else {
                return Err(format!(
                    "This source is not a readable text page ({}).",
                    page.content_type
                ));
            };
            if text.trim().is_empty() {
                return Err(
                    "This source returned no readable text. It may require a browser.".into(),
                );
            }
            let truncated = text.chars().count() > 32000;
            Ok(json!({"url":page.url,"text":text.chars().take(32000).collect::<String>(),"truncated":truncated}).to_string())
        }
        _ => Err("Unknown research tool".into()),
    }
}

struct Page {
    url: String,
    content_type: String,
    body: String,
}

fn call_exa_mcp_search(query: &str, stop: &AtomicBool) -> Result<String, String> {
    let mut command = Command::new("/usr/bin/curl");
    // Disable curl's user configuration. The harness, not the model, decides this is a search,
    // so the model supplies neither a command nor request body.
    command.args([
        "-q",
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-redirs",
        "5",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--connect-timeout",
        "8",
        "--max-time",
        "25",
        "--http1.1",
        "--user-agent",
        "Skyforge/0.1 (read-only research)",
        "-H",
        "Content-Type: application/json",
        "-H",
        "Accept: application/json, text/event-stream",
        "-d",
    ]);
    command
        .arg(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "web_search_advanced_exa",
                    "arguments": {
                        "query": query,
                        "type": "auto",
                        "numResults": 8,
                        "highlights": true,
                        "textMaxCharacters": 3000
                    }
                }
            })
            .to_string(),
        )
        .arg("--")
        .arg(EXA_MCP_URL);
    let (success, bytes, error) = crate::local::execute(
        command,
        None,
        stop,
        Duration::from_secs(28),
        EXA_RESULTS_LIMIT,
    )?;
    if !success {
        return Err(format!(
            "Web search is unavailable: {}",
            crate::runner::clip(&String::from_utf8_lossy(&error), 1000)
        ));
    }
    let body = String::from_utf8(bytes)
        .map_err(|_| "Web search returned no UTF-8 text. The search service may be blocking automated requests.".to_string())?;
    parse_exa_mcp_results(&body)
}

fn parse_exa_mcp_results(body: &str) -> Result<String, String> {
    // The endpoint answers with a Model Context Protocol event stream, so the JSON payload is the
    // last "data:" line of the stream. Parse it, then fall back to the text payload Exa exposes by
    // default through the basic tool when the advanced tool is not offered by this deployment.
    let payload = match body
        .rsplit_once("data:")
        .and_then(|(_, payload)| payload.strip_prefix(' '))
    {
        Some(payload) => payload,
        None => body,
    };
    let parsed: Value = serde_json::from_str(payload)
        .map_err(|e| format!("Web search returned an unreadable response: {e}"))?;
    if let Some(status) = parsed.pointer("/error/code").and_then(|v| v.as_i64()) {
        let detail = parsed
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Ok(
            json!({"message":format!("Web search failed: status {status} — {detail}")}).to_string(),
        );
    }
    let content = parsed
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| {
            "Web search returned no results. The search service may be blocking automated requests."
                .to_string()
        })?;
    let payload_text = content
        .iter()
        .find(|c| c.get("type") == Some(&json!("text")))
        .and_then(|c| c["text"].as_str())
        .ok_or_else(|| "Web search returned no readable results. The search service may be blocking automated requests.".to_string())?;
    let payload_value =
        serde_json::from_str::<Value>(payload_text).unwrap_or_else(|_| parsed.clone());
    let results = payload_value
        .pointer("/results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "Web search returned no results. The search service may be blocking automated requests."
                .to_string()
        })?;
    let mut sources = Vec::new();
    for entry in results.iter() {
        if sources.len() == 8 {
            break;
        }
        let url = match entry["url"].as_str() {
            Some(url) => url,
            None => continue,
        };
        let title = entry["title"].as_str().unwrap_or_default().trim();
        let snapshot = entry["text"].as_str().unwrap_or_default();
        let snippet = snapshot
            .split_whitespace()
            .take(60)
            .collect::<Vec<_>>()
            .join(" ");
        if !title.is_empty() || !snippet.is_empty() {
            sources.push(json!({"url":url,"title":title,"snippet":snippet}));
        }
    }
    if sources.is_empty() {
        return Ok(
            json!({"message":"Web search returned no usable results for this query."}).to_string(),
        );
    }
    Ok(json!({"sources":sources}).to_string())
}

fn source_url(input: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(input).map_err(|_| "Invalid source URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Research tools accept only HTTP(S) URLs without embedded credentials.".into());
    }
    Ok(url)
}

fn get(input: &str, stop: &AtomicBool) -> Result<Page, String> {
    let url = source_url(input)?;
    let mut command = Command::new("/usr/bin/curl");
    // Disable curl's user configuration and accept only HTTP(S), including redirects.
    // The model supplies a URL, never a command, headers, credentials, or a request body.
    command.args([
        "-q",
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--max-redirs",
        "5",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--connect-timeout",
        "8",
        "--max-time",
        "25",
        "--max-filesize",
        "1048576",
        "--user-agent",
        "Skyforge/0.1 (read-only research)",
        "--write-out",
        "\nHARNESS_WEB_META:%{url_effective}\t%{content_type}",
        "--",
        url.as_str(),
    ]);
    let (success, bytes, error) =
        crate::local::execute(command, None, stop, Duration::from_secs(30), 1_100_000)?;
    if !success {
        return Err(format!(
            "Could not read source: {}",
            crate::runner::clip(&String::from_utf8_lossy(&error), 1000)
        ));
    }
    let output = String::from_utf8(bytes).map_err(|_| "This source is not UTF-8 text")?;
    let (body, metadata) = output
        .rsplit_once("\nHARNESS_WEB_META:")
        .ok_or("Incomplete source response")?;
    let (url, content_type) = metadata.split_once('\t').ok_or("Missing source metadata")?;
    source_url(url)?;
    Ok(Page {
        url: url.into(),
        content_type: content_type.to_lowercase(),
        body: body.into(),
    })
}

#[cfg(test)]
fn search_results(html: &str) -> Result<String, String> {
    let anchors = Regex::new(r#"(?is)<a\b([^>]*)>(.*?)</a>"#).unwrap();
    let href = Regex::new(r#"(?is)\bhref\s*=\s*["']([^"']+)["']"#).unwrap();
    let snippet = Regex::new(r#"(?is)<(?:a|div|span)\b[^>]*class\s*=\s*["'][^"']*result__snippet[^"']*["'][^>]*>(.*?)</(?:a|div|span)>"#).unwrap();
    let results: Vec<_> = anchors
        .captures_iter(html)
        .filter(|c| html[c.get_group(1).unwrap().range()].contains("result__a"))
        .collect();
    let mut sources = Vec::new();
    for (index, entry) in results.iter().enumerate().take(8) {
        let attributes = &html[entry.get_group(1).unwrap().range()];
        let Some(link) = href.captures_iter(attributes).next() else {
            continue;
        };
        let raw = decode_entities(&attributes[link.get_group(1).unwrap().range()]);
        let Ok(mut url) = Url::parse("https://duckduckgo.com").unwrap().join(&raw) else {
            continue;
        };
        if url
            .host_str()
            .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
        {
            let target = url
                .query_pairs()
                .find(|(key, _)| key == "uddg")
                .map(|(_, value)| value.into_owned());
            let Some(target) = target else {
                continue;
            };
            let Ok(target) = source_url(&target) else {
                continue;
            };
            url = target;
        }
        if source_url(url.as_str()).is_err() {
            continue;
        }
        let following = &html[entry.get_group(0).unwrap().end
            ..results
                .get(index + 1)
                .map(|c| c.get_group(0).unwrap().start)
                .unwrap_or(html.len())];
        let summary = snippet
            .captures_iter(following)
            .next()
            .map(|c| readable_text(&following[c.get_group(1).unwrap().range()]))
            .unwrap_or_default();
        sources.push(json!({"title":readable_text(&html[entry.get_group(2).unwrap().range()]),"url":url.as_str(),"snippet":summary}));
    }
    if sources.is_empty() {
        let text = readable_text(html).to_lowercase();
        if text.contains("no results") {
            return Ok(json!({"sources":[],"message":"No search results found"}).to_string());
        }
        return Err("Search did not return usable results. The search service may be blocking automated requests or its response format may have changed. Do not treat this as evidence that no sources exist.".into());
    }
    Ok(json!({"sources":sources}).to_string())
}

fn replace_matches(text: &str, pattern: &str, replacement: &str) -> String {
    let regex = Regex::new(pattern).unwrap();
    let mut output = String::new();
    let mut cursor = 0;
    for matched in regex.find_iter(text) {
        output.push_str(&text[cursor..matched.start()]);
        output.push_str(replacement);
        cursor = matched.end();
    }
    output.push_str(&text[cursor..]);
    output
}

fn readable_text(html: &str) -> String {
    let body = replace_matches(
        html,
        r"(?is)<!--.*?-->|<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>|<noscript\b[^>]*>.*?</noscript>",
        " ",
    );
    let body = replace_matches(
        &body,
        r"(?is)</(?:p|div|h[1-6]|li|tr|section|article)>|<br\s*/?>",
        "\n",
    );
    let body = replace_matches(&body, r"(?s)<[^>]*>", " ");
    decode_entities(&body)
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_entities(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';').filter(|end| *end <= 12) else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|s| s.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(character) = decoded {
            output.push(character);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_sources_keep_citations_and_exclude_scripts_and_non_web_urls() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fsource%3Fx%3D1%26y%3D2">A &amp; B</a><a class="result__snippet">Evidence <b>here</b>.</a>"#;
        let results: Value = serde_json::from_str(&search_results(html).unwrap()).unwrap();
        assert_eq!(
            results["sources"][0]["url"],
            "https://example.org/source?x=1&y=2"
        );
        assert_eq!(results["sources"][0]["title"], "A & B");
        assert_eq!(results["sources"][0]["snippet"], "Evidence here .");
        assert_eq!(readable_text("<style>secret</style><script>execute()</script><p>Useful &#x1F980; &nbsp; text</p>"),"Useful 🦀 text");
        assert!(search_results("<p>Unfortunately bots use DuckDuckGo too</p>").is_err());
        for url in [
            "file:///etc/hosts",
            "https://user:secret@example.org",
            "ftp://example.org",
            "--output=/tmp/file",
        ] {
            assert!(source_url(url).is_err());
        }
    }

    #[test]
    fn source_fetch_reads_pages_without_a_model_provider_and_supports_cancellation() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/reference", listener.local_addr().unwrap());
        let fixture = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("GET /reference "));
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.to_lowercase().starts_with("authorization:"));
            }
            let body =
                "<h1>Source title</h1><p>Verified reference fact.</p><script>hidden()</script>";
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let output: Value = serde_json::from_str(
            &tool("fetch_url", &json!({"url":url}), &AtomicBool::new(false)).unwrap(),
        )
        .unwrap();
        fixture.join().unwrap();
        assert_eq!(output["url"], url);
        assert!(output["text"]
            .as_str()
            .unwrap()
            .contains("Verified reference fact."));
        assert!(!output["text"].as_str().unwrap().contains("hidden"));
        assert!(tool(
            "fetch_url",
            &json!({"url":"http://127.0.0.1:1"}),
            &AtomicBool::new(true)
        )
        .is_err());
    }
}
