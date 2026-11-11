use serde::{Deserialize, Serialize};

/// How the working-tree file terminates lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            Self::Crlf
        } else {
            Self::Lf
        }
    }
}

/// One conflict hunk parsed from a working-tree file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// 1-based index of this hunk within the file.
    pub id: usize,
    /// 1-based line number of the `<<<<<<<` marker.
    pub start_line: usize,
    /// 1-based line number of the `>>>>>>>` marker.
    pub end_line: usize,
    pub ours_label: String,
    pub theirs_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_label: Option<String>,
    /// Lines between `<<<<<<<` and the next marker (`|||||||` or `=======`).
    pub ours: Vec<String>,
    /// Lines between `=======` and `>>>>>>>`.
    pub theirs: Vec<String>,
    /// Lines between `|||||||` and `=======`, when present (diff3/zdiff3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Vec<String>>,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

/// A UTF-8 working-tree file split into lines, plus any conflict hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub lines: Vec<String>,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
    pub hunks: Vec<Hunk>,
}

/// Split `text` into lines without panicking on missing newlines or CRLF.
pub fn split_text(text: &str) -> (Vec<String>, LineEnding, bool) {
    let line_ending = LineEnding::detect(text);
    let trailing_newline = text.ends_with('\n');
    let normalized = if line_ending == LineEnding::Crlf {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    };
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if trailing_newline {
        // `split` yields a trailing empty string after the final newline.
        lines.pop();
    }
    (lines, line_ending, trailing_newline)
}

/// Parse git conflict markers from a UTF-8 file body.
///
/// Supports standard 2-way markers and optional diff3/zdiff3 `|||||||` base
/// sections. Multiple hunks per file are collected in order. Malformed or
/// unterminated markers return `Err` with a file-relative message — never panic.
pub fn parse_conflicts(
    text: &str,
    context_lines: usize,
) -> std::result::Result<ParsedFile, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (lines, line_ending, trailing_newline) = split_text(text);
    let hunk_bodies = collect_hunks(&lines)?;

    let hunks = hunk_bodies
        .into_iter()
        .enumerate()
        .map(|(idx, body)| body.into_hunk(idx + 1, &lines, context_lines))
        .collect();

    Ok(ParsedFile {
        lines,
        line_ending,
        trailing_newline,
        hunks,
    })
}

#[derive(Debug)]
struct HunkBody {
    start_line: usize,
    end_line: usize,
    ours_label: String,
    theirs_label: String,
    base_label: Option<String>,
    ours: Vec<String>,
    theirs: Vec<String>,
    base: Option<Vec<String>>,
}

impl HunkBody {
    fn into_hunk(self, id: usize, lines: &[String], context_lines: usize) -> Hunk {
        let before_start = self.start_line.saturating_sub(1 + context_lines);
        let context_before = lines[before_start..self.start_line.saturating_sub(1)].to_vec();
        let after_start = self.end_line.min(lines.len());
        let after_end = (self.end_line + context_lines).min(lines.len());
        let context_after = lines[after_start..after_end].to_vec();

        Hunk {
            id,
            start_line: self.start_line,
            end_line: self.end_line,
            ours_label: self.ours_label,
            theirs_label: self.theirs_label,
            base_label: self.base_label,
            ours: self.ours,
            theirs: self.theirs,
            base: self.base,
            context_before,
            context_after,
        }
    }
}

#[derive(Debug)]
enum State {
    Outside,
    Ours {
        start_line: usize,
        ours_label: String,
        ours: Vec<String>,
    },
    Base {
        start_line: usize,
        ours_label: String,
        ours: Vec<String>,
        base_label: String,
        base: Vec<String>,
    },
    Theirs {
        start_line: usize,
        ours_label: String,
        ours: Vec<String>,
        base_label: Option<String>,
        base: Option<Vec<String>>,
        theirs: Vec<String>,
    },
}

