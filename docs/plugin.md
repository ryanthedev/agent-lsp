# Plugin: alsp for Claude, Codex and agy

The plugin is a skill (`skills/alsp/SKILL.md`) that teaches an agent the `alsp` command line: which command answers which question, how to read the `[resolved]`, `[observed]` and `[predicted]` marks, the exit codes, and the trust caveats. It contains no agent-specific logic and no server; the agent runs the same `alsp` commands from any host. It does not install the `alsp` binary; `scripts/install.sh` installs both (see [install.md](install.md)).

Files, one set of skills read by all three hosts:

| Path | Used by |
|---|---|
| `.claude-plugin/plugin.json`, `.claude-plugin/marketplace.json` | Claude Code; Codex also reads the marketplace file; agy converts the Claude plugin on import |
| `.codex-plugin/plugin.json` | Codex |
| `skills/alsp/SKILL.md` | all three |

The repository root is the plugin root (the marketplace entry has `"source": "./"`), so installing copies the whole repository. Install from GitHub or from a fresh clone rather than a working copy with a large `target/` directory.

## Install and update

```
curl -fsSL https://raw.githubusercontent.com/ryanthedev/agent-lsp/main/scripts/install.sh | sh
```

The script installs the binary and the plugin for every one of `claude`, `codex` and `agy` it finds. To update, re-run it: it rebuilds the binary and refreshes each plugin from the public repository, so all three agents end on the same version. Then run `alsp doctor`. Claude Code refreshes an installed plugin only when its version changes, so any change under `skills/` or to a plugin manifest must bump the version (see CONTRIBUTING.md). Options and undo commands are in [install.md](install.md).

The by-hand commands, one agent at a time (each installs the skill only):

**Claude Code**

```
claude plugin marketplace add ryanthedev/agent-lsp
claude plugin install alsp@agent-lsp
```

From a local clone, use its path instead of `ryanthedev/agent-lsp`. To try it for one session without installing: `claude --plugin-dir <path to this repository>`.

**Codex**

```
codex plugin marketplace add ryanthedev/agent-lsp
codex plugin add alsp@agent-lsp
```

**agy** installs from a directory, so the script keeps a managed clone (`~/.local/share/agent-lsp-plugin`) and imports it:

```
git clone https://github.com/ryanthedev/agent-lsp ~/.local/share/agent-lsp-plugin
agy plugin import ~/.local/share/agent-lsp-plugin
```

`import` converts the Claude-format plugin into agy's layout and installs it (`agy plugin list` shows it).

## Check it

```
claude plugin validate .
claude plugin validate .claude-plugin/marketplace.json
agy plugin validate <imported copy>     # agy validates agy-format plugins, so run it after import
```

`agy plugin validate .` on the repository itself reports "no plugin.json found": that is the Claude-format layout before conversion, not a defect. After import it warns that `AGENTS.md` is never loaded (agy reads rules only from `rules/`) and that there is no logo; neither affects the skill.

To see that the skill fires, ask in a Rust repository: "who calls `<function>`?". The agent should load the `alsp` skill and run `alsp refs <function>`. In a repository with a package that has its own `[workspace]` (say `apps/foo/rust`), the first answer may be exit 4 until the agent adds `--root <package dir>`; the skill says so.

## Trust

The skill states the trust caveats briefly and links `docs/query.md`, "Trust and cost", which is the authority: building the index runs the repository's build scripts and proc macros, and `cover`, `gaps`, `tests --run` and `review --full` run its tests.
