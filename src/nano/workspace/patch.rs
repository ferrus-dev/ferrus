//! Exact line hunks with full-set preflight and explicitly non-atomic file-set commits.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hunk {
    pub start_line: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Edit {
    Create {
        path: String,
        content: String,
    },
    Update {
        path: String,
        expected_digest: String,
        hunks: Vec<Hunk>,
    },
    Delete {
        path: String,
        expected_digest: String,
    },
}

impl Edit {
    fn path(&self) -> &str {
        match self {
            Self::Create { path, .. } | Self::Update { path, .. } | Self::Delete { path, .. } => {
                path
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchRequest {
    pub edits: Vec<Edit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    NotApplied,
    Applied,
    DurabilityUnconfirmed,
}

#[derive(Debug, Serialize)]
pub(crate) struct Change {
    pub path: String,
    pub before_digest: Option<String>,
    pub intended_digest: Option<String>,
    pub after_digest: Option<String>,
    pub state: State,
}

#[derive(Debug, Serialize)]
pub(crate) struct PatchResult {
    pub source_kind: &'static str,
    pub workspace_id: String,
    pub generation: u64,
    pub complete: bool,
    pub changes: Vec<Change>,
    pub failure: Option<Failure>,
}

struct Plan {
    path: String,
    before: Option<String>,
    after: Option<String>,
}

impl Workspace {
    pub(crate) async fn apply_patch(
        &mut self,
        request: PatchRequest,
        cancellation: &Cancellation,
    ) -> PatchResult {
        let start = Instant::now();
        let plans = match self.prepare(request, start, cancellation) {
            Ok(plans) => plans,
            Err(failure) => return self.patch_result(Vec::new(), Some(failure)),
        };

        self.commit_plans(plans, start, cancellation).await
    }

    fn prepare(
        &self,
        request: PatchRequest,
        start: Instant,
        cancellation: &Cancellation,
    ) -> Result<Vec<Plan>> {
        if request.edits.is_empty()
            || request.edits.len() > 16
            || encode(&request, 256 * 1024).is_err()
        {
            return Err(Failure::new(Code::InvalidPatch, ""));
        }

        let mut names = BTreeSet::new();
        let mut plans = Vec::new();
        for edit in request.edits {
            if self.stopped(start, cancellation) {
                return Err(Failure::new(Code::Interrupted, edit.path()));
            }

            let path = path(edit.path())?;
            let parent = self.root.parent(&path).map_err(|e| Failure::io(&path, e))?;
            let key = parent.patch_key().map_err(|e| Failure::io(&path, e))?;
            if !names.insert(key) {
                return Err(Failure::new(Code::InvalidPatch, &path));
            }

            let (before, after) = match edit {
                Edit::Create { content, .. } => {
                    self.check_base(&parent, &path, None)?;
                    self.check_text(&path, &content)?;
                    (None, Some(content))
                }
                Edit::Update {
                    expected_digest,
                    hunks,
                    ..
                } => {
                    let current = self
                        .check_base(&parent, &path, Some(&expected_digest))?
                        .unwrap();
                    let after = apply_hunks(&path, &current.text, &hunks, self.limits.file_bytes)?;
                    self.check_text(&path, &after)?;
                    (Some(current.digest), Some(after))
                }
                Edit::Delete {
                    expected_digest, ..
                } => {
                    let current = self
                        .check_base(&parent, &path, Some(&expected_digest))?
                        .unwrap();
                    (Some(current.digest), None)
                }
            };

            plans.push(Plan {
                path,
                before,
                after,
            });
        }

        Ok(plans)
    }

    fn check_text(&self, path: &str, text: &str) -> Result<()> {
        if text.len() > self.limits.file_bytes {
            return Err(Failure::new(Code::FileTooLarge, path));
        }

        if text.contains('\0') {
            return Err(Failure::new(Code::Binary, path));
        }

        Ok(())
    }

    fn check_base(
        &self,
        parent: &fs::Parent,
        path: &str,
        expected: Option<&str>,
    ) -> Result<Option<Content>> {
        if expected.is_some_and(|value| {
            value.len() != 64
                || !value
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        }) {
            return Err(Failure::new(Code::InvalidPatch, path));
        }

        let current = match parent.open() {
            Ok(file) => Some(self.read_content(file, path)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Failure::io(path, error)),
        };

        if current.as_ref().map(|c| c.digest.as_str()) != expected {
            let mut failure = Failure::new(Code::Conflict, path);
            failure.current_digest = current.map(|c| c.digest);
            return Err(failure);
        }

        Ok(current)
    }

    fn patch_result(&self, changes: Vec<Change>, failure: Option<Failure>) -> PatchResult {
        PatchResult {
            source_kind: "workspace",
            workspace_id: self.id.clone(),
            generation: self.generation,
            complete: failure.is_none(),
            changes,
            failure,
        }
    }

    async fn commit_plans(
        &mut self,
        plans: Vec<Plan>,
        start: Instant,
        cancellation: &Cancellation,
    ) -> PatchResult {
        let changes = plans
            .iter()
            .map(|plan| Change {
                path: plan.path.clone(),
                before_digest: plan.before.clone(),
                intended_digest: plan.after.as_ref().map(|text| digest(text.as_bytes())),
                after_digest: None,
                state: State::NotApplied,
            })
            .collect();

        let mut result = self.patch_result(changes, None);
        // Reserve space for the error and wrapper before any effect is possible.
        if encode(&result, self.limits.output_bytes.saturating_sub(2048)).is_err() {
            return self.patch_result(Vec::new(), Some(Failure::new(Code::OutputLimit, "")));
        }

        for (index, plan) in plans.into_iter().enumerate() {
            if self.stopped(start, cancellation) {
                result.failure = Some(Failure::new(Code::Interrupted, &plan.path));
                break;
            }

            let applied = (|| -> Result<_> {
                let parent = self
                    .root
                    .parent(&plan.path)
                    .map_err(|e| Failure::io(&plan.path, e))?;

                let current = self.check_base(&parent, &plan.path, plan.before.as_deref())?;
                let staged = plan
                    .after
                    .as_ref()
                    .map(|text| {
                        parent
                            .stage(text.as_bytes(), current.as_ref().map(|c| &c.mode))
                            .map_err(|e| Failure::io(&plan.path, e))
                    })
                    .transpose()?;

                // Catch changes during staging. No fuzzy merge, rebasing, or rollback of other files.
                self.check_base(&parent, &plan.path, plan.before.as_deref())?;
                Ok(match staged {
                    Some(staged) => parent.publish(staged, plan.before.is_none()),
                    None => parent.delete(),
                })
            })();

            match applied {
                Ok(Ok(())) => {
                    result.changes[index].state = State::Applied;
                    result.changes[index].after_digest =
                        result.changes[index].intended_digest.clone();
                    self.generation = self.generation.saturating_add(1);
                }
                Ok(Err(error)) => {
                    if error.changed {
                        result.changes[index].state = State::DurabilityUnconfirmed;
                        result.changes[index].after_digest =
                            result.changes[index].intended_digest.clone();
                        self.generation = self.generation.saturating_add(1);
                    }
                    result.failure = Some(Failure::io(&plan.path, error.error));
                    break;
                }
                Err(failure) => {
                    result.failure = Some(failure);
                    break;
                }
            }

            tokio::task::yield_now().await;
        }

        result.generation = self.generation;
        result.complete = result.failure.is_none();
        result
    }
}

fn apply_hunks(path: &str, before: &str, hunks: &[Hunk], limit: usize) -> Result<String> {
    if hunks.is_empty() || hunks.len() > 128 {
        return Err(Failure::new(Code::InvalidPatch, path));
    }

    let mut offsets = vec![0];
    offsets.extend(before.match_indices('\n').map(|(index, _)| index + 1));

    let mut output = String::new();
    let mut consumed = 0;
    let mut previous_start = None;
    for hunk in hunks {
        let start = hunk
            .start_line
            .checked_sub(1)
            .and_then(|index| offsets.get(index))
            .copied()
            .ok_or_else(|| Failure::new(Code::InvalidPatch, path))?;

        let end = start
            .checked_add(hunk.old_text.len())
            .filter(|end| *end <= before.len())
            .ok_or_else(|| Failure::new(Code::InvalidPatch, path))?;

        if start < consumed
            || previous_start.is_some_and(|previous| start <= previous)
            || !before[start..].starts_with(&hunk.old_text)
            || (end < before.len() && !hunk.old_text.is_empty() && !hunk.old_text.ends_with('\n'))
            || (end < before.len() && !hunk.new_text.is_empty() && !hunk.new_text.ends_with('\n'))
        {
            return Err(Failure::new(Code::InvalidPatch, path));
        }

        if output
            .len()
            .saturating_add(start - consumed)
            .saturating_add(hunk.new_text.len())
            > limit
        {
            return Err(Failure::new(Code::FileTooLarge, path));
        }

        output.push_str(&before[consumed..start]);
        output.push_str(&hunk.new_text);

        consumed = end;
        previous_start = Some(start);
    }

    if output.len().saturating_add(before.len() - consumed) > limit {
        return Err(Failure::new(Code::FileTooLarge, path));
    }

    output.push_str(&before[consumed..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn late_filesystem_failure_reports_the_applied_prefix() {
        let directory = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(directory.path().join("parent")).unwrap();
        let mut workspace =
            Workspace::new(&directory.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let start = Instant::now();
        let cancel = Cancellation::default();
        let plans = workspace
            .prepare(
                PatchRequest {
                    edits: vec![
                        Edit::Create {
                            path: "first".into(),
                            content: "one".into(),
                        },
                        Edit::Create {
                            path: "parent/second".into(),
                            content: "two".into(),
                        },
                        Edit::Create {
                            path: "third".into(),
                            content: "three".into(),
                        },
                    ],
                },
                start,
                &cancel,
            )
            .unwrap();
        std::fs::remove_dir(directory.path().join("parent")).unwrap();
        let result = workspace.commit_plans(plans, start, &cancel).await;
        assert!(!result.complete);
        assert_eq!(
            result.changes.iter().map(|c| c.state).collect::<Vec<_>>(),
            [State::Applied, State::NotApplied, State::NotApplied]
        );
        assert_eq!(result.changes[0].after_digest, Some(digest(b"one")));
        assert_eq!(result.generation, 1);
        assert_eq!(result.failure.unwrap().code, Code::NotFound);
        assert_eq!(
            std::fs::read(directory.path().join("first")).unwrap(),
            b"one"
        );
        assert!(!directory.path().join("third").exists());
    }
    #[tokio::test]
    async fn a_concurrent_create_is_not_overwritten_and_temporary_files_are_removed() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut workspace =
            Workspace::new(&directory.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let start = Instant::now();
        let cancel = Cancellation::default();
        let plans = workspace
            .prepare(
                PatchRequest {
                    edits: vec![Edit::Create {
                        path: "file".into(),
                        content: "agent".into(),
                    }],
                },
                start,
                &cancel,
            )
            .unwrap();
        std::fs::write(directory.path().join("file"), "human").unwrap();
        let result = workspace.commit_plans(plans, start, &cancel).await;
        assert_eq!(result.failure.unwrap().code, Code::Conflict);
        assert!(result.changes.iter().all(|c| c.state == State::NotApplied));
        assert_eq!(
            std::fs::read(directory.path().join("file")).unwrap(),
            b"human"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
