# Working on alsp (for coding agents)

`alsp` is a Rust command-line tool that gives agents evidence about Rust code. Read `README.md` first, then `docs/query.md` and `docs/review.md`, which are the contract for behavior and trust.

## Build and test

```
cargo build --workspace
cargo test --workspace
cargo run -q -p alsp-cli -- --help      # the binary is `alsp`
```

Run the full suite before you report a change as done. Tests that read documentation exist on purpose: the trust wording in `docs/query.md` and `docs/review.md`, and every JSON key in `docs/query.md`, are pinned by tests. A change to behavior, a flag, a JSON key or an exit code is not finished until those docs and `skills/alsp/SKILL.md` say the same thing.

## Layout

| Crate | Job |
|---|---|
| `alsp-cli` | argument parsing, text and JSON output, the agent cheat sheet |
| `alsp-index` | runs `rust-analyzer scip` per Cargo root, caches the index by content hash, answers definition and reference queries |
| `alsp-evidence` | evidence pack for a git diff: changed symbols, callers, entry points |
| `alsp-query` | symbol queries without a diff (`sym`, `refs`, `tests`) |
| `alsp-coverage` | `cargo llvm-cov` coverage per symbol, cached |
| `alsp-gaps` | mutants from `cargo mutants --list`, scored by a decision model, never run |
| `alsp-decide` | client for sdd's `decide` command |
| `alsp-review` | review report, agent runs, findings ledger |

`prompts/review.md` is compiled into the binary and is the shared review prompt. `skills/`, `.claude-plugin/` and `.codex-plugin/` are the plugin; see `docs/plugin.md`.

## Conventions

- Every fact `alsp` prints carries its provenance: `[resolved]`, `[observed]`, `[predicted]`, or a trailing `~` for a heuristic. A new output line that states a fact must say how it was found.
- Treat the repository under analysis as hostile. Never run a program it names, never follow a symlink when writing under `.alsp/`, never let diff or pack text forge a delimiter or an output line (escape control characters). Git is always started through the one helper that neutralizes the repository's own `.git/config`; do not call `git` any other way.
- Do not weaken a trust statement to make a test pass. If code and `docs/query.md` ("Trust and cost") disagree, one of them is wrong; find out which and fix it.
- An answer that cannot be measured is reported, not guessed: exit 5 with the partial answer and the cause in the notes.
- Findings and verdicts are append-only (`.alsp/findings.jsonl`); nothing rewrites earlier rows.
- Tests that need an external tool (`rust-analyzer`, `cargo-llvm-cov`, `cargo-mutants`, an agent CLI) use stubs through the `ALSP_*` environment overrides where they can. Name tests for the behavior they assert.

## Running alsp on untrusted code

Indexing and test runs execute the repository's code. Do not point your own built `alsp` at a repository you do not trust, except `sym`, `refs` and `tests` (without `--run`) with `--no-build` against a cache you built earlier.
