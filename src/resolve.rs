use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TruceError};
use crate::parse::{join_lines, LineEnding, ParsedFile};
use crate::Hunk;

/// Which side (or blend) a resolver picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Ours,
    Theirs,
    Merged,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Merged => "merged",
        })
    }
}

/// Which API produced the applied text, or attempted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolverPath {
    /// System One choice applied deterministically in Rust.
    Decision,
    /// Chat completions on the code model.
    Code,
}

impl fmt::Display for ResolverPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Decision => "decision",
            Self::Code => "code",
        })
    }
}

/// Why a hunk was sent to the code model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationReason {
    NeedsCode,
    LowConfidence,
    DecisionError,
    UnknownChoice,
    TooLarge,
}

impl fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NeedsCode => "needs_code",
            Self::LowConfidence => "low_confidence",
            Self::DecisionError => "decision_error",
            Self::UnknownChoice => "unknown_choice",
            Self::TooLarge => "too_large",
        })
    }
}

/// System One answer recorded on a hunk, including when it was not applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionInfo {
    pub model: String,
    pub choice: String,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
}

/// Per-hunk record of which path ran. Absent fields are omitted from JSON.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttemptInfo {
    pub decision: Option<DecisionInfo>,
    pub resolver: Option<ResolverPath>,
    pub escalation_reason: Option<EscalationReason>,
}

impl Choice {
    pub fn parse_label(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ours" | "our" | "ours_side" => Some(Self::Ours),
            "theirs" | "their" | "theirs_side" => Some(Self::Theirs),
            "merged" | "merge" | "both" => Some(Self::Merged),
            _ => None,
        }
    }
}

/// Result of attempting to resolve one hunk.
#[derive(Debug, Clone, PartialEq)]
pub enum HunkOutcome {
    Unresolved,
    Resolved(Resolution),
    Rejected(String),
}

impl HunkOutcome {
    pub fn resolution(&self) -> Option<&Resolution> {
        match self {
            Self::Resolved(r) => Some(r),
            _ => None,
        }
    }

    pub fn rejection(&self) -> Option<&str> {
        match self {
            Self::Rejected(s) => Some(s),
            _ => None,
        }
    }
}

/// A successful resolution of one hunk. Produced by ENG-56's model client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub text: String,
    pub choice: Choice,
    pub rationale: String,
    pub confidence: f64,
}

/// Pluggable hunk resolver. ENG-55 ships a no-op; ENG-56 supplies OpenRouter.
pub trait Resolver {
    fn resolve(&mut self, path: &str, hunk: &Hunk) -> Result<Option<Resolution>>;
}

/// Always leaves hunks unresolved. Used until a real resolver is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullResolver;

impl Resolver for NullResolver {
    fn resolve(&mut self, _path: &str, _hunk: &Hunk) -> Result<Option<Resolution>> {
        Ok(None)
    }
}

/// Rebuild a file body from resolved hunks.
///
/// Returns `Ok(None)` when any hunk is missing a resolution — callers must not
/// write a partial file. Replacements cover the full marker span
/// (`<<<<<<<` through `>>>>>>>`).
pub fn reconstruct(
    parsed: &ParsedFile,
    resolutions: &[Option<Resolution>],
) -> Result<Option<String>> {
    if parsed.hunks.is_empty() {
        return Ok(None);
    }
    if parsed.hunks.len() != resolutions.len() {
        return Err(TruceError::Write {
            path: PathBuf::from("<memory>"),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "internal error: {} hunks but {} resolutions",
                    parsed.hunks.len(),
                    resolutions.len()
                ),
            ),
        });
    }
    if resolutions.iter().any(|r| r.is_none()) {
        return Ok(None);
    }

    let mut out: Vec<String> = Vec::new();
    let mut line_no = 1usize;
    let mut hunk_idx = 0usize;
    while line_no <= parsed.lines.len() {
        if hunk_idx < parsed.hunks.len() && line_no == parsed.hunks[hunk_idx].start_line {
            let text = &resolutions[hunk_idx].as_ref().expect("checked None").text;
            out.extend(split_resolution(text, parsed.line_ending));
            line_no = parsed.hunks[hunk_idx].end_line + 1;
            hunk_idx += 1;
        } else {
            out.push(parsed.lines[line_no - 1].clone());
            line_no += 1;
        }
    }

    Ok(Some(join_lines(
        &out,
        parsed.line_ending,
        parsed.trailing_newline,
    )))
}

