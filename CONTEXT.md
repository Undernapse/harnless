# CONTEXT — harnless

The Rust reimplementation of the DeepSeek Harness (`dsh`), preserving its
plugability architecture. Glossary of the durable-session vocabulary (the
#66–#72 decisions); code comments own the details, this file owns the names.

## Session durability

- **Session** — one append-only event log, identified by a minted `u64` id
  (`(unix_micros << 20) | rand(20 bits)`), stored as `<id>.jsonl` in a plan's
  store dir. The log is the single source of truth for what the model sees.
- **Session id scope** (#68 §2) — ids are message *identity within one log*.
  A fresh session's ids start at 1; a resume/fork floors the allocator at the
  store's max + 1 so a continued session never reuses an id. Id values may
  recur across *different* session files, by design.
- **Position** — a record's index in its log, writer-assigned at append
  (`records.len()`), never caller-supplied. Contiguity is an invariant:
  the store refuses to fork from a non-contiguous source, and a seeded log
  refuses a non-contiguous seed.
- **Seed boundary** (`SeedBoundary`) — the log-only structural marker a
  resume/fork mount carries ahead of the region inherited from another
  session. It never produces a message and never joins derived history;
  multiple boundaries (resume-of-a-resume, fork-of-fork) stay unambiguous.
- **Fork** — copy-on-read of a source log into a new minted id: header line
  naming `forked_from`, the source's records verbatim, exactly one boundary.
  The source is byte-frozen; a refused fork (live source, corrupt positions)
  leaves no orphan target.
- **Store mount** (#69) — a plan's `store:` row (plugin `storage-jsonl`)
  names the state dir; mounting is pure path state (dir created lazily at
  first write). `${home}` expands through the substitution pass at compose
  time; the built-in `default` profile's rows carry
  `${home}/.harnless/sessions`.
- **Integrity rules** (#70 §3) — torn tail (unterminated last line): dropped
  by a tolerant load, repaired by truncation under the writer's first append.
  Mid-file corruption: `session-corrupt` naming the line. Lock:
  `flock(LOCK_EX)` on `<file>.lock`; a would-block is `session-locked`.
  Missing id: `session-not-found`. Store flag on a sessionless plan:
  `storage-not-mounted`. Exhausted mint retry: `session-mint-failed`.
- **CLI session surface** (#71) — `run`/`interactive` take
  `--resume <id>` / `--fork <id>` (mutually exclusive); a store-mounted run
  prints `session: <id>` on stderr before the turn (stdout stays "the answer
  text, then nothing else"); `sessions list` renders the store's table
  (mtime-desc, corrupt rows shown, absent dir = header-only, exit 0). Every
  session failure is boot-time, carrying one of the five stable codes.
- **Dump-equals-mount** — `--dump-config` prints the same fold the boot
  mounts; it is a pure offline operation (no mkdir, no lock, no open).
