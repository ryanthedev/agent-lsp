# Symbol queries

`alsp sym | refs | tests | cover | gaps` answer questions about a Rust repository without a diff: where is this defined, who uses it, which tests reach it, did the tests run it, would the tests catch a bug here. `alsp` with no arguments, and `alsp help agents`, print a one-screen cheat sheet for agents. Diff questions stay with `alsp pack` and `alsp review` (docs/review.md).

```
alsp sym   <target>            definitions, kind, reference count (lists every match)
alsp refs  <target>            references grouped by enclosing symbol; tests marked; entry points reached
alsp tests <target> [--run]    tests that reach it; --run: do the package's tests execute it
alsp cover <target>            line and branch coverage of its span
alsp gaps  <target>            mutants in its span and the predicted chance the tests catch each
```

Flags on every query: `--repo <path>` (any directory inside the work tree; default: the git repository of the current directory), `--json`, `--all`, `--no-build`, `--root <dir>` (repeatable), `--cache-dir <dir>`. `gaps` also takes `--gap-budget <n>` (most mutants sent to the model, default 100) and `--gap-threshold <p>` (a predicted gap is reported below this chance of being caught, default 0.50).

## Targets

A target is a symbol name or `file:line`.

- **Name.** Exact (`net::handle_request`, the qualified name the answer prints) or a suffix that starts at a `::` boundary (`handle_request`, `net::handle_request`). An exact match wins over suffix matches. A name that matches several definitions is never guessed: `sym` lists them all (exit 0); every other command prints the candidates and exits 4. Repeat the command with a longer name or with `file:line`.
- **`file:line`.** The file is repo-relative (`apps/foo/rust/src/net.rs`), absolute, relative to the current directory, or a unique path suffix (`net.rs`). The target is the innermost function, type, const or static whose span holds the line. A line in no such item (a comment between items, imports) targets the whole file: its definitions together, and for `refs` and `tests` only the uses from other files. A file with no indexed items still answers (an empty list and a note).
- **Module.** A module name targets the module's span and the definitions inside it.
- No match: exit 4, with the nearest names (a similar last segment) or files.

## Provenance

Every fact says how it was obtained.

| Provenance | Meaning |
|---|---|
| `resolved` | From the rust-analyzer index (type-resolved), or from syntax that leaves no doubt (a `#[test]` attribute, `fn main` in a binary root). |
| `observed` | Measured by running the package's tests (`cargo llvm-cov`). |
| `predicted` | Estimated by a model (`gaps`). |
| `heuristic` | Inferred: a `pub` function stands in for "reachable from outside the crate". |

In text, each section line ends with its provenance in brackets (`refs(3 in 2 symbols) [resolved]`); a section that mixes provenances names both (`[resolved+heuristic]`, `[predicted+observed]`); a single heuristic item carries a trailing `~`, as in `alsp pack`.

## Text format

One line per fact, no decoration; lists are capped at 25 items with `+N more (--all lists everything)`. Control characters in names and paths are shown as escapes (`\n`), so a hostile file name cannot forge a line.

```
refs net::handle_request index=fresh @1a2b3c4d
target fn apps/foo/rust/src/net.rs:120-150 [resolved]
! the working tree has uncommitted changes to 1 Rust/Cargo file(s) (src/net.rs); the index describes the working tree, not 1a2b3c4d
refs(7 in 3 symbols) [resolved]
  net::dispatch apps/foo/rust/src/net.rs:200,215
  net::tests::t_x apps/foo/rust/src/net.rs:300 [test]
entry(2) [resolved+heuristic]
  test net::tests::t_x apps/foo/rust/src/net.rs:298
  pub net::start apps/foo/rust/src/net.rs:40~
```