fn split_resolution(text: &str, file_eol: LineEnding) -> Vec<String> {
    let normalized = match file_eol {
        LineEnding::Crlf => text.replace("\r\n", "\n"),
        LineEnding::Lf => text.to_string(),
    };
    let trailing = normalized.ends_with('\n');
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if trailing {
        lines.pop();
    }
    // A resolution that is itself empty means "delete the hunk".
    if lines.len() == 1 && lines[0].is_empty() && text.is_empty() {
        lines.clear();
    }
    lines
}

/// Write `contents` to `path` via a same-directory temp file + rename.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| TruceError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let result = (|| {
        let mut file = fs::File::create(&tmp).map_err(|source| TruceError::Write {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(contents)
            .map_err(|source| TruceError::Write {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| TruceError::Write {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| TruceError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Build resolved text for a confident `ours`, `theirs`, or `both` decision.
///
/// * `ours` — the OURS lines, unchanged.
/// * `theirs` — the THEIRS lines, unchanged.
/// * `both` ([`Choice::Merged`]) — OURS lines, then THEIRS lines. When both
///   sides are non-empty and the last OURS line is byte-for-byte identical
///   to the first THEIRS line, that shared seam line is emitted once (the
///   OURS copy is kept). No other lines are removed: duplicates away from
///   the seam stay, and lines that differ only by whitespace stay. An empty
///   side contributes nothing.
///
/// Lines are joined so a trailing blank line survives hunk reconstruction
/// (a single trailing newline is the line terminator, not an empty line).
/// The caller must run [`validate_resolution`] before applying the text.
pub fn materialize_choice(choice: Choice, ours: &[String], theirs: &[String]) -> (String, String) {
    match choice {
        Choice::Ours => (
            join_resolution_lines(ours),
            "decision model selected ours".into(),
        ),
        Choice::Theirs => (
            join_resolution_lines(theirs),
            "decision model selected theirs".into(),
        ),
        Choice::Merged => {
            let (lines, dropped) = concat_both(ours, theirs);
            let rationale = if dropped {
                "decision model selected both: ours then theirs, dropping one identical seam line"
            } else {
                "decision model selected both: ours then theirs"
            };
            (join_resolution_lines(&lines), rationale.into())
        }
    }
}

fn concat_both(ours: &[String], theirs: &[String]) -> (Vec<String>, bool) {
    if ours.is_empty() {
        return (theirs.to_vec(), false);
    }
    if theirs.is_empty() {
        return (ours.to_vec(), false);
    }
    let dropped = ours.last() == theirs.first();
    let mut lines = ours.to_vec();
    if dropped {
        lines.extend_from_slice(&theirs[1..]);
    } else {
        lines.extend_from_slice(theirs);
    }
    (lines, dropped)
}

/// Inverse of reconstruction's line split for a list of resolved lines.
fn join_resolution_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut text = lines.join("\n");
    if lines.last().is_some_and(|line| line.is_empty()) {
        text.push('\n');
    }
    text
}

/// Accept a model payload or return a user-facing rejection reason.
pub fn validate_resolution(
    text: String,
    choice: Choice,
    rationale: String,
    confidence: f64,
    min_confidence: f64,
    ours: &[String],
    theirs: &[String],
) -> std::result::Result<Resolution, String> {
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(format!(
            "garbage output: confidence {confidence} is not in 0..=1"
        ));
    }
    if confidence < min_confidence {
        return Err(format!(
            "confidence {confidence:.2} below min {min_confidence:.2}"
        ));
    }
    if has_conflict_markers(&text) {
        return Err("leftover conflict markers in resolved text".into());
    }
    let both_empty = ours.iter().all(|l| l.is_empty()) && theirs.iter().all(|l| l.is_empty());
    if is_garbage(&text) && !both_empty {
        return Err("empty or garbage resolved text".into());
    }
    Ok(Resolution {
        text,
        choice,
        rationale,
        confidence,
    })
}

pub fn has_conflict_markers(text: &str) -> bool {
    text.lines().any(|line| {
        line.starts_with("<<<<<<<")
            || line.starts_with(">>>>>>>")
            || line.starts_with("|||||||")
            || line == "======="
            || line.starts_with("======= ")
    })
}

fn is_garbage(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.chars().all(|c| c.is_whitespace() || c == '\u{fffd}')
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("file"))
        .to_os_string();
    name.push(".truce.tmp");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_conflicts;

    fn res(text: &str) -> Resolution {
        Resolution {
            text: text.to_string(),
            choice: Choice::Merged,
            rationale: "test".into(),
            confidence: 1.0,
        }
    }

    #[test]
    fn reconstruct_replaces_marker_span() {
        let text = "keep\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\nend\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        let out = reconstruct(&parsed, &[Some(res("c"))]).unwrap().unwrap();
        assert_eq!(out, "keep\nc\nend\n");
    }

    #[test]
    fn reconstruct_refuses_partial_files() {
        let text = "\
<<<<<<< A
1
=======
2
>>>>>>> B
<<<<<<< A
3
=======
4
>>>>>>> B
";
        let parsed = parse_conflicts(text, 0).unwrap();
        let out = reconstruct(&parsed, &[Some(res("x")), None]).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn reconstruct_preserves_crlf() {
        let text = "keep\r\n<<<<<<< HEAD\r\na\r\n=======\r\nb\r\n>>>>>>> x\r\nend\r\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        let out = reconstruct(&parsed, &[Some(res("c"))]).unwrap().unwrap();
        assert_eq!(out, "keep\r\nc\r\nend\r\n");
    }

    #[test]
    fn write_atomic_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("truce-atomic-{}.txt", std::process::id()));
        write_atomic(&path, b"hello\n").unwrap();
        let got = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(got, b"hello\n");
    }

    #[test]
    fn null_resolver_never_resolves() {
        let text = "<<<<<<< A\n1\n=======\n2\n>>>>>>> B\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        let mut resolver = NullResolver;
        let got = resolver.resolve("f.txt", &parsed.hunks[0]).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn both_drops_only_the_identical_seam_line() {
        let ours = vec!["alpha".into(), "shared".into()];
        let theirs = vec!["shared".into(), "beta".into()];
        let (text, rationale) = materialize_choice(Choice::Merged, &ours, &theirs);
        assert_eq!(text, "alpha\nshared\nbeta");
        assert!(rationale.contains("seam"), "{rationale}");

        let (same, rationale) =
            materialize_choice(Choice::Merged, &["only".into()], &["only".into()]);
        assert_eq!(same, "only");
        assert!(rationale.contains("seam"), "{rationale}");
    }

    #[test]
    fn both_keeps_non_seam_duplicates_and_whitespace() {
        let ours = vec!["alpha".into(), "shared".into()];
        let theirs = vec!["shared ".into(), "alpha".into()];
        let (text, rationale) = materialize_choice(Choice::Merged, &ours, &theirs);
        assert_eq!(text, "alpha\nshared\nshared \nalpha");
        assert!(!rationale.contains("dropping"), "{rationale}");

        let (empty_side, _) = materialize_choice(Choice::Merged, &[], &["beta".into()]);
        assert_eq!(empty_side, "beta");
    }

    #[test]
    fn trailing_blank_line_survives_reconstruction() {
        let text = "keep\n<<<<<<< HEAD\nalpha\n\n=======\nbeta\n>>>>>>> x\nend\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        let (resolved, _) =
            materialize_choice(Choice::Ours, &parsed.hunks[0].ours, &parsed.hunks[0].theirs);
        let out = reconstruct(&parsed, &[Some(res(&resolved))])
            .unwrap()
            .unwrap();
        assert_eq!(out, "keep\nalpha\n\nend\n");
    }

    #[test]
    fn validate_rejects_markers_empty_and_low_confidence() {
        let ours = vec!["a".into()];
        let theirs = vec!["b".into()];
        assert!(validate_resolution(
            "<<<<<<< HEAD\na\n".into(),
            Choice::Merged,
            "x".into(),
            0.9,
            0.7,
            &ours,
            &theirs,
        )
        .is_err());
        assert!(validate_resolution(
            "   ".into(),
            Choice::Merged,
            "x".into(),
            0.9,
            0.7,
            &ours,
            &theirs,
        )
        .is_err());
        assert!(validate_resolution(
            "ok".into(),
            Choice::Merged,
            "x".into(),
            0.2,
            0.7,
            &ours,
            &theirs,
        )
        .is_err());
        assert!(validate_resolution(
            "ok".into(),
            Choice::Merged,
            "x".into(),
            0.9,
            0.7,
            &ours,
            &theirs,
        )
        .is_ok());
    }
}
