//! OpenRouter System One (decisions) client.
//!
//! Decision models are listed with `output_modalities=decisions` and are not
//! callable via chat completions. Each hunk is
//! `POST {base_url}/systemone` (`/api/v1/systemone` when the base URL is the
//! OpenRouter v1 root) with `model`, `state`, and one `choice` question.
//! The response is `answers.resolution = {type, choice, confidence,
//! probabilities}` plus `usage`. It contains no generated text.
//!
//! `liquid/d1-ultra` has a 32,768-token context. Tokens are estimated as one
//! per [`CHARS_PER_TOKEN`] characters, rounded up, and
//! [`DECISION_HEADROOM_TOKENS`] are left unused because that estimate can
//! under-count. Surrounding `context` is trimmed from the outside inward.
//! If `ours`, `theirs`, and optional `base` still do not fit, the caller
//! skips the HTTP call and escalates with reason `too_large`.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::error::{Result, TruceError};
use crate::parse::{language_of, Hunk};
use crate::resolve::{Choice, DecisionInfo};
use crate::transport::Transport;

/// Context window of the default decision model (`liquid/d1-ultra`).
pub(crate) const DECISION_CONTEXT_TOKENS: usize = 32_768;
/// Reserved so a 4-characters-per-token estimate that under-counts still fits.
pub(crate) const DECISION_HEADROOM_TOKENS: usize = 1_024;
pub(crate) const CHARS_PER_TOKEN: usize = 4;

pub(crate) const RESOLUTION_INSTRUCTIONS: &str = "Choose how to resolve this single git merge conflict. Prefer ours or theirs when that side can be kept verbatim. Choose both only when both sides must be retained and concatenating them is safe. Choose needs_code when the fix requires a rewrite.";

pub(crate) const CRITERION_OURS: &str =
    "Keep the OURS side unchanged and drop THEIRS. OURS is already a complete, correct resolution.";
pub(crate) const CRITERION_THEIRS: &str = "Keep the THEIRS side unchanged and drop OURS. THEIRS is already a complete, correct resolution.";
pub(crate) const CRITERION_BOTH: &str = "Keep both sides by placing every OURS line first and every THEIRS line second, without rewriting either side.";
pub(crate) const CRITERION_NEEDS_CODE: &str = "Neither side alone, nor a plain concatenation of both, is a correct resolution. The hunk needs edited or new code.";

/// Relative path under the configured API base (`…/api/v1/systemone`).
pub(crate) const SYSTEMONE_PATH: &str = "systemone";

#[derive(Clone, Copy)]
pub(crate) struct Budget {
    pub limit_tokens: usize,
    pub headroom_tokens: usize,
}

impl Budget {
    pub(crate) fn decision_default() -> Self {
        Self {
            limit_tokens: DECISION_CONTEXT_TOKENS,
            headroom_tokens: DECISION_HEADROOM_TOKENS,
        }
    }
}

