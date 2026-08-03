//! Sanitized source documents shared by discovery and deterministic extractors.

use serde::{Deserialize, Serialize};

use crate::repository_graph::domain::{RepoPath, SnapshotId, SourcePosition, SourceSpan};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArchiveSourceDocument {
    pub archive_id: String,
    pub spec_path: RepoPath,
    pub archived_at: String,
    pub task_count: u64,
    pub run_count: u64,
    pub task_ids: Vec<String>,
    pub milestone_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeSourceDocument {
    pub tasks: Vec<RuntimeTaskDocument>,
    pub runs: Vec<RuntimeRunDocument>,
    pub checks: Vec<RuntimeCheckDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeTaskDocument {
    pub id: String,
    pub milestone_id: Option<String>,
    pub status: String,
    pub baseline_snapshot_id: Option<SnapshotId>,
    pub repository_snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeRunDocument {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub check_ids: Vec<String>,
    pub baseline_snapshot_id: Option<SnapshotId>,
    pub repository_snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeCheckDocument {
    pub id: String,
    pub task_id: String,
    pub run_id: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SpecStructureDocument {
    pub title: Option<String>,
    pub milestones: Vec<SpecMilestoneDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SpecMilestoneDocument {
    pub id: String,
    pub title: String,
    pub completed: bool,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SpecOutcomeDocument {
    pub text: String,
    pub span: SourceSpan,
    pub sections: Vec<OutcomeSection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) enum OutcomeSectionKind {
    Decision,
    Deviation,
    Validation,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OutcomeSection {
    pub kind: OutcomeSectionKind,
    pub text: String,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSpecMemory {
    pub structure: SpecStructureDocument,
    pub title_span: Option<SourceSpan>,
    pub outcome: Option<SpecOutcomeDocument>,
}

#[derive(Debug, Clone)]
struct Line<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    number: u32,
}

pub(crate) fn parse_spec_memory(content: &str) -> ParsedSpecMemory {
    let lines = lines(content);
    let title_line = lines
        .iter()
        .find(|line| line.text.trim_start().starts_with("# "));
    let title = title_line
        .map(|line| line.text.trim().trim_start_matches("# ").trim())
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let title_span = title_line.map(|line| span_for_lines(line, line));

    let mut milestones = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((completed, title)) = milestone_header(line.text) else {
            continue;
        };
        let end = lines
            .iter()
            .skip(index + 1)
            .position(|candidate| milestone_header(candidate.text).is_some())
            .map_or(lines.len(), |offset| index + 1 + offset);
        let id = lines[index + 1..end]
            .iter()
            .find_map(|candidate| child_field(candidate.text, "ID"));
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            milestones.push(SpecMilestoneDocument {
                id: id.to_string(),
                title: title.to_string(),
                completed,
                span: span_for_lines(line, line),
            });
        }
    }

    let outcome = lines
        .iter()
        .position(|line| line.text.trim_end() == "## Outcome")
        .and_then(|start| parse_outcome(&lines, start, content));
    ParsedSpecMemory {
        structure: SpecStructureDocument { title, milestones },
        title_span,
        outcome,
    }
}

fn parse_outcome(lines: &[Line<'_>], start: usize, content: &str) -> Option<SpecOutcomeDocument> {
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| line.text.trim_start().starts_with("## ").then_some(index))
        .unwrap_or(lines.len());
    let body_start = start + 1;
    let body = text_for_line_range(lines, body_start, end, content);
    if body.trim().is_empty() {
        return None;
    }
    let first = lines.get(body_start).unwrap_or(&lines[start]);
    let last = lines.get(end.saturating_sub(1)).unwrap_or(&lines[start]);
    let mut sections = Vec::new();
    let headings = (body_start..end)
        .filter_map(|index| {
            let title = lines[index].text.trim().strip_prefix("### ")?.trim();
            section_kind(title).map(|kind| (index, kind))
        })
        .collect::<Vec<_>>();
    for (position, (heading, kind)) in headings.iter().enumerate() {
        let section_end = headings.get(position + 1).map_or(end, |(next, _)| *next);
        let section_start = heading + 1;
        let text = text_for_line_range(lines, section_start, section_end, content);
        if text.trim().is_empty() {
            continue;
        }
        let section_first = lines.get(section_start).unwrap_or(&lines[*heading]);
        let section_last = lines
            .get(section_end.saturating_sub(1))
            .unwrap_or(&lines[*heading]);
        sections.push(OutcomeSection {
            kind: *kind,
            text: text.trim().to_string(),
            span: span_for_lines(section_first, section_last),
        });
    }
    Some(SpecOutcomeDocument {
        text: body.trim().to_string(),
        span: span_for_lines(first, last),
        sections,
    })
}

fn lines(content: &str) -> Vec<Line<'_>> {
    let mut result = Vec::new();
    let mut start = 0usize;
    for (index, part) in content.split_inclusive('\n').enumerate() {
        let end = start + part.len();
        result.push(Line {
            text: part.trim_end_matches(['\n', '\r']),
            start,
            end,
            number: u32::try_from(index + 1).unwrap_or(u32::MAX),
        });
        start = end;
    }
    if content.is_empty() {
        result.push(Line {
            text: "",
            start: 0,
            end: 0,
            number: 1,
        });
    } else if !content.ends_with('\n') && result.is_empty() {
        result.push(Line {
            text: content,
            start: 0,
            end: content.len(),
            number: 1,
        });
    }
    result
}

fn text_for_line_range(lines: &[Line<'_>], start: usize, end: usize, content: &str) -> String {
    let Some(first) = lines.get(start) else {
        return String::new();
    };
    let last_end = lines
        .get(end.saturating_sub(1))
        .map_or(first.start, |line| line.end);
    content[first.start..last_end].to_string()
}

fn span_for_lines(first: &Line<'_>, last: &Line<'_>) -> SourceSpan {
    SourceSpan {
        start: SourcePosition {
            byte_offset: first.start as u64,
            line: Some(first.number),
            column: Some(1),
        },
        end: SourcePosition {
            byte_offset: last.end as u64,
            line: Some(last.number),
            column: Some(u32::try_from(last.text.len() + 1).unwrap_or(u32::MAX)),
        },
    }
}

fn milestone_header(line: &str) -> Option<(bool, &str)> {
    let line = line.trim();
    let rest = line.strip_prefix("- [")?;
    let completed = match rest.as_bytes().first().copied()? {
        b'x' | b'X' => true,
        b' ' => false,
        _ => return None,
    };
    let header = rest.get(1..)?.strip_prefix(']')?.trim();
    let mut parts = header.splitn(2, char::is_whitespace);
    let marker = parts.next()?;
    let title = parts.next().unwrap_or_default().trim();
    (marker.len() > 1 && marker.starts_with('#') && !title.is_empty()).then_some((completed, title))
}

fn child_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.trim()
        .strip_prefix("- ")
        .unwrap_or(line.trim())
        .strip_prefix(field)?
        .strip_prefix(':')
        .map(str::trim)
}

fn section_kind(title: &str) -> Option<OutcomeSectionKind> {
    let normalized = title
        .to_ascii_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match normalized.as_str() {
        "decision" | "decisions" => Some(OutcomeSectionKind::Decision),
        "deviation" | "deviations" => Some(OutcomeSectionKind::Deviation),
        "validation" | "validation evidence" | "verification" => {
            Some(OutcomeSectionKind::Validation)
        }
        "follow up" | "follow up work" | "remaining work" => Some(OutcomeSectionKind::FollowUp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_milestones_and_curated_outcome_subsections() {
        let parsed = parse_spec_memory(
            "# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n\n## Outcome\n\nDelivered.\n\n### Decisions\n\nUse SQLite.\n\n### Follow-up Work\n\nAdd CLI.\n",
        );
        assert_eq!(parsed.structure.title.as_deref(), Some("Example"));
        assert_eq!(parsed.structure.milestones.len(), 1);
        assert!(parsed.structure.milestones[0].completed);
        let outcome = parsed.outcome.unwrap();
        assert!(outcome.text.contains("Delivered"));
        assert_eq!(outcome.sections.len(), 2);
        assert_eq!(outcome.sections[0].kind, OutcomeSectionKind::Decision);
        assert_eq!(outcome.sections[1].kind, OutcomeSectionKind::FollowUp);
    }
}
