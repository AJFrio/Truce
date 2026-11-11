//! OpenRouter chat-completions client for hunk resolution.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::config::Settings;
use crate::error::{Result, TruceError};
use crate::output::{FileScan, FileStatus};
use crate::parse::Hunk;
use crate::resolve::{validate_resolution, Choice, HunkOutcome, Resolution};

const JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "resolved": {
      "type": "string",
      "description": "Exact replacement for the conflict region (<<<<<<< through >>>>>>>). No conflict markers."
    },
    "choice": {
      "type": "string",
      "enum": ["ours", "theirs", "merged"]
    },
    "rationale": {
      "type": "string",
      "description": "One or two sentences explaining the decision."
    },
    "confidence": {
      "type": "number",
      "minimum": 0,
      "maximum": 1
    }
  },
  "required": ["resolved", "choice", "rationale", "confidence"],
  "additionalProperties": false
}"#;

struct Inner {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    timeout: Duration,
    retries: u32,
}

/// HTTP client. The API key is never shown in Debug output.
#[derive(Clone)]
pub struct OpenRouterClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OpenRouterClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterClient")
            .field("base_url", &self.inner.base_url)
            .field("model", &self.inner.model)
            .field("api_key", &"***")
            .finish()
    }
}

impl OpenRouterClient {
    pub fn new(settings: &Settings) -> Result<Self> {
        let api_key = settings.api_key.clone().ok_or(TruceError::MissingApiKey)?;
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|err| TruceError::OpenRouter(redact(&err.to_string(), &api_key)))?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                api_key,
                base_url: settings.base_url.trim_end_matches('/').to_string(),
                model: settings.model.clone(),
                timeout: settings.timeout,
                retries: settings.retries,
            }),
        })
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.inner.base_url)
    }

    fn redact_err(&self, text: impl AsRef<str>) -> String {
        redact(text.as_ref(), &self.inner.api_key)
    }

    /// Resolve one hunk. Auth failures are fatal; everything else is a rejection.
    pub async fn resolve_hunk(
        &self,
        path: &str,
        hunk: &Hunk,
        min_confidence: f64,
    ) -> Result<HunkOutcome> {
        match self.complete(path, hunk).await {
            Ok(raw) => match parse_and_validate(&raw, hunk, min_confidence) {
                Ok(res) => Ok(HunkOutcome::Resolved(res)),
                Err(reason) => Ok(HunkOutcome::Rejected(reason)),
            },
            Err(TruceError::OpenRouterAuth) => Err(TruceError::OpenRouterAuth),
            Err(TruceError::MissingApiKey) => Err(TruceError::MissingApiKey),
            Err(err) => Ok(HunkOutcome::Rejected(err.to_string())),
        }
    }

    async fn complete(&self, path: &str, hunk: &Hunk) -> Result<String> {
        let body = chat_body(&self.inner.model, path, hunk);
        let mut last = String::from("OpenRouter request failed");
        let attempts = self.inner.retries.saturating_add(1);

        for attempt in 0..attempts {
            let send = self
                .inner
                .http
                .post(self.url())
                .bearer_auth(&self.inner.api_key)
                .header("HTTP-Referer", "https://github.com/AJFrio/Truce")
                .header("X-Title", "Truce")
                .json(&body)
                .send();

            let response = match tokio::time::timeout(self.inner.timeout, send).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(err)) => {
                    last = self.redact_err(err.to_string());
                    if err.is_timeout() || err.is_connect() || err.is_request() {
                        sleep_backoff(attempt, None).await;
                        continue;
                    }
                    return Err(TruceError::OpenRouter(last));
                }
                Err(_) => {
                    last = format!("request timed out after {:?}", self.inner.timeout);
                    sleep_backoff(attempt, None).await;
                    continue;
                }
            };

            let status = response.status();
            let headers = response.headers().clone();
            let text = response
                .text()
                .await
                .unwrap_or_else(|err| self.redact_err(err.to_string()));
            let text = self.redact_err(text);

            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(TruceError::OpenRouterAuth);
            }

            if status.as_u16() == 429 || status.is_server_error() {
                last = format!("HTTP {status}: {text}");
                sleep_backoff(attempt, retry_after(&headers)).await;
                continue;
            }

            if !status.is_success() {
                return Err(TruceError::OpenRouter(format!("HTTP {status}: {text}")));
            }

            return extract_content(&text).map_err(|e| TruceError::OpenRouter(self.redact_err(e)));
        }

        Err(TruceError::OpenRouter(format!("retries exhausted: {last}")))
    }
}

fn chat_body(model: &str, path: &str, hunk: &Hunk) -> Value {
    let schema: Value = serde_json::from_str(JSON_SCHEMA).expect("static schema");
    json!({
        "model": model,
        "temperature": 0,
        "max_tokens": 4096,
        "messages": [
            {
                "role": "system",
                "content": "You are Truce, a git merge-conflict resolver. \
    Decide the correct replacement for ONE conflict hunk. Reply with JSON only: \
    resolved (exact replacement for the <<<<<<< ... >>>>>>> region, no markers \
    and no surrounding context), choice (ours|theirs|merged), rationale (short), \
    confidence (0-1). Preserve indentation and local style."
            },
            {
                "role": "user",
                "content": user_prompt(path, hunk)
            }
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "truce_resolution",
                "strict": true,
                "schema": schema
            }
        }
    })
}