pub(crate) enum PreparedRequest {
    Ready(Value),
    /// `ours` + `theirs` + optional `base` exceed the budget with empty context.
    TooLarge {
        /// Estimated tokens of the request with context removed. Read by tests.
        #[allow(dead_code)]
        tokens: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DecisionAnswer {
    pub model: String,
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

pub(crate) enum DecisionClass {
    Side(Choice),
    NeedsCode,
    Unknown,
}

impl DecisionAnswer {
    pub(crate) fn info(&self) -> DecisionInfo {
        DecisionInfo {
            model: self.model.clone(),
            choice: self.choice.clone(),
            confidence: self.confidence,
            probabilities: self.probabilities.clone(),
        }
    }

    /// Map the choice id. Only the four question options are known;
    /// `merged` is not an alias of `both` here.
    pub(crate) fn classify(&self) -> DecisionClass {
        match self.choice.as_str() {
            "ours" => DecisionClass::Side(Choice::Ours),
            "theirs" => DecisionClass::Side(Choice::Theirs),
            "both" => DecisionClass::Side(Choice::Merged),
            "needs_code" => DecisionClass::NeedsCode,
            _ => DecisionClass::Unknown,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SystemOneClient {
    transport: Transport,
    model: String,
}

impl std::fmt::Debug for SystemOneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOneClient")
            .field("model", &self.model)
            .field("transport", &self.transport)
            .finish()
    }
}

impl SystemOneClient {
    pub(crate) fn from_parts(transport: Transport, model: String) -> Self {
        Self { transport, model }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// Submit one decision request.
    ///
    /// Non-2xx responses other than 401/403, and 2xx bodies that are not a
    /// choice answer, become [`TruceError::Decision`]. 401/403 stay
    /// [`TruceError::OpenRouterAuth`] so the run aborts instead of escalating
    /// with the same key.
    pub(crate) async fn decide(&self, body: &Value) -> Result<DecisionAnswer> {
        let text = match self.transport.post_json(SYSTEMONE_PATH, body).await {
            Ok(text) => text,
            Err(TruceError::OpenRouterAuth) => return Err(TruceError::OpenRouterAuth),
            Err(TruceError::OpenRouter(message)) => return Err(TruceError::Decision(message)),
            Err(err) => return Err(err),
        };
        parse_decision(&text, &self.model).map_err(TruceError::Decision)
    }
}

pub(crate) fn prepare_request(model: &str, path: &str, hunk: &Hunk) -> PreparedRequest {
    prepare_request_with_budget(model, path, hunk, Budget::decision_default())
}

pub(crate) fn prepare_request_with_budget(
    model: &str,
    path: &str,
    hunk: &Hunk,
    budget: Budget,
) -> PreparedRequest {
    let language = language_of(path);
    let ours = hunk.ours.join("\n");
    let theirs = hunk.theirs.join("\n");
    let base = hunk.base.as_ref().map(|lines| lines.join("\n"));

    let render = |before: &[String], after: &[String]| -> Value {
        let context = render_context(hunk, before, after);
        let body = RequestBody {
            model,
            questions: questions(),
            state: State {
                file: path,
                language,
                ours: &ours,
                theirs: &theirs,
                base: base.as_deref(),
                context: &context,
            },
        };
        serde_json::to_value(&body).expect("decision request is valid JSON")
    };

    let full = render(&hunk.context_before, &hunk.context_after);
    if fits(&full, budget) {
        return PreparedRequest::Ready(full);
    }

    let empty = render(&[], &[]);
    if !fits(&empty, budget) {
        return PreparedRequest::TooLarge {
            tokens: token_count(&empty),
        };
    }

    let mut before = hunk.context_before.clone();
    let mut after = hunk.context_after.clone();
    while !before.is_empty() || !after.is_empty() {
        if before.len() >= after.len() && !before.is_empty() {
            let n = trim_count(before.len());
            before.drain(0..n);
        } else if !after.is_empty() {
            let n = trim_count(after.len());
            let keep = after.len() - n;
            after.truncate(keep);
        } else {
            let n = trim_count(before.len());
            before.drain(0..n);
        }
        let body = render(&before, &after);
        if fits(&body, budget) {
            return PreparedRequest::Ready(body);
        }
    }

    PreparedRequest::Ready(empty)
}

pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

fn token_count(value: &Value) -> usize {
    estimate_tokens(&value.to_string())
}

fn fits(value: &Value, budget: Budget) -> bool {
    token_count(value).saturating_add(budget.headroom_tokens) <= budget.limit_tokens
}

/// Drop several lines at once when a side is large, otherwise one line.
fn trim_count(len: usize) -> usize {
    if len > 32 {
        len / 2
    } else {
        1
    }
}

fn render_context(hunk: &Hunk, before: &[String], after: &[String]) -> String {
    let mut out = String::new();
    push_label(&mut out, "ours_label", &hunk.ours_label);
    push_label(&mut out, "theirs_label", &hunk.theirs_label);
    if let Some(label) = &hunk.base_label {
        push_label(&mut out, "base_label", label);
    }
    out.push_str("before:\n");
    if !before.is_empty() {
        out.push_str(&before.join("\n"));
        out.push('\n');
    }
    out.push_str("after:\n");
    if !after.is_empty() {
        out.push_str(&after.join("\n"));
        out.push('\n');
    }
    out
}

fn push_label(out: &mut String, key: &str, label: &str) {
    out.push_str(key);
    out.push_str(": ");
    if label.is_empty() {
        out.push_str("(none)");
    } else {
        out.push_str(label);
    }
    out.push('\n');
}

fn questions() -> Questions {
    Questions {
        resolution: Question {
            criteria: Criteria {
                ours: CRITERION_OURS,
                theirs: CRITERION_THEIRS,
                both: CRITERION_BOTH,
                needs_code: CRITERION_NEEDS_CODE,
            },
            instructions: RESOLUTION_INSTRUCTIONS,
            kind: "choice",
        },
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    questions: Questions,
    state: State<'a>,
}

#[derive(Serialize)]
struct Questions {
    resolution: Question,
}

#[derive(Serialize)]
struct Question {
    criteria: Criteria,
    instructions: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct Criteria {
    ours: &'static str,
    theirs: &'static str,
    both: &'static str,
    needs_code: &'static str,
}

#[derive(Serialize)]
struct State<'a> {
    file: &'a str,
    language: &'a str,
    ours: &'a str,
    theirs: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a str>,
    context: &'a str,
}

pub(crate) fn parse_decision(
    body: &str,
    fallback_model: &str,
) -> std::result::Result<DecisionAnswer, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| format!("malformed response: invalid JSON: {err}"))?;
    if let Some(error) = value.get("error") {
        if !error.is_null() {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("decision response contained an error");
            return Err(message.to_string());
        }
    }
    let answer = value
        .get("answers")
        .and_then(|answers| answers.get("resolution"))
        .ok_or_else(|| "malformed response: missing answers.resolution".to_string())?;
    let kind = answer
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("choice");
    if kind != "choice" {
        return Err(format!(
            "malformed response: answers.resolution type is {kind}, expected choice"
        ));
    }
    let raw_choice = answer
        .get("choice")
        .and_then(|choice| choice.as_str())
        .ok_or_else(|| "malformed response: missing choice".to_string())?;
    let choice = raw_choice.trim().to_ascii_lowercase();
    if choice.is_empty() {
        return Err("malformed response: empty choice".into());
    }
    let confidence = match answer.get("confidence") {
        Some(value) => parse_confidence(value)?,
        None => return Err("malformed response: missing confidence".into()),
    };
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(format!(
            "malformed response: confidence {confidence} is not in 0..=1"
        ));
    }
    let probabilities = parse_probabilities(answer.get("probabilities"))?;
    let model = value
        .get("model")
        .and_then(|model| model.as_str())
        .filter(|model| !model.is_empty())
        .unwrap_or(fallback_model)
        .to_string();
    Ok(DecisionAnswer {
        model,
        choice,
        confidence,
        probabilities,
    })
}

fn parse_confidence(value: &Value) -> std::result::Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| "malformed response: confidence is not a number".to_string())
}

fn parse_probabilities(
    value: Option<&Value>,
) -> std::result::Result<BTreeMap<String, f64>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let obj = value
        .as_object()
        .ok_or_else(|| "malformed response: probabilities is not an object".to_string())?;
    let mut out = BTreeMap::new();
    for (key, prob) in obj {
        let n = prob
            .as_f64()
            .ok_or_else(|| format!("malformed response: probability {key} is not a number"))?;
        out.insert(key.clone(), n);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_conflicts;
    use crate::resolve::Choice;

    fn hunk_text() -> Hunk {
        parse_conflicts(
            "before\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\nafter\n",
            1,
        )
        .unwrap()
        .hunks
        .remove(0)
    }

    #[test]
    fn request_is_a_choice_question_without_chat_fields() {
        let hunk = hunk_text();
        let PreparedRequest::Ready(body) = prepare_request("liquid/d1-ultra", "src/app.rs", &hunk)
        else {
            panic!("small hunk should fit");
        };
        assert!(body.get("messages").is_none(), "{body}");
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("response_format").is_none(), "{body}");
        assert_eq!(body["model"], "liquid/d1-ultra");
        assert_eq!(body["questions"]["resolution"]["type"], "choice");
        assert_eq!(
            body["questions"]["resolution"]["instructions"],
            RESOLUTION_INSTRUCTIONS
        );
        assert_eq!(
            body["questions"]["resolution"]["criteria"]["ours"],
            CRITERION_OURS
        );
        assert_eq!(
            body["questions"]["resolution"]["criteria"]["needs_code"],
            CRITERION_NEEDS_CODE
        );
        assert_eq!(body["questions"].as_object().unwrap().len(), 1);
        assert_eq!(body["state"]["file"], "src/app.rs");
        assert_eq!(body["state"]["language"], "rust");
        assert_eq!(body["state"]["ours"], "ours");
        assert_eq!(body["state"]["theirs"], "theirs");
        assert!(body["state"].get("base").is_none(), "{body}");
        let context = body["state"]["context"].as_str().unwrap();
        assert!(context.contains("ours_label: HEAD"), "{context}");
        assert!(context.contains("theirs_label: feature"), "{context}");
        assert!(context.contains("before:\nbefore\n"), "{context}");
        assert!(context.contains("after:\nafter\n"), "{context}");
    }

