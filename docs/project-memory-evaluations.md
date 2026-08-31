# Project Memory Retrieval Evaluation

RG4 includes a deterministic offline evaluation for one high-value retrieval scenario: an agent
needs to understand why current code uses a bounded context service. The approved Outcome contains
the decision, while the repository graph contains the current implementation.

The corpus is stored in `tests/fixtures/project_memory_eval/cases.json`. The test builds a fresh
repository graph and project-memory sidecar from local fixture files, with no network, LLM, or
embedding dependency.

Run the evaluation with:

```sh
cargo test approved_history_reduces_raw_artifact_reading_without_displacing_source_evidence
```

## Gates

The case passes only when all of these conditions hold:

- memory search finds the approved Outcome instead of requiring raw run artifacts;
- combined context follows a resolved evidence-backed link to the expected current source path;
- current repository evidence is ranked before the linked historical item;
- repository snapshot and memory revision freshness are both reported independently;
- every returned cross-domain link is `resolved` for the exact selected revisions;
- private submission, review, and patch markers are absent from the response.

The fixture records a baseline of three raw artifact reads for reconstructing the decision without
project memory. The federated path uses the approved Outcome and current source evidence directly,
so the expected raw artifact read count is zero.

This is a regression gate, not a claim that every historical question is answerable. If no approved
Outcome or exact repository evidence exists, Ferrus must return an unresolved or stale link rather
than use text similarity to invent authority.