pub fn language_of(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "mjs" | "cjs" | "jsx") => "javascript",
        Some("ts" | "tsx") => "typescript",
        Some("go") => "go",
        Some("java") => "java",
        Some("kt" | "kts") => "kotlin",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh") => "cpp",
        Some("cs") => "csharp",
        Some("rb") => "ruby",
        Some("php") => "php",
        Some("swift") => "swift",
        Some("md") => "markdown",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("json") => "json",
        Some("html" | "htm") => "html",
        Some("css") => "css",
        Some("sh" | "bash" | "zsh") => "shell",
        Some("sql") => "sql",
        _ => "text",
    }
}

fn user_prompt(path: &str, hunk: &Hunk) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "File: {path}\nLanguage: {}\nHunk: {}  lines {}-{}\n\n",
        language_of(path),
        hunk.id,
        hunk.start_line,
        hunk.end_line
    ));
    if !hunk.context_before.is_empty() {
        out.push_str("Context before:\n");
        out.push_str(&hunk.context_before.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(&format!("OURS ({}):\n", or_none(&hunk.ours_label)));
    out.push_str(&hunk.ours.join("\n"));
    out.push_str("\n\n");
    if let (Some(label), Some(base)) = (&hunk.base_label, &hunk.base) {
        out.push_str(&format!("BASE ({label}):\n"));
        out.push_str(&base.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(&format!("THEIRS ({}):\n", or_none(&hunk.theirs_label)));
    out.push_str(&hunk.theirs.join("\n"));
    out.push('\n');
    if !hunk.context_after.is_empty() {
        out.push_str("\nContext after:\n");
        out.push_str(&hunk.context_after.join("\n"));
        out.push('\n');
    }
    out
}

fn or_none(label: &str) -> &str {
    if label.is_empty() {
        "none"
    } else {
        label
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Option<Vec<ChatChoice>>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: Option<ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: Option<MessageContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: Option<String>,
}

fn extract_content(body: &str) -> std::result::Result<String, String> {
    let parsed: ChatResponse =
        serde_json::from_str(body).map_err(|err| format!("invalid OpenRouter JSON: {err}"))?;
    if let Some(err) = parsed.error {
        return Err(err.message.unwrap_or_else(|| "OpenRouter error".into()));
    }
    let content = parsed
        .choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .ok_or_else(|| "empty OpenRouter choices".to_string())?;
    match content {
        MessageContent::Text(s) => Ok(s),
        MessageContent::Parts(parts) => {
            let mut s = String::new();
            for p in parts {
                if let Some(t) = p.text {
                    s.push_str(&t);
                }
            }
            if s.is_empty() {
                Err("empty OpenRouter content".into())
            } else {
                Ok(s)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawResolution {
    resolved: Option<String>,
    resolved_text: Option<String>,
    text: Option<String>,
    choice: Option<String>,
    rationale: Option<String>,
    reason: Option<String>,
    confidence: Option<Value>,
}

/// Parse model JSON, including markdown fences and extra prose.
pub fn parse_model_json(raw: &str) -> std::result::Result<(String, Choice, String, f64), String> {
    let trimmed = strip_fences(raw.trim());
    let blob =
        extract_object(trimmed).ok_or_else(|| "no JSON object in model output".to_string())?;
    let parsed: RawResolution =
        serde_json::from_str(blob).map_err(|err| format!("model JSON: {err}"))?;
    let text = parsed
        .resolved
        .or(parsed.resolved_text)
        .or(parsed.text)
        .ok_or_else(|| "missing resolved text".to_string())?;
    let choice = parsed
        .choice
        .as_deref()
        .and_then(Choice::parse_label)
        .ok_or_else(|| "missing or invalid choice (ours|theirs|merged)".to_string())?;
    let rationale = parsed.rationale.or(parsed.reason).unwrap_or_default();
    let confidence = parse_confidence(parsed.confidence.as_ref())?;
    Ok((text, choice, rationale, confidence))
}

fn parse_and_validate(
    raw: &str,
    hunk: &Hunk,
    min_confidence: f64,
) -> std::result::Result<Resolution, String> {
    let (text, choice, rationale, confidence) = parse_model_json(raw)?;
    validate_resolution(
        text,
        choice,
        rationale,
        confidence,
        min_confidence,
        &hunk.ours,
        &hunk.theirs,
    )
}

fn parse_confidence(value: Option<&Value>) -> std::result::Result<f64, String> {
    match value {
        None => Ok(0.0),
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| "confidence is not a number".into()),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("confidence {s:?} is not a number")),
        Some(_) => Err("confidence is not a number".into()),
    }
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest)
        .trim_start_matches('\n');
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

fn extract_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end >= start {
        Some(&s[start..=end])
    } else {
        None
    }
}

/// Replace every occurrence of `key` (and `Bearer {key}`) in `text`.
pub fn redact(text: &str, key: &str) -> String {
    if key.is_empty() {
        return text.to_string();
    }
    text.replace(key, "***")
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    raw.parse::<u64>()
        .ok()
        .map(|secs| Duration::from_secs(secs.min(60)))
}

async fn sleep_backoff(attempt: u32, retry_after: Option<Duration>) {
    if let Some(d) = retry_after {
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
        return;
    }
    let base_ms = 200u64.saturating_mul(1u64 << attempt.min(5));
    let jitter = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        nanos % 200
    };
    let delay = Duration::from_millis((base_ms + jitter).min(8_000));
    tokio::time::sleep(delay).await;
}

/// Fill `outcomes` for every conflicted hunk, with a concurrency limit.
pub async fn resolve_scans(
    scans: &mut [FileScan],
    client: &OpenRouterClient,
    concurrency: usize,
    min_confidence: f64,
) -> Result<()> {
    struct Job {
        file: usize,
        hunk: usize,
        path: String,
        hunk_data: Hunk,
    }

    let mut jobs = Vec::new();
    for (file, scan) in scans.iter().enumerate() {
        if scan.status != FileStatus::Conflicted {
            continue;
        }
        let path = scan.rel_path.to_string_lossy().replace('\\', "/");
        for (hunk_idx, hunk) in scan.hunks.iter().enumerate() {
            jobs.push(Job {
                file,
                hunk: hunk_idx,
                path: path.clone(),
                hunk_data: hunk.clone(),
            });
        }
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for job in jobs {
        let client = client.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore");
            let outcome = client
                .resolve_hunk(&job.path, &job.hunk_data, min_confidence)
                .await;
            (job.file, job.hunk, outcome)
        });
    }

    while let Some(joined) = set.join_next().await {
        let (file, hunk, outcome) =
            joined.map_err(|err| TruceError::OpenRouter(format!("worker task failed: {err}")))?;
        match outcome {
            Ok(result) => {
                if let Some(scan) = scans.get_mut(file) {
                    if let Some(slot) = scan.outcomes.get_mut(hunk) {
                        *slot = result;
                    }
                }
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::parse::parse_conflicts;

    fn hunk() -> Hunk {
        parse_conflicts("<<<<<<< A\n1\n=======\n2\n>>>>>>> B\n", 0)
            .unwrap()
            .hunks
            .remove(0)
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(language_of("src/app.rs"), "rust");
        assert_eq!(language_of("notes/my file.txt"), "text");
        assert_eq!(language_of("a.TSX"), "typescript");
    }

    #[test]
    fn prompt_includes_path_language_and_sides() {
        let p = user_prompt("src/app.rs", &hunk());
        assert!(p.contains("File: src/app.rs"));
        assert!(p.contains("Language: rust"));
        assert!(p.contains("OURS"));
        assert!(p.contains("THEIRS"));
        assert!(p.contains("1"));
        assert!(p.contains("2"));
    }

    #[test]
    fn parse_plain_json() {
        let (text, choice, _, conf) = parse_model_json(
            r#"{"resolved":"x","choice":"theirs","rationale":"r","confidence":0.8}"#,
        )
        .unwrap();
        assert_eq!(text, "x");
        assert_eq!(choice, Choice::Theirs);
        assert!((conf - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_fenced_and_prose() {
        let raw = "Sure.\n```json\n{\"resolved\":\"y\",\"choice\":\"OURS\",\"rationale\":\"ok\",\"confidence\":\"0.95\"}\n```\n";
        let (text, choice, _, conf) = parse_model_json(raw).unwrap();
        assert_eq!(text, "y");
        assert_eq!(choice, Choice::Ours);
        assert!((conf - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn redact_strips_key() {
        let key = "sk-or-v1-SECRETVALUE";
        let out = redact(&format!("Authorization Bearer {key} failed"), key);
        assert!(!out.contains("SECRETVALUE"), "{out}");
        assert!(out.contains("***"), "{out}");
    }

    #[test]
    fn debug_hides_key() {
        let settings = Settings {
            model: "m".into(),
            min_confidence: 0.7,
            concurrency: 1,
            timeout: Duration::from_secs(5),
            retries: 0,
            base_url: "http://127.0.0.1".into(),
            api_key: Some("sk-or-v1-SECRETVALUE".into()),
        };
        let client = OpenRouterClient::new(&settings).unwrap();
        let dbg = format!("{client:?}");
        assert!(!dbg.contains("SECRETVALUE"), "{dbg}");
        assert!(dbg.contains("***"), "{dbg}");
    }

    #[test]
    fn extract_openai_shape() {
        let body = r#"{"choices":[{"message":{"content":"{\"resolved\":\"z\"}"}}]}"#;
        assert!(extract_content(body).unwrap().contains("resolved"));
    }
}