    #[test]
    fn diff3_base_is_included() {
        let hunk = parse_conflicts(
            "<<<<<<< HEAD\nours\n||||||| merged common ancestors\nbase\n=======\ntheirs\n>>>>>>> feature\n",
            0,
        )
        .unwrap()
        .hunks
        .remove(0);
        let PreparedRequest::Ready(body) = prepare_request("liquid/d1-ultra", "a.txt", &hunk)
        else {
            panic!("small hunk should fit");
        };
        assert_eq!(body["state"]["base"], "base");
        assert!(body["state"]["context"]
            .as_str()
            .unwrap()
            .contains("base_label: merged common ancestors"));
    }

    #[test]
    fn trims_far_context_before_giving_up() {
        let mut hunk = hunk_text();
        let far = "FAR_CONTEXT_LINE_THAT_IS_LONG_ENOUGH_TO_DROP";
        let near = "NEAR_CONTEXT_LINE";
        hunk.context_before = vec![far.into(), near.into()];
        hunk.context_after.clear();
        let PreparedRequest::Ready(full) = prepare_request_with_budget(
            "liquid/d1-ultra",
            "a.txt",
            &hunk,
            Budget {
                limit_tokens: 1_000_000,
                headroom_tokens: 0,
            },
        ) else {
            panic!("unlimited budget should fit");
        };
        let full_tokens = token_count(&full);
        let far_tokens = estimate_tokens(far).max(1);
        let PreparedRequest::Ready(trimmed) = prepare_request_with_budget(
            "liquid/d1-ultra",
            "a.txt",
            &hunk,
            Budget {
                limit_tokens: full_tokens - far_tokens / 2,
                headroom_tokens: 0,
            },
        ) else {
            panic!("trimming context should still fit");
        };
        let context = trimmed["state"]["context"].as_str().unwrap();
        assert!(!context.contains(far), "{context}");
        assert!(context.contains(near), "{context}");
        assert_eq!(trimmed["state"]["ours"], "ours");
        assert_eq!(trimmed["state"]["theirs"], "theirs");
    }