- First line: command, target, index state (`fresh`, `stale` or `missing`, `+rebuilt` when the indexer ran for this query), the checked-out commit.
- `target KIND FILE:SPAN [resolved]`; a whole file is `1-end`.
- `! ...` lines are notes: caveats the reader needs to weigh the facts. A dirty working tree is one: the index always describes the working tree (it is rebuilt when its sources changed), and the note names the uncommitted Rust and Cargo files so a stale mental model of `HEAD` is not mistaken for the index. Others: a root that failed to index, an entry-point search that hit its limit, references that cannot be attributed (same symbol id in several crate roots), why a measurement is missing.
- `sym`: `matches(N) [resolved]`, then `KIND NAME FILE:LINE refs=N` (the line of the name).
- `refs`: `refs(SITES in GROUPS symbols)`, then `SYMBOL FILE:LINES` with `[test]` when the enclosing symbol is a `#[test]` function or lives under `tests/`; then `entry(N)`, then `KIND NAME FILE:LINE` with kind `test`, `main` or `pub`. References that sit in no indexed symbol (macro-generated code the index gives no span for) are grouped under `<outside any symbol>`; imports appear under their module.
- `tests`: `tests(N) [resolved]`, then `NAME FILE:LINE direct|indirect` (the test uses the target itself, or reaches it through other functions), nearest first. With `--run`: `executed: yes|no|unknown (HIT/TOTAL lines ran) [observed]`.
- `cover`: `lines HIT/TOTAL covered[, branches HIT/TOTAL] [observed]` and `uncovered: LINES [observed]`.
- `gaps`: `gaps(N) [...]`, then `FILE:LINE MUTATION p=0.20 [predicted]` or `FILE:LINE MUTATION uncovered [observed]`; predictions first, lowest chance first.
- `! unavailable: ...` as the last line when a needed measurement could not be made.

## JSON

`--json` prints one object on one line. The same caps apply (`--all` lifts them). Keys that do not apply to a command are absent, not null. `schema` is bumped when a key is renamed or removed; adding keys does not bump it.

| Key | Meaning |
|---|---|
| `schema` | Schema version, `1`. |
| `command` | `sym`, `refs`, `tests`, `cover` or `gaps`. |
| `target` | Absent for `sym`. `kind` (`fn`, `const`, `static`, `struct`, `enum`, `trait`, `type`, `macro`, `mod`, or `module` / `file` for a region), `name`, `file`, `span` (`start`, `end`; a whole file ends at 4294967295). |
| `index` | `status` (`fresh`, `stale`, `missing`), `rebuilt` (the indexer ran for this query), `head` (checked-out commit), `dirty` (uncommitted Rust and Cargo files). |
| `notes` | Strings: the `!` lines. |
| `matches` | `sym` only. A list section (below) of `name`, `kind`, `file`, `line` (of the name), `span`, `references`. |
| `reference_count` | `refs` only. Reference sites before grouping. |
| `refs` | `refs` only. A list section of groups: `symbol`, `kind`, `file`, `test`, `lines`. |
| `entry_points` | `refs` only. A list section of `name`, `kind` (`test`, `main`, `pub`), `file`, `line`, `provenance` of each. |
| `tests` | `tests` only. A list section of `name`, `file`, `line`, `direct`. |
| `executed` | `tests --run` only. `provenance`, `status` (`yes`, `no`, `unknown`), `lines_hit`, `lines_total`. |
| `coverage` | `cover` only; absent when nothing was measured or the span has no instrumented line. `provenance`, `lines_total`, `lines_hit`, `branches_total`, `branches_hit` (null when the toolchain cannot count branches), `uncovered_lines`. |
| `gaps` | `gaps` only. A list section of `file`, `line`, `mutation`, `p_tests_catch` (0 for lines no test runs), `provenance` of each (`observed` or `predicted`). |
| `unavailable` | Why a needed measurement is missing; the exit code is then 5. |

A **list section** is `{"provenance", "total", "omitted", "items"}`: `provenance` of the whole list, `total` everything found, `omitted` what the cap left out, `items` the facts shown.

A target error (exit 4) is printed on stdout in the same mode, and its `hint`, which says what to do next, is also on stderr. JSON keys:

