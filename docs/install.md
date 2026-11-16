# Installing alsp

```
curl -fsSL https://raw.githubusercontent.com/ryanthedev/agent-lsp/main/scripts/install.sh | sh
```

Or from a clone: `scripts/install.sh`. Run it again to update; every step is safe to repeat. The script is plain `sh`, and it builds from source, so it needs [Rust](https://rustup.rs). Without `cargo` it prints that and stops.

| Option | Effect |
|---|---|
| `--dry-run` | Print every command with a leading `+` and run none of them. |
| `--rev <rev>` | Build the binary from this branch, tag or commit. The plugin copies still come from the default branch. |

## What it does

1. `cargo install --git https://github.com/ryanthedev/agent-lsp alsp-cli --locked --force` (this overwrites the installed `alsp`).
2. `rustup component add rust-analyzer`, only when `rust-analyzer --version` does not run.
3. For each agent CLI on `PATH`, and only those:
   - Claude Code: `claude plugin marketplace add`, `marketplace update agent-lsp`, `plugin install`, `plugin update alsp@agent-lsp`.
   - Codex: `codex plugin marketplace add`, `marketplace upgrade agent-lsp`, `plugin add alsp@agent-lsp`.
   - agy: clone `https://github.com/ryanthedev/agent-lsp` to `~/.local/share/agent-lsp-plugin` (or `git pull --ff-only` it, after pointing a stale `origin` back at the public repository), then `agy plugin import` that directory. Set `ALSP_PLUGIN_DIR` to keep the copy elsewhere.
4. `alsp doctor` ([doctor.md](doctor.md)).

"Already added" and "already installed" answers from the first command of a pair are ignored; the update that follows must succeed. If one agent's step fails, the script says which, goes on with the others, and exits 1 at the end. `alsp doctor` still runs.

## Undo

A bad binary is replaced by re-running with a known good `--rev`. Plugins:

```
claude plugin uninstall alsp@agent-lsp && claude plugin marketplace remove agent-lsp
codex plugin remove alsp@agent-lsp && codex plugin marketplace remove agent-lsp
agy plugin uninstall alsp
cargo uninstall alsp-cli
```
