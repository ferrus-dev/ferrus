//! Read-only archive evidence, scoped to terminal tasks of the selected spec.
use super::*;

pub(super) const NAME: &str = "read_archive_artifact";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    task_id: String,
    artifact: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "page_bytes")]
    max_bytes: usize,
}

fn page_bytes() -> usize {
    2048
}

pub(super) fn parse(args: &Value) -> Result<Request> {
    let request: Request = serde_json::from_value(args.clone())?;
    ensure!(
        super::super::journal::valid_id(&request.task_id)
            && matches!(
                request.artifact.as_str(),
                "task" | "submission" | "review" | "patch" | "integration_error"
            )
            && (1..=4096).contains(&request.max_bytes),
        "Invalid archive artifact request"
    );
    Ok(request)
}

pub(super) fn descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: NAME.into(),
        description: "Read task intent, submission/check evidence, review, patch or integration error for a terminal task linked to the launch-selected archive spec. Use this tool for .ferrus artifacts; read_file deliberately cannot read runtime files. Missing files return available=false: continue with other linked evidence without retrying or inventing missing contents. Pages are bounded to 4096 bytes; artifacts to 1 MiB. No arbitrary paths.".into(),
        input_schema: json!({"type":"object","properties":{
            "task_id":{"type":"string"},
            "artifact":{"type":"string","enum":["task","submission","review","patch","integration_error"]},
            "offset":{"type":"integer","minimum":0},
            "max_bytes":{"type":"integer","minimum":1,"maximum":4096}
        },"required":["task_id","artifact"],"additionalProperties":false}),
    }
}

pub(super) async fn read(scope: &Scope, args: &Value) -> Result<Value> {
    scope.check().await?;
    ensure!(
        scope.mode == DefinitionMode::Archive,
        "Archive evidence is unavailable in this profile"
    );
    let request = parse(args)?;
    let spec = scope
        .archive_path
        .as_deref()
        .context("No selected archive specification")?;
    let task = project::list_tasks_for_spec(spec)
        .await?
        .into_iter()
        .find(|task| task.id == request.task_id)
        .context("Task is not linked to the selected archive specification")?;
    ensure!(
        task.status.parse::<project::TaskStatus>()?.is_terminal(),
        "Archive evidence requires a terminal task"
    );
    let path = match request.artifact.as_str() {
        "task" => format!(".ferrus/tasks/{}.md", request.task_id),
        name => {
            let file = match name {
                "submission" => "SUBMISSION.md",
                "review" => "REVIEW.md",
                "patch" => "PATCH.diff",
                "integration_error" => "INTEGRATION_ERROR.md",
                _ => unreachable!("validated artifact"),
            };
            format!(
                "{}/{file}",
                project::run_dir_for_task_display(&request.task_id)
            )
        }
    };
    let Some((text, digest)) = super::super::workspace::instruction_file(
        scope.binding.project_root(),
        &path,
        1024 * 1024,
    )?
    else {
        // Historical terminal rows can outlive their checkout artifacts. This
        // is a successful availability observation, not an execution failure.
        return Ok(
            json!({"task_id":request.task_id,"artifact":request.artifact,
            "available":false,"reason":"artifact_missing","task_status":task.status,
            "failure_reason":task.failure_reason,
            "message":"The file is absent. Continue with other linked task evidence; do not retry or reconstruct missing contents."}),
        );
    };
    ensure!(
        request.offset <= text.len() && text.is_char_boundary(request.offset),
        "Invalid UTF-8 artifact offset"
    );
    let mut end = request
        .offset
        .saturating_add(request.max_bytes)
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }

    ensure!(
        end > request.offset || end == text.len(),
        "Increase max_bytes to fit the next character"
    );
    Ok(
        json!({"task_id":request.task_id,"artifact":request.artifact,"available":true,"digest":digest,
        "text":&text[request.offset..end],"next_offset":end,"truncated":end<text.len()}),
    )
}
