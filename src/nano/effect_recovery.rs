//! Read-only command-spool reconciliation shared by both hosts.

use super::{
    session::{Record, SessionEvent},
    tools::ToolOutcome,
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
};

pub(super) fn recorded_commands(records: &[Record]) -> Option<BTreeSet<String>> {
    let calls: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolIntent { call_id, call, .. } if call.name == "exec" => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    let mut processes = BTreeSet::new();
    for record in records {
        if let SessionEvent::ToolResult {
            call_id,
            outcome: ToolOutcome::Success(value),
        } = &record.event
            && calls.contains(call_id.as_str())
        {
            processes.insert(value.get("process_id")?.as_str()?.to_owned());
        }
    }
    Some(processes)
}

pub(super) fn unresolved_commands(
    directory: &Path,
    session_id: &str,
    expected: &BTreeSet<String>,
) -> Result<bool> {
    let commands = directory.join("commands");
    if !commands.try_exists()? {
        return Ok(!expected.is_empty());
    }
    super::private::check(&commands, true)?;
    let mut count = 0;
    let mut stdout = BTreeSet::new();
    let mut stderr = BTreeSet::new();
    let mut states = BTreeMap::new();
    for entry in fs::read_dir(&commands)? {
        let path = entry?.path();
        count += 1;
        ensure!(
            count <= super::commands::MAX_PROCESSES * 3,
            "Command spool exceeds file bound"
        );
        super::private::check(&path, false)?;
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(true);
        };
        if let Some(id) = name.strip_suffix("-stdout") {
            stdout.insert(id.to_owned());
            continue;
        }
        if let Some(id) = name.strip_suffix("-stderr") {
            stderr.insert(id.to_owned());
            continue;
        }
        let Some(id) = name.strip_suffix(".json") else {
            return Ok(true);
        };
        let mut bytes = Vec::new();
        super::private::file(&path, false)?
            .take(super::commands::STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > super::commands::STATE_BYTES {
            return Ok(true);
        }
        let Ok(state) = serde_json::from_slice::<super::commands::Snapshot>(&bytes) else {
            return Ok(true);
        };
        if id != state.process_id
            || !state.process_id.starts_with(&format!("{session_id}-p"))
            || state.backend != "trusted_local"
            || state.stdout.handle != format!("{id}-stdout")
            || state.stderr.handle != format!("{id}-stderr")
            || !matches!(
                state.completion,
                super::commands::Completion::Exited { .. }
                    | super::commands::Completion::Cancelled
                    | super::commands::Completion::TimedOut
            )
            || !state.output_complete
        {
            return Ok(true);
        }
        states.insert(id.to_owned(), state);
    }
    let ids: BTreeSet<_> = states.keys().cloned().collect();
    if ids != stdout || ids != stderr || !expected.is_subset(&ids) {
        return Ok(true);
    }
    for (id, state) in states {
        let out = super::private::file(&commands.join(format!("{id}-stdout")), false)?;
        let err = super::private::file(&commands.join(format!("{id}-stderr")), false)?;
        if out.metadata()?.len() != state.stdout.bytes
            || err.metadata()?.len() != state.stderr.bytes
        {
            return Ok(true);
        }
    }
    Ok(false)
}
