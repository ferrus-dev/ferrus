# Nano context budget and compaction

Status: implemented for #82. This is a model-request projection, not a rewrite of the session
journal or Ferrus task state. The system policy and first task/constraint message remain intact.

Before each inference, Nano measures the provider's serialized request, including tool schemas
and wire framing, then reserves output tokens and a context safety margin. The OpenAI-compatible
adapter measures its actual request body. Estimates and provider-reported usage remain separate;
failed or interrupted requests conservatively charge their reservations. The host records a
`context_composed` event with capacity, request size, output reservation, evictions, summary use,
and the unchanged prefix shared with the previous request.

The private provider setting `session_tokens` controls the cumulative work-phase allowance
(default 1,000,000), independently of the per-request `context_tokens` window. Admission may
stop below that total when the remaining allowance cannot pay for another conservative request
reservation or for both summary inference and its following normal request.

`context_tokens` is optional. When omitted, Nano does not assume a model window;
session-token and context-byte admission remain bounded. The server owns its loaded
context size and can still reject a request that exceeds it. Set an explicit window
to enable local admission and compaction against that model capacity.

If the request does not fit, Nano first retains the journal and replaces large older read-only
tool results with bounded handles. Each handle names the original tool and arguments and includes
an output digest. The model can reissue that read-only tool through normal validation and host
authorization to obtain current evidence. A handle is never itself current source evidence.
Assistant calls, provider continuation data, and call/result ordering remain unchanged for every
retained group.

If deterministic eviction is insufficient, Nano summarizes complete older groups through the
same provider with no tools advertised. The attempt consumes the session's model-turn, token, and
elapsed budgets. Its start, usage, result, and the following projection are journaled before the
next inference. The summary is labeled untrusted historical context and includes bounded
reissue handles; it cannot grant tool authority or update SQLite. The active task and recent
groups remain outside the summary. A failed or canceled summary ends the attempt without
changing the original journal; recovery can verify the charged prefix. A session never retries
the same failed summary in a loop.

At most 64 old output handles are projected at once, 16 reissue handles are included in one
summary, and summary text is capped at 4 KiB. If the provider cannot fit the summary request or
even the minimal current context, Nano returns a typed context limit instead of silently losing
constraints. Live continuation of an ended session remains #84. Comparative quality, token, and
latency measurements remain #85.
