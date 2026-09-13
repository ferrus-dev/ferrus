# Nano workspace tools

Issue #76 supplies a `Workspace` implementation of Nano's `Tools` port. It does not
register MCP handlers, start a model, or launch an Executor. The managed host must
validate the session authority and supply the assigned workspace root; standalone
hosts can supply an explicitly authorized root through the same constructor.

## Read and search

- `read_file`: `path`, optional 1-based `start_line`, `max_lines`, and `max_bytes`.
  Returns whole UTF-8 lines, their full-file SHA-256 digest, and explicit truncation.
  Line endings are returned verbatim. A line exceeding the byte allowance is omitted;
  `returned_lines = 0` with `truncated = true` means the first selected line did not fit.
  A range beyond EOF returns no text with `truncated = false`.
- `search_text`: literal, case-sensitive `query`, optional file/directory `paths`
  (default `["."]`), and `max_results`. Traversal is lexical within the admitted entry
  set. Each matching line yields one result with a 1-based line and UTF-8 byte column,
  a bounded snippet around the first match, and its source digest. Unsupported files,
  interrupted traversal, and resource caps make incompleteness explicit. This is not
  a regex or glob API. Queue keys use exact spelling on Unix and NT uppercase on
  Windows. Opened objects are deduplicated by filesystem identity before charging
  scan budgets, including on case-insensitive Unix volumes. Results retain the first
  visited spelling. Unsupported portable-path names make the search incomplete with
  bounded diagnostics; intentionally protected metadata remains hidden.

Every source record has `kind = "workspace"`, an opaque root identity, a local edit
`generation`, a relative path, and the digest of the bytes observed. These are current
workspace observations, not graph snapshot evidence. Generation advances for successful
native publications; external edits do not advance it. Digests remain the precondition
for writes, including after edits made outside Nano.

Default host limits are 1 MiB per file, 8 MiB of search content, 4096 visited/pending
entries, a 24 KiB serialized result, and a cooperative five-second operation deadline.
Read results admit at most 2000 lines; search admits at most 128 matching lines.
Callers may request smaller outputs. JSON escaping counts toward the output allowance.
Filesystem work is synchronous and bounded per file; cancellation/deadline checkpoints
occur between files. No detached filesystem task continues after the engine drops a call.

## Exact patches

`apply_patch` accepts an `edits` array of at most 16 files, encoded within 256 KiB.
Parents must already exist. Operations are:

```json
{"operation":"create","path":"src/new.rs","content":"pub fn new() {}\n"}
```

```json
{
  "operation":"update",
  "path":"src/lib.rs",
  "expected_digest":"<64 lowercase SHA-256 hex digits from read_file>",
  "hunks":[{"start_line":3,"old_text":"old line\n","new_text":"new line\n"}]
}
```

```json
{"operation":"delete","path":"src/old.rs","expected_digest":"<SHA-256>"}
```

Hunks are exact, whole-line replacements against the original file, ordered by their
1-based start line and without overlaps. At most 128 hunks are allowed per file.
An empty `old_text` inserts at a line boundary; an empty `new_text` removes the matched
lines. Include the exact LF/CRLF endings in both strings. Only the final line may omit
its terminator. There is no fuzzy matching, offset search, or implicit newline conversion.

All paths, bases, and hunks are checked before any publication. A stale base returns
`conflict`, its current digest when available, and a request to reread the file. No file
is written when preflight fails. Base digests are checked again before each publication,
including after staging. Existing permission modes and Windows owner/group/DACL are
retained; new files use owner-only permissions on Unix and inherited permissions on
Windows. Windows staging requests security-management rights only for updates that
copy owner/group/DACL. Other file contents and Git staging/history are untouched.

Batch target keys combine the opened parent directory's filesystem identity with the
filename key. Windows uses NT uppercase mapping. Unix conservatively rejects names
that collide after canonical Unicode decomposition and full default case folding,
including absent NFC/NFD targets and aliases such as long-s/ASCII s or sharp-s/ss.
This can reject a batch of distinct names on a case- or normalization-sensitive Unix
volume; submit those edits separately. Names
in different actual directories remain independent. Keys never change the names written.

Each file is staged in its destination directory and published individually; creation
never replaces an existing entry. A result lists `before_digest`, `intended_digest`,
`after_digest`, and one of `not_applied`, `applied`, or `durability_unconfirmed` per file.
`after_digest` describes an applied target (null for deletion); `intended_digest` also
records the target for edits that did not run. If a later file fails, the applied prefix
remains and later edits are not attempted. There is no multi-file atomicity or rollback.
A partial result is an unknown-effect tool outcome, so the engine stops for reconciliation.

The protocol is optimistic: digest checks cannot provide a filesystem compare-and-swap
against an unrelated process writing between the final check and rename. Concurrent
external writers should be quiesced by the host. A crash or engine interruption can lose
the returned report; recovery must compare the journaled intent with current content
before replaying any edit. Crash reconciliation is tracked in #84. Staged files may remain
after a crash and are excluded from native file access.

## Confinement and limitations

Paths are portable, relative, and at most 256 UTF-8 bytes. Traversal uses held directory
handles and component-relative no-follow opens on Unix and Windows. Symlinks, Windows
reparse points/junctions, hardlinks, special files, alternate streams, short-name aliases,
reserved device names, parent traversal, and nonportable spellings are rejected.

Any `.git` or `.ferrus` component is protected. Ferrus runtime database basenames and their
WAL/SHM/journal sidecars are also protected, as are owned `.nano-tmp-*` files. This tool set
never performs Git operations, lifecycle transitions, graph builds, or process execution.

Only UTF-8 text without NUL bytes is supported. Binary and oversized files produce typed
limitations. Updates preserve ordinary permission modes; preserving inode identity,
extended attributes, Windows audit SACLs, and concurrent metadata edits is not
part of this file-replacement contract. Root authority is host-owned; it is not a sandbox
against another process with the same OS account moving authorized directories.