enum Marker<'a> {
    Ours(&'a str),
    Base(&'a str),
    Sep,
    Theirs(&'a str),
}

fn marker(line: &str) -> Option<Marker<'_>> {
    if let Some(rest) = line.strip_prefix("<<<<<<<") {
        Some(Marker::Ours(rest.trim()))
    } else if let Some(rest) = line.strip_prefix("|||||||") {
        Some(Marker::Base(rest.trim()))
    } else if line.starts_with("=======") {
        Some(Marker::Sep)
    } else {
        line.strip_prefix(">>>>>>>")
            .map(|rest| Marker::Theirs(rest.trim()))
    }
}

fn collect_hunks(lines: &[String]) -> std::result::Result<Vec<HunkBody>, String> {
    let mut state = State::Outside;
    let mut hunks = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        match (marker(line), &mut state) {
            (Some(Marker::Ours(label)), State::Outside) => {
                state = State::Ours {
                    start_line: line_no,
                    ours_label: label.to_string(),
                    ours: Vec::new(),
                };
            }
            (Some(Marker::Ours(_)), _) => {
                return Err(format!(
                    "malformed conflict markers: nested '<<<<<<<' at line {line_no}"
                ));
            }
            (
                Some(Marker::Base(label)),
                State::Ours {
                    start_line,
                    ours_label,
                    ours,
                },
            ) => {
                state = State::Base {
                    start_line: *start_line,
                    ours_label: std::mem::take(ours_label),
                    ours: std::mem::take(ours),
                    base_label: label.to_string(),
                    base: Vec::new(),
                };
            }
            (Some(Marker::Base(_)), _) => {
                return Err(format!(
                    "malformed conflict markers: unexpected '|||||||' at line {line_no}"
                ));
            }
            (
                Some(Marker::Sep),
                State::Ours {
                    start_line,
                    ours_label,
                    ours,
                },
            ) => {
                state = State::Theirs {
                    start_line: *start_line,
                    ours_label: std::mem::take(ours_label),
                    ours: std::mem::take(ours),
                    base_label: None,
                    base: None,
                    theirs: Vec::new(),
                };
            }
            (
                Some(Marker::Sep),
                State::Base {
                    start_line,
                    ours_label,
                    ours,
                    base_label,
                    base,
                },
            ) => {
                state = State::Theirs {
                    start_line: *start_line,
                    ours_label: std::mem::take(ours_label),
                    ours: std::mem::take(ours),
                    base_label: Some(std::mem::take(base_label)),
                    base: Some(std::mem::take(base)),
                    theirs: Vec::new(),
                };
            }
            (Some(Marker::Sep), _) => {
                return Err(format!(
                    "malformed conflict markers: unexpected '=======' at line {line_no}"
                ));
            }
            (
                Some(Marker::Theirs(label)),
                State::Theirs {
                    start_line,
                    ours_label,
                    ours,
                    base_label,
                    base,
                    theirs,
                },
            ) => {
                hunks.push(HunkBody {
                    start_line: *start_line,
                    end_line: line_no,
                    ours_label: std::mem::take(ours_label),
                    theirs_label: label.to_string(),
                    base_label: base_label.take(),
                    ours: std::mem::take(ours),
                    theirs: std::mem::take(theirs),
                    base: base.take(),
                });
                state = State::Outside;
            }
            (Some(Marker::Theirs(_)), _) => {
                return Err(format!(
                    "malformed conflict markers: unexpected '>>>>>>>' at line {line_no}"
                ));
            }
            (None, State::Ours { ours, .. }) => ours.push(line.clone()),
            (None, State::Base { base, .. }) => base.push(line.clone()),
            (None, State::Theirs { theirs, .. }) => theirs.push(line.clone()),
            (None, State::Outside) => {}
        }
    }

    match state {
        State::Outside => Ok(hunks),
        State::Ours { start_line, .. }
        | State::Base { start_line, .. }
        | State::Theirs { start_line, .. } => Err(format!(
            "unterminated conflict marker starting at line {start_line}"
        )),
    }
}