| Key | Meaning |
|---|---|
| `schema` | `1`. |
| `error` | `no_match` or `ambiguous`. |
| `query` | The target as typed. |
| `nearest` | `no_match`: the closest names or files. |
| `candidates` | `ambiguous`: `name`, `kind`, `file`, `line` of each; a file candidate has kind `file`. |
| `omitted` | `ambiguous`: candidates the cap left out. |
| `hint` | What to do next. |

Text: `no match for ...` followed by `nearest:`, or `... is ambiguous: N candidates` followed by one line per candidate.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Answered. |
| 2 | Usage error, not a git work tree, no `Cargo.toml`, or another failure; stderr says what to do. |
| 3 | No index could be served: `--no-build` with a stale or missing cache, or every Cargo root failed to index. `alsp pack` and `alsp review` use it too: they still print what they have, an `Index:` line (status, each failed root and why, the indexer's notes) and a note that an empty symbol list means unknown, not unchanged. A partial index (some roots failed) exits 0 and names the failed roots in the `Index:` line. |
| 4 | The target matches nothing, or `refs`, `tests`, `cover` or `gaps` got an ambiguous name. Candidates or nearest names are on stdout. |
| 5 | A measurement the question needs could not be made (tests failed, `cargo-llvm-cov` missing, mutants not listable, sdd `decide` unavailable). The answer is still printed, with the cause in its notes and `unavailable`. |

## The index and its cost

The index is the same cached rust-analyzer SCIP index `alsp pack` keeps for the checked-out tree (shared, one build serves both). The first query of a large repository builds it, about 30 s for a large workspace; later queries reuse it until a Rust or Cargo file changes. `--no-build` never runs rust-analyzer to index (see Trust and cost for what that does and does not make safe). Facts come from the working tree, uncommitted edits included.

## Packages outside the root workspace

When the repository root has a `Cargo.toml`, the index covers that workspace. A package with its own `[workspace]` (say `apps/foo/rust`) is not a member and is not indexed unless you ask: `--root apps/foo/rust` (relative to the repository, repeatable) indexes it as a root of its own, cached separately. A target in such a package answers with a note naming the flag. Coverage and mutant runs find the owning package through `cargo metadata`, so they work for it once it is indexed. This is a query-side option: `alsp pack` still indexes the root workspace only.

## Trust and cost

**Building the index runs the repository's code.** rust-analyzer, which `alsp` runs to index, builds the workspace's build scripts and proc macros and reads the repository's cargo configuration (`.cargo/config.toml`, for example a `rustc-wrapper`). Every command without `--no-build` therefore executes code from the repository, and so do `alsp pack` and `alsp review` (`alsp review` has no `--no-build`). Checked 2026-11-15 with rust-analyzer 1.99.0-nightly: `rust-analyzer scip` ignores `cargo.buildScripts.enable` and `procMacro.enable` in its `--config-path` file, and even with the build-script command overridden to do nothing the repository's `rustc-wrapper` still ran, so `alsp` has no "safe indexing" mode and does not pretend to have one.

`--no-build` stops that indexing and nothing else: it never runs rust-analyzer to index, and serves only a cached index (built earlier, for example on code you trust), exiting 3 when there is none. Its one remaining spawn is `rust-analyzer --version` (part of the cache key), started from the cache directory, outside the repository, so a repository's `rust-toolchain.toml` does not choose the binary. Only `sym`, `refs`, `tests` without `--run`, and `alsp pack` without `--with` are therefore safe on a repository you do not trust, and only with `--no-build`.  With no cache, read the code as text or run `alsp` inside a sandbox or throwaway VM.

**The repository's own git config.** A repository handed over as an archive or a directory carries its own `.git/config`, which git obeys and which can name programs to run (`core.fsmonitor`, `diff.external`, `diff.<driver>.textconv`, `filter.<name>.clean|smudge|process`, `core.hooksPath`, `core.pager`, `core.sshCommand`, a gpg program, a remote's `uploadpack` or an `ext::` URL). Every `git` that `alsp` starts goes through one helper that forces those settings off through git's `GIT_CONFIG_COUNT` environment (which outranks every config file, included files too), empties the clean, smudge and process commands of every filter the repository's own config defines (including ones pulled in by `include.path`), passes `--no-ext-diff --no-textconv` to every diff and show, refuses every network and program transport (`protocol.*.allow=never`, `GIT_PROTOCOL_FROM_USER=0`) and never fetches a missing object (`GIT_NO_LAZY_FETCH=1`), ignores the system config, and never pages or prompts. It does not enter submodules: status and diff of the work tree run with `--ignore-submodules=all`, and the review snapshot leaves each submodule at the commit `HEAD` records, because git would otherwise read the submodule's own config, whose filters `alsp` cannot list ahead of time. If git cannot list the repository's filters (an unreadable config, say), `alsp` refuses to run git at all rather than run it without the overrides. git runs with the repository's own `.git/config` neutralized this way, so `sym`, `refs`, `pack` without `--with` and the ad-hoc `review` snapshot start none of those programs (tests plant each one, in the repository, in a submodule and in a partial clone's remote, and check, and check that plain git does start them). It needs git 2.31 or newer; `alsp` refuses an older git. Before 2.44, git ignores `GIT_NO_LAZY_FETCH`, so a partial clone relies on the refused transports alone, which the tests show is enough on the git they ran against.

