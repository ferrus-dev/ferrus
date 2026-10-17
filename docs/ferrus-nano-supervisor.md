# Nano Supervisor authoring

Nano supports taskless Supervisor conversations in HQ with `nano-openai`. Register the
loaded model as usual:

```sh
ferrus register --supervisor nano --supervisor-model YOUR_LOADED_MODEL_ID
ferrus
```

Registration provisions the same private `~/.ferrus/nano.toml` as the Executor, using
Ferrus home-directory resolution on every platform. It never replaces an existing file.
After provisioning, omit `--supervisor-model` to clear the HQ override and use the file's
model. `FERRUS_NANO_CONFIG` selects an explicit absolute private provider file.
`ferrus nano --version` needs no provider configuration.

| HQ entry point | Native profile | Authoring tool |
| --- | --- | --- |
| `/supervisor`, `/plan` | `plan` | None; discuss and inspect only |
| `/task`, `/run` task-definition stage | `task` | `enqueue_task` |
| `/spec` | `spec` | `create_spec` |
| `/archive-spec` | `archive` | `archive_spec` for the launch-selected spec |

These profiles use the existing task, specification, and archive operations directly,
without loopback MCP. Native file/search, scoped instructions, repository graph, and
project-memory reads remain available under the existing source and freshness policies.
`definition_context` provides the selected spec, linked task metadata, and authoring templates.
Tools are selected by the host profile, independently of model requests or supporting documents.
There is no task lease, Executor submission, review, or consultation authority.

Before each write, the terminal displays the complete proposed arguments in a scrollable
confirmation view. Type its fresh `approve <code>` phrase and press Enter to authorize
that exact operation. `reject` or Esc declines it; Ctrl-C ends the session. Conversation
text, queued steering, and model claims cannot approve a write. Task origins must identify
an incomplete milestone without a live task; specifications require unique stable milestone
IDs. Outcome content is accepted only by the archive profile through the existing archive
workflow. Shell commands, arbitrary file writes, and external MCP peers are not exposed in
these Supervisor profiles, so they cannot bypass confirmation.

Confirmed operations are journaled before execution, share the canonical approval lock,
and finish before shutdown returns. Task/spec/archive sessions return to HQ after saving
their requested count; planning remains open until cancelled. SQLite run identity and role
are checked independently of conversation content. Archive retains normal provenance and
best-effort memory refresh; a failed refresh does not undo a successful archive.

HQ also launches [task-scoped Reviewer and Consultant profiles](ferrus-nano-review.md).
Those profiles use independent run bindings and authority; taskless conversations do not
gain review or consultation tools. Executor and standalone MCP support are unchanged.
Live-model validation remains opt-in; automated authoring tests use a local mock provider
and exercise terminal confirmation without contacting an LLM.