/// Join lines back into a file body, preserving EOL style and the original
/// trailing-newline policy.
pub fn join_lines(lines: &[String], line_ending: LineEnding, trailing_newline: bool) -> String {
    let mut body = lines.join(line_ending.as_str());
    if trailing_newline {
        body.push_str(line_ending.as_str());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(hunks: &[Hunk]) -> Vec<usize> {
        hunks.iter().map(|h| h.id).collect()
    }

    #[test]
    fn two_way_conflict() {
        let text = "\
keep
<<<<<<< HEAD
ours line
=======
theirs line
>>>>>>> feature
tail
";
        let parsed = parse_conflicts(text, 1).unwrap();
        assert_eq!(parsed.hunks.len(), 1);
        let hunk = &parsed.hunks[0];
        assert_eq!(hunk.start_line, 2);
        assert_eq!(hunk.end_line, 6);
        assert_eq!(hunk.ours_label, "HEAD");
        assert_eq!(hunk.theirs_label, "feature");
        assert_eq!(hunk.ours, ["ours line"]);
        assert_eq!(hunk.theirs, ["theirs line"]);
        assert!(hunk.base.is_none());
        assert_eq!(hunk.context_before, ["keep"]);
        assert_eq!(hunk.context_after, ["tail"]);
        assert!(parsed.trailing_newline);
        assert_eq!(parsed.line_ending, LineEnding::Lf);
    }

    #[test]
    fn diff3_base_section() {
        let text = "\
<<<<<<< ours
left
||||||| merged common ancestors
old
=======
right
>>>>>>> theirs
";
        let parsed = parse_conflicts(text, 0).unwrap();
        let hunk = &parsed.hunks[0];
        assert_eq!(hunk.base_label.as_deref(), Some("merged common ancestors"));
        assert_eq!(hunk.base, Some(vec!["old".into()]));
        assert_eq!(hunk.ours, ["left"]);
        assert_eq!(hunk.theirs, ["right"]);
    }

    #[test]
    fn multiple_hunks() {
        let text = "\
a
<<<<<<< A
1
=======
2
>>>>>>> B
mid
<<<<<<< A
3
=======
4
>>>>>>> B
z
";
        let parsed = parse_conflicts(text, 1).unwrap();
        assert_eq!(ids(&parsed.hunks), [1, 2]);
        assert_eq!(parsed.hunks[0].ours, ["1"]);
        assert_eq!(parsed.hunks[1].theirs, ["4"]);
        assert_eq!(parsed.hunks[1].context_before, ["mid"]);
        assert_eq!(parsed.hunks[0].context_after, ["mid"]);
    }

    #[test]
    fn crlf_line_endings() {
        let text = "<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> feat\r\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        assert_eq!(parsed.line_ending, LineEnding::Crlf);
        assert_eq!(parsed.hunks[0].ours, ["ours"]);
        assert_eq!(parsed.hunks[0].theirs, ["theirs"]);
        let rebuilt = join_lines(&parsed.lines, parsed.line_ending, parsed.trailing_newline);
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn no_trailing_newline() {
        let text = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feat";
        let parsed = parse_conflicts(text, 0).unwrap();
        assert!(!parsed.trailing_newline);
        assert_eq!(parsed.hunks[0].end_line, 5);
        let rebuilt = join_lines(&parsed.lines, parsed.line_ending, parsed.trailing_newline);
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn unterminated_marker_is_error() {
        let text = "<<<<<<< HEAD\nours\n=======\ntheirs\n";
        let err = parse_conflicts(text, 0).unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn nested_start_is_malformed() {
        let text = "<<<<<<< A\n<<<<<<< B\n=======\nx\n>>>>>>> C\n";
        let err = parse_conflicts(text, 0).unwrap_err();
        assert!(err.contains("nested"), "{err}");
    }

    #[test]
    fn stray_separator_is_malformed() {
        let err = parse_conflicts("=======\n", 0).unwrap_err();
        assert!(err.contains("unexpected '======='"), "{err}");
    }

    #[test]
    fn empty_sides_are_allowed() {
        let text = "<<<<<<< HEAD\n=======\n>>>>>>> other\n";
        let parsed = parse_conflicts(text, 0).unwrap();
        assert!(parsed.hunks[0].ours.is_empty());
        assert!(parsed.hunks[0].theirs.is_empty());
    }

    #[test]
    fn fixture_two_way_file() {
        let text = include_str!("../tests/fixtures/two_way.txt");
        let parsed = parse_conflicts(text, 2).unwrap();
        assert_eq!(parsed.hunks.len(), 1);
        assert_eq!(parsed.hunks[0].ours_label, "HEAD");
        assert_eq!(parsed.hunks[0].theirs_label, "feature");
        assert_eq!(parsed.hunks[0].ours, ["return 1;", ""]);
        assert_eq!(parsed.hunks[0].theirs, ["return 2;", ""]);
        assert_eq!(parsed.hunks[0].context_before, ["fn answer() {"]);
        assert_eq!(parsed.hunks[0].context_after[0], "}");
    }

    #[test]
    fn fixture_diff3_file() {
        let text = include_str!("../tests/fixtures/diff3.txt");
        let parsed = parse_conflicts(text, 0).unwrap();
        assert_eq!(
            parsed.hunks[0].base_label.as_deref(),
            Some("merged common ancestors")
        );
        assert_eq!(
            parsed.hunks[0].base,
            Some(vec!["alpha".into(), String::new()])
        );
    }

    #[test]
    fn fixture_multiple_hunks() {
        let text = include_str!("../tests/fixtures/multiple_hunks.txt");
        let parsed = parse_conflicts(text, 0).unwrap();
        assert_eq!(parsed.hunks.len(), 2);
        assert_eq!(parsed.hunks[0].ours, ["one-a"]);
        assert_eq!(parsed.hunks[1].theirs, ["two-b"]);
    }
}