What this does not cover: a program named in your own **global** git config (`~/.gitconfig`, `$XDG_CONFIG_HOME/git/config`) still runs when the repository's `.gitattributes` selects it (git-lfs configured there is the usual case), because the global config is yours. The **system** config (`/etc/gitconfig` and the like) is ignored altogether, so a filter configured only there, git-lfs installed system-wide for example, does not run: such files are read as plain files, and an LFS file shows as its pointer text. In a partial clone, a blob that is not on disk is not fetched, so a command that needs it fails or reports the file absent. Submodule contents are not examined. Anything git itself or the operating system does with a hostile directory (a malicious `.git` pointing `core.worktree` elsewhere, for example) is outside what `alsp` hardens.

`cover`, `gaps` and `tests --run` go further and also build and run the repository's tests under `cargo llvm-cov` (build scripts, proc macros, tests), as do `alsp pack --with` and `alsp review --full` or `--with coverage|gaps`, and `--no-build` does not prevent it (the tests are built even when the index came from the cache): use them only on code you trust. Results are cached per package by content hash (the package's workspace sources, `Cargo.toml` files and `Cargo.lock`, plus the `cargo-llvm-cov` version; nothing about the environment, so `CARGO_TARGET_DIR`, `RUSTFLAGS` and the like do not change it), so a repeat query after no edits costs nothing, and any edit to a source file in the workspace starts a new test run. Before it starts a test run, or when cargo reports it is waiting for another cargo's file lock, `alsp` prints one line to stderr saying so (`alsp: coverage <package>: no cached report ...` or `... cargo is waiting for a file lock ...`) and that a cold instrumented build takes minutes, so a caller that must stay inside a time limit can run the command in the background or under `timeout`. `gaps` also lists mutants with `cargo mutants --list` (nothing is executed) and sends the enclosing function and the tests that reach it to the decision model, the same backend `alsp pack --with gaps` uses (Gemini through sdd's `decide` command, found via `ALSP_SDD_ROOT` and run with `ALSP_BUN`; alsp holds no key; spend is printed on stderr). Mutants on lines no test runs are reported without a model call.

## Limits

- Rust only. No daemon, no editing.
- `tests --run` says whether the package's tests ran the target, not which test did: coverage is measured for the whole package at once. Tests listed by `tests` are the ones that reach the target through resolved references; calls through trait objects, function pointers or macros are not followed, so a test can run code it is not listed for (the answer notes the case).
- `entry` kind `pub` is heuristic: `pub` does not mean reachable from outside the crate.
- References from macro-expanded or generated code have no source span in the index and may be missing or grouped under `<outside any symbol>`.
- Test files (`tests/`, `benches/`, `examples/`) are not measured: `cargo llvm-cov` reports nothing for them.
- A symbol id shared by items in several crate roots (`tests/a.rs`, `tests/b.rs`, bins) can only be attributed within its own file; the answer says so.
