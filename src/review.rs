//! AI code review — builtin engine (faithful port of review.py) plus the
//! shared publication pipeline used by every engine.
//!
//! Flow: fetch diff → parse added lines → build prompt → call AI → parse
//! verdict → post review (with inline comments, degrading gracefully).

use serde_json::{json, Value};

use crate::config::Config;

// ---------------------------------------------------------------------------
// AI call — three formats
// ---------------------------------------------------------------------------

pub async fn call_ai(
    cfg: &Config,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let base = cfg.ai_base_url.trim_end_matches('/');
    let fmt = cfg.api_format.to_lowercase();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let url;
    let mut headers = reqwest::header::HeaderMap::new();
    let body: Value;

    if fmt == "chat" {
        url = format!("{base}/chat/completions");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cfg.ai_api_key).parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
        });
    } else if fmt == "responses" {
        url = format!("{base}/responses");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cfg.ai_api_key).parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "input": user_prompt,
            "instructions": system_prompt,
            "text": {"format": {"type": "json_object"}},
        });
    } else if fmt == "anthropic" {
        url = format!("{base}/v1/messages");
        headers.insert(
            reqwest::header::HeaderName::from_static("x-api-key"),
            cfg.ai_api_key.parse().unwrap(),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            "2023-06-01".parse().unwrap(),
        );
        body = json!({
            "model": cfg.ai_model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_prompt}],
        });
    } else {
        return Err(format!("unknown api_format: {}", cfg.api_format));
    }

    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AI request failed to {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("AI read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "AI request failed ({}) to {url}: {}",
            status,
            &text.chars().take(400).collect::<String>()
        ));
    }
    let out: Value =
        serde_json::from_str(&text).map_err(|e| format!("AI response not JSON: {e}"))?;

    extract_ai_text(&fmt, &out)
}

// ---------------------------------------------------------------------------
// Verdict parsing (robust, three-tier fallback)
// ---------------------------------------------------------------------------

pub fn parse_verdict(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    // ```json fenced
    let re = regex::Regex::new(r"```(?:json)?\s*(\{.*?\})\s*```").unwrap();
    if let Some(m) = re.captures(text) {
        if let Ok(v) = serde_json::from_str::<Value>(m.get(1).unwrap().as_str()) {
            return Some(v);
        }
    }
    // greediest {...}
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<Value>(&text[a..=b]) {
                return Some(v);
            }
        }
    }
    None
}

fn extract_ai_text(fmt: &str, out: &Value) -> Result<String, String> {
    match fmt {
        "chat" => out
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(String::from)
            .ok_or_else(|| format!("chat API: no content in {out}")),
        "responses" => {
            if let Some(t) = out.get("output_text").and_then(|t| t.as_str()) {
                return Ok(t.to_string());
            }
            // fallback: walk output array backwards
            if let Some(arr) = out.get("output").and_then(|o| o.as_array()) {
                for item in arr.iter().rev() {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for c in content {
                            if c.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        return Ok(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(format!("responses API: no text in {out}"))
        }
        "anthropic" => {
            if let Some(content) = out.get("content").and_then(|c| c.as_array()) {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                return Ok(t.to_string());
                            }
                        }
                    }
                }
            }
            Err(format!("anthropic API: no text in {out}"))
        }
        _ => Err("unknown format".into()),
    }
}

// ---------------------------------------------------------------------------
// Diff parsing — per-file added line numbers (RIGHT side)
// ---------------------------------------------------------------------------

/// For each file, collect line numbers on the new side that were added.
/// These are the only lines GitHub lets inline review comments attach to.
pub fn parse_added_lines(
    diff: &str,
) -> std::collections::HashMap<String, std::collections::HashSet<i64>> {
    use regex::Regex;
    use std::collections::{HashMap, HashSet};

    let mut added: HashMap<String, HashSet<i64>> = HashMap::new();
    let file_re = Regex::new(r"^\+\+\+ b/(.+)$").unwrap();
    let hunk_re = Regex::new(r"\+(\d+)(?:,(\d+))?").unwrap();

    let mut current_file: Option<String> = None;
    let mut new_line: i64 = 0;

    for line in diff.lines() {
        if let Some(m) = file_re.captures(line) {
            let name = m.get(1).unwrap().as_str().to_string();
            added.entry(name.clone()).or_default();
            current_file = Some(name);
            continue;
        }
        if line.starts_with("+++ ") {
            current_file = None;
            continue;
        }
        if line.starts_with("@@") {
            if let Some(mm) = hunk_re.captures(line) {
                new_line = mm.get(1).and_then(|d| d.as_str().parse().ok()).unwrap_or(0) - 1;
            }
            continue;
        }
        let Some(file) = &current_file else {
            continue;
        };
        if line.starts_with('+') {
            new_line += 1;
            added.get_mut(file).unwrap().insert(new_line);
        } else if line.starts_with('-') {
            // removed line: new-side numbering unchanged
        } else {
            new_line += 1;
        }
    }
    added
}

pub fn truncate(diff: &str, max_chars: usize) -> (String, bool) {
    if diff.len() <= max_chars {
        return (diff.to_string(), false);
    }
    (diff.chars().take(max_chars).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,6 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
+    let y = 2;
 }
@@ -10,3 +12,4 @@
 fn other() {
+    helper();
 }
";

    #[test]
    fn test_parse_added_lines() {
        let added = parse_added_lines(SAMPLE_DIFF);
        let set = added.get("src/main.rs").unwrap();
        // lines 2,3,4 added in first hunk (start=1, +3 lines → 2,3,4);
        // hunk 2 starts at 12: line 13 added
        assert!(set.contains(&2) && set.contains(&3) && set.contains(&4));
        assert!(set.contains(&13));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn test_truncate() {
        let (d, t) = truncate("hello", 10);
        assert_eq!(d, "hello");
        assert!(!t);
        let (d, t) = truncate("hello world", 5);
        assert_eq!(d, "hello");
        assert!(t);
    }

    #[test]
    fn test_parse_verdict_direct() {
        let v = parse_verdict(r#"{"summary": "ok", "findings": []}"#);
        assert!(v.is_some());
    }

    #[test]
    fn test_parse_verdict_fenced() {
        let v = parse_verdict("here you go:\n```json\n{\"summary\": \"x\", \"findings\": []}\n```");
        assert!(v.is_some());
        assert_eq!(v.unwrap()["summary"], "x");
    }

    #[test]
    fn test_parse_verdict_greedy() {
        let v = parse_verdict("junk before {\"summary\": \"y\", \"findings\": []} junk after");
        assert!(v.is_some());
    }

    #[test]
    fn test_parse_verdict_none() {
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("no json here").is_none());
    }
}
