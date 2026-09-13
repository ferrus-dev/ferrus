//! Bounded current-workspace tools. Host roots never come from model arguments.

mod fs;
pub(crate) mod patch;
#[cfg(test)]
mod tests;

use super::{journal::encode, tools::*};
use crate::repository_graph::domain::RepoPath;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

use patch::PatchRequest;

#[derive(Debug, Clone)]
pub(crate) struct Limits {
    pub file_bytes: usize,
    pub scan_bytes: usize,
    pub entries: usize,
    pub output_bytes: usize,
    pub elapsed_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            file_bytes: 1024 * 1024,
            scan_bytes: 8 * 1024 * 1024,
            entries: 4096,
            output_bytes: 24 * 1024,
            elapsed_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Code {
    InvalidPath,
    ProtectedPath,
    UnsafeFile,
    NotFound,
    Io,
    Binary,
    FileTooLarge,
    Conflict,
    InvalidPatch,
    OutputLimit,
    Interrupted,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Failure {
    pub code: Code,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_digest: Option<String>,
    pub message: &'static str,
}

impl Failure {
    fn new(code: Code, path: &str) -> Self {
        Self {
            code,
            path: path.chars().take(256).collect(),
            current_digest: None,
            message: match code {
                Code::Conflict => "Base changed; read_file again and rebuild exact hunks.",
                Code::InvalidPatch => {
                    "Use ordered, non-overlapping, exact whole-line hunks against the expected digest."
                }
                Code::NotFound => {
                    "Path or parent directory is missing; parents must already exist."
                }
                Code::ProtectedPath => {
                    "Git metadata and Ferrus runtime state are not file-tool targets."
                }
                Code::UnsafeFile => {
                    "Symlinks, reparse points, hardlinks, and non-regular files are unsupported."
                }
                Code::Binary => "Only UTF-8 text without NUL bytes is supported.",
                Code::FileTooLarge => "File exceeds the host's content limit.",
                Code::InvalidPath => {
                    "Use a portable relative path inside the authorized workspace."
                }
                Code::OutputLimit => "Narrow the request to fit the host's output limit.",
                Code::Interrupted => {
                    "Operation stopped at a bounded cancellation or deadline checkpoint."
                }
                Code::Io => {
                    "Filesystem operation failed; inspect the reported per-path state before retrying."
                }
            },
        }
    }
    fn io(path: &str, error: std::io::Error) -> Self {
        Self::new(
            match error.kind() {
                std::io::ErrorKind::NotFound => Code::NotFound,
                std::io::ErrorKind::InvalidInput => Code::InvalidPath,
                std::io::ErrorKind::Unsupported => Code::UnsafeFile,
                _ => Code::Io,
            },
            path,
        )
    }
}

type Result<T> = std::result::Result<T, Failure>;

pub(crate) struct Workspace {
    root: fs::Root,
    id: String,
    generation: u64,
    limits: Limits,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Source {
    pub kind: &'static str,
    pub workspace_id: String,
    pub generation: u64,
    pub path: String,
    pub digest: String,
}

#[derive(Debug)]
struct Content {
    text: String,
    digest: String,
    mode: std::fs::Permissions,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn path(value: &str) -> Result<String> {
    if value.len() > 256 || value.contains('\\') {
        return Err(Failure::new(Code::InvalidPath, value));
    }

    let parsed = RepoPath::new(value).map_err(|_| Failure::new(Code::InvalidPath, value))?;
    for component in parsed.as_str().split('/') {
        let lower = component.to_uppercase().to_ascii_lowercase();
        if matches!(lower.as_str(), ".git" | ".ferrus")
            || lower.starts_with(".nano-tmp-")
            || ["ferrus.db", "repo-graph.db", "project-memory.db"]
                .iter()
                .any(|name| {
                    lower == *name
                        || lower
                            .strip_prefix(name)
                            .is_some_and(|suffix| matches!(suffix, "-wal" | "-shm" | "-journal"))
                })
        {
            return Err(Failure::new(Code::ProtectedPath, value));
        }

        let stem = lower.split('.').next().unwrap_or_default();
        if component.ends_with(['.', ' '])
            || component
                .chars()
                .any(|c| c.is_control() || ":*?\"<>|~".contains(c))
            || matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
            || ["com", "lpt"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|s| {
                    s.chars().count() == 1 && "123456789\u{b9}\u{b2}\u{b3}".contains(s)
                })
            })
        {
            return Err(Failure::new(Code::InvalidPath, value));
        }
    }

    Ok(parsed.as_str().to_owned())
}

impl Workspace {
    /// The caller selects an authorized root and limits after validating host authority.
    pub(crate) fn new(root: &Path, limits: Limits) -> anyhow::Result<Self> {
        anyhow::ensure!(root.is_absolute(), "Workspace root must be absolute");
        anyhow::ensure!(
            limits.file_bytes > 0
                && limits.file_bytes <= 16 * 1024 * 1024
                && limits.scan_bytes >= limits.file_bytes
                && limits.scan_bytes <= 64 * 1024 * 1024
                && (1..=65_536).contains(&limits.entries)
                && (4096..=24 * 1024).contains(&limits.output_bytes)
                && (1..=30_000).contains(&limits.elapsed_ms),
            "Invalid workspace limits"
        );

        let canonical = root.canonicalize()?;
        let root = fs::Root::new(&canonical)?;
        Ok(Self {
            id: digest(canonical.as_os_str().as_encoded_bytes()),
            root,
            limits,
            generation: 0,
        })
    }

    fn source(&self, path: &str, digest: &str) -> Source {
        Source {
            kind: "workspace",
            workspace_id: self.id.clone(),
            generation: self.generation,
            path: path.into(),
            digest: digest.into(),
        }
    }

    fn content(&self, path: &str) -> Result<Content> {
        let file = self.root.open(path).map_err(|e| Failure::io(path, e))?;
        self.read_content(file, path)
    }

    fn read_content(&self, file: std::fs::File, path: &str) -> Result<Content> {
        self.read_content_limited(file, path, self.limits.file_bytes + 1, &mut 0)
    }

    fn read_content_limited(
        &self,
        mut file: std::fs::File,
        path: &str,
        read_budget: usize,
        inspected: &mut usize,
    ) -> Result<Content> {
        let limit = self.limits.file_bytes.min(read_budget);
        let meta = file.metadata().map_err(|e| Failure::io(path, e))?;
        fs::regular(&file).map_err(|e| Failure::io(path, e))?;
        if meta.len() > limit as u64 {
            return Err(Failure::new(Code::FileTooLarge, path));
        }

        let mut bytes = Vec::new();
        let read_limit = (limit + 1).min(read_budget);
        let read = file
            .by_ref()
            .take(read_limit as u64)
            .read_to_end(&mut bytes);
        *inspected += bytes.len();
        read.map_err(|e| Failure::io(path, e))?;
        // Probe growth only within the read budget. If content uses it all,
        // recheck the opened file's length instead of reading an extra byte.
        if bytes.len() > limit
            || (bytes.len() == read_budget
                && file.metadata().map_err(|e| Failure::io(path, e))?.len() > limit as u64)
        {
            return Err(Failure::new(Code::FileTooLarge, path));
        }

        if bytes.contains(&0) {
            return Err(Failure::new(Code::Binary, path));
        }

        let hash = digest(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| Failure::new(Code::Binary, path))?;
        Ok(Content {
            text,
            digest: hash,
            mode: meta.permissions(),
        })
    }

    fn stopped(&self, start: Instant, cancellation: &Cancellation) -> bool {
        cancellation.is_cancelled()
            || start.elapsed() >= Duration::from_millis(self.limits.elapsed_ms)
    }

    pub(crate) fn read_file(&self, request: ReadRequest) -> Result<ReadResult> {
        let path = path(&request.path)?;
        if request.start_line == 0 || request.max_lines == 0 || request.max_bytes == 0 {
            return Err(Failure::new(Code::InvalidPath, &path));
        }

        let content = self.content(&path)?;
        let lines = request.max_lines.min(2000);
        // Charge actual JSON escaping while leaving room for source identity and the wrapper.
        let bytes = request.max_bytes.min(self.limits.output_bytes - 1024);
        let mut encoded_bytes = 0;
        let mut text = String::new();
        let mut returned_lines = 0;
        let mut truncated = false;

        for line in content
            .text
            .split_inclusive('\n')
            .skip(request.start_line - 1)
        {
            if returned_lines == lines || text.len() + line.len() > bytes {
                truncated = true;
                break;
            }

            let encoded = serde_json::to_string(line)
                .expect("serializable text")
                .len()
                - 2;

            if encoded_bytes + encoded > self.limits.output_bytes - 1024 {
                truncated = true;
                break;
            }

            encoded_bytes += encoded;
            text.push_str(line);
            returned_lines += 1;
        }

        Ok(ReadResult {
            source: self.source(&path, &content.digest),
            start_line: request.start_line,
            returned_lines,
            next_line: truncated.then_some(request.start_line + returned_lines),
            text,
            truncated,
        })
    }
    pub(crate) async fn search_text(
        &self,
        request: SearchRequest,
        cancellation: &Cancellation,
    ) -> Result<SearchResult> {
        let start = Instant::now();
        let mut pending = BTreeMap::new();
        for value in &request.paths {
            let path = if value == "." {
                String::new()
            } else {
                path(value)?
            };
            pending.entry(fs::search_key(&path)).or_insert(path);
        }

        if pending.is_empty()
            || pending.len() > 16
            || request.query.is_empty()
            || request.query.len() > 4096
            || request.max_results == 0
        {
            return Err(Failure::new(Code::InvalidPath, ""));
        }

        let mut result = SearchResult {
            matches: Vec::new(),
            issues: Vec::new(),
            scanned_bytes: 0,
            visited_entries: 0,
            listed_entries: 0,
            truncated: false,
            suppressed_issues: 0,
        };

        let mut seen = BTreeSet::new();
        let mut objects = BTreeSet::new();
        let mut output_bytes = 512;
        let mut output_full = false;

        while let Some((key, path)) = pending.pop_first() {
            if self.stopped(start, cancellation) {
                result.truncated = true;
                result.issue(
                    Failure::new(Code::Interrupted, ""),
                    &mut output_bytes,
                    self.limits.output_bytes,
                );
                break;
            }

            if !seen.insert(key) {
                continue;
            }

            let file = if path.is_empty() {
                self.root.directory()
            } else {
                self.root.open(&path)
            }
            .and_then(|file| fs::identity(&file).map(|identity| (file, identity)));

            // Unix volumes can ignore case or normalize Unicode names. Use the
            // opened object's identity before charging visits, listing or reads.
            if let Ok((_, identity)) = &file
                && !objects.insert(*identity)
            {
                continue;
            }

            if result.visited_entries == self.limits.entries {
                result.truncated = true;
                break;
            }

            result.visited_entries += 1;

            let item = (|| {
                let (file, _) = file.map_err(|e| Failure::io(&path, e))?;
                if file.metadata().map_err(|e| Failure::io(&path, e))?.is_dir() {
                    let capacity = self.limits.entries.saturating_sub(result.listed_entries);
                    let (children, truncated) =
                        fs::children(&file, capacity).map_err(|e| Failure::io(&path, e))?;

                    result.truncated |= truncated;
                    result.listed_entries += children.len();

                    for name in children {
                        let child = if path.is_empty() {
                            name
                        } else {
                            format!("{path}/{name}")
                        };

                        match self::path(&child) {
                            Ok(child) => {
                                let key = fs::search_key(&child);
                                if !seen.contains(&key) {
                                    pending.entry(key).or_insert(child);
                                }
                            }
                            Err(failure) if failure.code == Code::ProtectedPath => (),
                            Err(failure) => {
                                result.truncated = true;
                                result.issue(failure, &mut output_bytes, self.limits.output_bytes);
                            }
                        }
                    }

                    return Ok(());
                }
                let remaining = self.limits.scan_bytes.saturating_sub(result.scanned_bytes);
                if remaining == 0 {
                    result.truncated = true;
                    return Err(Failure::new(Code::FileTooLarge, &path));
                }

                let content =
                    self.read_content_limited(file, &path, remaining, &mut result.scanned_bytes)?;

                for (index, line) in content.text.split_inclusive('\n').enumerate() {
                    let Some(column) = line.find(&request.query) else {
                        continue;
                    };

                    if result.matches.len() >= request.max_results.min(128) {
                        result.truncated = true;
                        break;
                    }

                    let mut snippet_start = column.saturating_sub(120);
                    while !line.is_char_boundary(snippet_start) {
                        snippet_start -= 1;
                    }

                    let snippet = prefix(&line[snippet_start..], 512);
                    let hit = SearchMatch {
                        source: self.source(&path, &content.digest),
                        line: index + 1,
                        byte_column: column + 1,
                        snippet_start_column: snippet_start + 1,
                        text: snippet.to_owned(),
                        truncated: snippet.len() < line.len(),
                    };

                    let size = serde_json::to_vec(&hit).expect("serializable match").len() + 1;
                    if output_bytes + size > self.limits.output_bytes - 512 {
                        result.truncated = true;
                        output_full = true;
                        break;
                    }

                    output_bytes += size;
                    result.matches.push(hit);
                }

                Ok(())
            })();

            if let Err(failure) = item {
                result.truncated = true;
                result.issue(failure, &mut output_bytes, self.limits.output_bytes);
            }

            if result.matches.len() >= request.max_results.min(128) || output_full {
                result.truncated |= !pending.is_empty();
                break;
            }

            tokio::task::yield_now().await;
        }

        Ok(result)
    }
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn one() -> usize {
    1
}

fn lines() -> usize {
    200
}

fn bytes() -> usize {
    16 * 1024
}

fn results() -> usize {
    50
}

fn roots() -> Vec<String> {
    vec![".".into()]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadRequest {
    pub path: String,
    #[serde(default = "one")]
    pub start_line: usize,
    #[serde(default = "lines")]
    pub max_lines: usize,
    #[serde(default = "bytes")]
    pub max_bytes: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadResult {
    pub source: Source,
    pub start_line: usize,
    pub returned_lines: usize,
    pub next_line: Option<usize>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchRequest {
    #[serde(default = "roots")]
    pub paths: Vec<String>,
    pub query: String,
    #[serde(default = "results")]
    pub max_results: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchMatch {
    pub source: Source,
    pub line: usize,
    pub byte_column: usize,
    pub snippet_start_column: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub issues: Vec<Failure>,
    pub suppressed_issues: usize,
    pub scanned_bytes: usize,
    pub visited_entries: usize,
    pub listed_entries: usize,
    pub truncated: bool,
}

impl SearchResult {
    fn issue(&mut self, failure: Failure, used: &mut usize, limit: usize) {
        let size = serde_json::to_vec(&failure)
            .expect("serializable issue")
            .len()
            + 1;

        if self.issues.len() < 4 && *used + size <= limit - 512 {
            *used += size;
            self.issues.push(failure);
        } else {
            self.suppressed_issues += 1;
        }
    }
}

impl Tools for Workspace {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let string = json!({"type":"string"});
        let positive = json!({"type":"integer","minimum":1});
        let hunk = json!({"type":"object","properties":{"start_line":positive,"old_text":string,"new_text":string},
            "required":["start_line","old_text","new_text"],"additionalProperties":false});

        let edit = |operation: &str, extra: Value, required: &[&str]| {
            let mut properties = json!({"operation":{"const":operation},"path":string});
            properties
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let mut fields = vec!["operation", "path"];
            fields.extend_from_slice(required);
            json!({"type":"object","properties":properties,"required":fields,"additionalProperties":false})
        };

        vec![
            ToolDescriptor { name:"read_file".into(), description:"Read whole UTF-8 lines from the current workspace with a full-file SHA-256 digest. Truncation is explicit; oversized lines may return no text.".into(),
                input_schema:json!({"type":"object","properties":{"path":string,"start_line":positive,"max_lines":positive,"max_bytes":positive},"required":["path"],"additionalProperties":false}) },
            ToolDescriptor { name:"search_text".into(), description:"Literal, case-sensitive search in workspace files or directories. One match per line; paths default to the root. No symlinks or runtime metadata. Inspect truncation and skipped-file issues.".into(),
                input_schema:json!({"type":"object","properties":{"paths":{"type":"array","items":string,"minItems":1,"maxItems":16},"query":{"type":"string","minLength":1},"max_results":positive},"required":["query"],"additionalProperties":false}) },
            ToolDescriptor { name:"apply_patch".into(), description:"Apply up to 16 exact file edits. Update/delete require read_file's expected_digest. Hunks replace whole lines at 1-based start_line with exact old_text/new_text (including line endings). Parents must exist. All edits are preflighted; per-file publication is not a multi-file transaction.".into(),
                input_schema:json!({"type":"object","properties":{"edits":{"type":"array","minItems":1,"maxItems":16,"items":{"oneOf":[
                    edit("create",json!({"content":string}),&["content"]),
                    edit("update",json!({"expected_digest":string,"hunks":{"type":"array","items":hunk,"minItems":1,"maxItems":128}}),&["expected_digest","hunks"]),
                    edit("delete",json!({"expected_digest":string}),&["expected_digest"])]}}},"required":["edits"],"additionalProperties":false}) },
        ]
    }

    fn validate(&self, name: &str, arguments: &Value) -> std::result::Result<(), ToolError> {
        decode(name, arguments).map(|_| ())
    }

    async fn execute(&mut self, call: &ValidatedCall, cancellation: &Cancellation) -> ToolOutcome {
        let request = match decode(&call.name, &call.arguments) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::Failed(e),
        };

        if cancellation.is_cancelled() {
            return ToolOutcome::Failed(ToolError::Interrupted);
        }

        let result = match request {
            Request::Read(request) => self
                .read_file(request)
                .map(|v| serde_json::to_value(v).unwrap()),
            Request::Search(request) => self
                .search_text(request, cancellation)
                .await
                .map(|v| serde_json::to_value(v).unwrap()),
            Request::Patch(request) => {
                let result = self.apply_patch(request, cancellation).await;
                if !result.complete {
                    let partial = result
                        .changes
                        .iter()
                        .any(|change| change.state != patch::State::NotApplied);
                    let error = ToolError::Workspace(serde_json::to_value(result).unwrap());
                    return if partial {
                        ToolOutcome::Unknown(error)
                    } else {
                        ToolOutcome::Failed(error)
                    };
                }
                Ok(serde_json::to_value(result).unwrap())
            }
        };

        let outcome = match result {
            Ok(value) => ToolOutcome::Success(value),
            Err(failure) => {
                ToolOutcome::Failed(ToolError::Workspace(serde_json::to_value(failure).unwrap()))
            }
        };

        if encode(&outcome, self.limits.output_bytes).is_err() {
            ToolOutcome::Failed(ToolError::OutputLimit)
        } else {
            outcome
        }
    }
}

enum Request {
    Read(ReadRequest),
    Search(SearchRequest),
    Patch(PatchRequest),
}

fn decode(name: &str, value: &Value) -> std::result::Result<Request, ToolError> {
    if encode(value, 256 * 1024).is_err() {
        return Err(ToolError::InvalidArguments);
    }

    let parsed = match name {
        "read_file" => serde_json::from_value(value.clone()).map(Request::Read),
        "search_text" => serde_json::from_value(value.clone()).map(Request::Search),
        "apply_patch" => serde_json::from_value(value.clone()).map(Request::Patch),
        _ => return Err(ToolError::UnknownTool),
    };

    let request = parsed.map_err(|_| ToolError::InvalidArguments)?;
    let valid = match &request {
        Request::Read(r) => r.start_line > 0 && r.max_lines > 0 && r.max_bytes > 0,
        Request::Search(r) => {
            !r.paths.is_empty()
                && r.paths.len() <= 16
                && !r.query.is_empty()
                && r.query.len() <= 4096
                && r.max_results > 0
        }
        Request::Patch(r) => {
            !r.edits.is_empty()
                && r.edits.len() <= 16
                && r.edits.iter().all(|edit| match edit {
                    patch::Edit::Update { hunks, .. } => {
                        !hunks.is_empty()
                            && hunks.len() <= 128
                            && hunks.iter().all(|h| h.start_line > 0)
                    }
                    _ => true,
                })
        }
    };

    if valid {
        Ok(request)
    } else {
        Err(ToolError::InvalidArguments)
    }
}
