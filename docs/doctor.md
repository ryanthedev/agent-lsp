# alsp doctor

`alsp doctor [--repo <dir>] [--json]` checks everything alsp needs and prints, for each item, `ready` or `missing` with the command that fixes it. It changes nothing, builds no index, and spends nothing (it never calls Gemini).

```
$ alsp doctor
ready     alsp (required): 0.2.0
ready     git (required): git version 2.50.1 (/usr/bin/git)
ready     repository (optional): /work/project
ready     rust-analyzer (required): rust-analyzer 1.95.0 (/Users/me/.cargo/bin/rust-analyzer; PATH)
missing   cargo-llvm-cov (optional): `cargo-llvm-cov` is not on PATH; needed by `cover` and `tests --run`
          fix: cargo install cargo-llvm-cov --locked
...
alsp doctor: ready
```

The last line is `alsp doctor: ready` or `alsp doctor: N problems`.

## Checks

| Check | Required | Needed for | Fix when missing |
|---|---|---|---|
| alsp | yes | | |
| git >= 2.31 | yes | every command (config of the analysed repository is neutralized with it) | install git 2.31 or newer |
| rust-analyzer | yes | `sym`, `refs`, `tests`, `pack`, `review` | the command from the lookup: `rustup component add rust-analyzer --toolchain <pinned>`, or an install hint |
| cargo-llvm-cov | no | `cover`, `tests --run`, `review --with coverage` | `cargo install cargo-llvm-cov --locked` |
| cargo-mutants | no | `gaps` | `cargo install cargo-mutants --locked` |
| sdd decide + bun | no | `gaps` (the decision model) | clone sdd, set `ALSP_SDD_ROOT` / `ALSP_BUN` |
| claude / codex / agy CLI | no | `review` with that agent | install the CLI |
| plugin per agent | no | agents finding the skill | the plugin commands in `docs/plugin.md` |

rust-analyzer is found by the same lookup the index commands use (`ALSP_RUST_ANALYZER`, then `PATH`, then another rustup toolchain's copy, run by its real path); a fallback is named in the detail. No agent CLI is needed: alsp is a plain command-line program, so a machine with none still gets `ready`. An absent agent CLI is reported as a missing optional tool, and its plugin check as `skipped`.

The repository root is found by looking for a `.git` entry up the directory tree; git is not run to find it. Outside a git work tree the `repository` check is `skipped` and says so; rust-analyzer is then looked for without a pinned toolchain.

## Output and exit code

Exit 0 when every required check is `ready`, 1 otherwise (optional problems never change it).

`--json` prints an array with one object per check: `name`, `status` (`ready`, `missing`, `timed-out`, `skipped`), `required` (bool), `fix` (string or null) and `detail` (text). It has no verdict line; the exit code carries it.

## Trust and limits

doctor runs next to a repository it does not trust, so it only asks tools for `--version` (an agent CLI `plugin list`, which it reads for `alsp`). It never builds, never runs a build script, and never runs a file that lies inside the repository (including a `PATH` entry that is relative or that resolves into it). Probes start from a neutral directory, not the repository. sdd and bun are only looked for, not started.

Each probe is stopped after 10 seconds, and an agent CLI's `plugin list` after 45 (codex takes about 30). `ALSP_DOCTOR_TIMEOUT_SECS` (1 to 120), when set, replaces both. A probe that runs out of time is stopped together with anything it started, and shown as `timed-out`; only a required check that times out is a problem. The rust-analyzer lookup keeps its own limit of 60 seconds per candidate.