    #[test]
    fn oversize_side_skips_without_a_body() {
        let mut hunk = hunk_text();
        hunk.ours = vec!["x".repeat(150_000)];
        hunk.context_before = vec!["keep-me".into()];
        match prepare_request("liquid/d1-ultra", "a.txt", &hunk) {
            PreparedRequest::TooLarge { tokens } => {
                assert!(
                    tokens + DECISION_HEADROOM_TOKENS > DECISION_CONTEXT_TOKENS,
                    "tokens={tokens}"
                );
            }
            PreparedRequest::Ready(body) => panic!("should be too large: {body}"),
        }
    }

    #[test]
    fn parses_choice_answer_and_falls_back_model() {
        let raw = r#"{
            "answers": {
                "resolution": {
                    "type": "choice",
                    "choice": " OURS ",
                    "confidence": 1,
                    "probabilities": {"ours": 1, "theirs": 0, "both": 0, "needs_code": 0}
                }
            }
        }"#;
        let answer = parse_decision(raw, "liquid/d1-ultra").unwrap();
        assert_eq!(answer.choice, "ours");
        assert_eq!(answer.model, "liquid/d1-ultra");
        assert!((answer.confidence - 1.0).abs() < f64::EPSILON);
        assert!((answer.probabilities["theirs"] - 0.0).abs() < f64::EPSILON);
        assert!(matches!(
            answer.classify(),
            DecisionClass::Side(Choice::Ours)
        ));
    }

    #[test]
    fn classifies_needs_code_and_unknown() {
        let needs = parse_decision(
            r#"{"model":"liquid/d1-ultra","answers":{"resolution":{"type":"choice","choice":"needs_code","confidence":0.5}}}"#,
            "fallback",
        )
        .unwrap();
        assert!(matches!(needs.classify(), DecisionClass::NeedsCode));
        assert_eq!(needs.model, "liquid/d1-ultra");
        let unknown = parse_decision(
            r#"{"answers":{"resolution":{"type":"choice","choice":"rewrite","confidence":0.5}}}"#,
            "fallback",
        )
        .unwrap();
        assert!(matches!(unknown.classify(), DecisionClass::Unknown));
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert!(parse_decision("not-json", "m").is_err());
        assert!(parse_decision(r#"{"answers":{}}"#, "m").is_err());
        assert!(parse_decision(
            r#"{"answers":{"resolution":{"type":"score","choice":"ours","confidence":0.5}}}"#,
            "m"
        )
        .is_err());
        assert!(
            parse_decision(r#"{"error":{"code":400,"message":"bad model"}}"#, "m")
                .unwrap_err()
                .contains("bad model")
        );
    }
}
