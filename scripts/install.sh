#!/bin/sh
# Install or update alsp and its agent plugins. Safe to re-run.
#
#   curl -fsSL https://raw.githubusercontent.com/ryanthedev/agent-lsp/main/scripts/install.sh | sh
#   scripts/install.sh [--dry-run] [--rev <git rev>]
#
# --dry-run  print every command and run none of them
# --rev      build the binary from this branch, tag or commit instead of the default branch
#
# Steps: cargo install alsp, rust-analyzer if missing, then the plugin for each of claude,
# codex and agy that is on PATH (none is required), then `alsp doctor`.
# Everything is inside main() so a download cut short never runs half a script.

REPO_URL=https://github.com/ryanthedev/agent-lsp
MARKETPLACE_REPO=ryanthedev/agent-lsp
PLUGIN=alsp@agent-lsp

dry=0
rev=
rev_set=0
failed=0

say() { printf '%s\n' "$*"; }
warn() { printf 'install.sh: %s\n' "$*" >&2; }

# Print a command, then run it unless this is a dry run.
run() {
    printf '+'
    for a in "$@"; do printf ' %s' "$a"; done
    printf '\n'
    [ "$dry" = 1 ] && return 0
    "$@"
}

# A step whose failure is reported and counted; the script carries on with the next one.
step() {
    run "$@" || { warn "failed: $*"; failed=1; return 1; }
}

# A step that may fail harmlessly (already added, already installed); `update` follows it.
try() {
    run "$@" || warn "ignored (probably already present): $*"
    return 0
}

have() { command -v "$1" >/dev/null 2>&1; }

install_claude() {
    say "claude: installing or updating the alsp plugin"
    try claude plugin marketplace add "$MARKETPLACE_REPO"
    step claude plugin marketplace update agent-lsp || return 0
    try claude plugin install "$PLUGIN"
    step claude plugin update "$PLUGIN"
}

install_codex() {
    say "codex: installing or updating the alsp plugin"
    try codex plugin marketplace add "$MARKETPLACE_REPO"
    step codex plugin marketplace upgrade agent-lsp || return 0
    if ! run codex plugin add "$PLUGIN"; then
        # The add may fail only because the plugin is already there; the list says which.
        listed=$(codex plugin list 2>/dev/null)
        case $listed in
            *"$PLUGIN"*) present=1 ;;
            *) present=0 ;;
        esac
        if [ "$present" = 1 ]; then
            warn "ignored (already present): codex plugin add $PLUGIN"
        else
            warn "failed: codex plugin add $PLUGIN (alsp is not in 'codex plugin list')"
            failed=1
        fi
    fi
}

# agy installs from a directory, so keep a managed copy of the public repo and import it.
install_agy() {
    say "agy: installing or updating the alsp plugin"
    copy=${ALSP_PLUGIN_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/agent-lsp-plugin}
    if [ -d "$copy/.git" ]; then
        # A copy cloned from a local path (or an old fork) would keep pulling the wrong thing.
        origin_ok=0
        if [ -r "$copy/.git/config" ]; then
            while IFS= read -r line; do
                case $line in *"url = $REPO_URL"|*"url = $REPO_URL.git") origin_ok=1 ;; esac
            done <"$copy/.git/config"
        fi
        if [ "$origin_ok" = 0 ]; then
            step git -C "$copy" remote set-url origin "$REPO_URL" || return 0
        fi
        step git -C "$copy" -c protocol.allow=never -c protocol.https.allow=always pull --ff-only || return 0
    else
        step git -c protocol.allow=never -c protocol.https.allow=always clone --depth 1 "$REPO_URL" "$copy" || return 0
    fi
    step agy plugin import "$copy"
}

main() {
    while [ $# -gt 0 ]; do
        case $1 in
            --dry-run) dry=1 ;;
            --rev)
                [ $# -ge 2 ] || { warn "--rev needs a value"; return 2; }
                rev=$2
                rev_set=1
                shift
                ;;
            -h|--help) say "usage: install.sh [--dry-run] [--rev <git rev>]"; return 0 ;;
            *) warn "unknown argument: $1 (try --help)"; return 2 ;;
        esac
        shift
    done
    [ "$rev_set" = 0 ] || case $rev in
        ''|-*|*[!A-Za-z0-9._/-]*) warn "--rev takes a branch, tag or commit name, got: $rev"; return 2 ;;
    esac

    # rustup puts cargo here; a non-interactive shell does not always have it on PATH.
    PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    export PATH

    if ! have cargo; then
        warn "cargo not found. alsp is built from source, so install Rust first:"
        warn "  https://rustup.rs  (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh)"
        warn "then open a new shell and run this script again."
        return 1
    fi

    say "alsp: building and installing the binary"
    if [ "$rev_set" = 1 ]; then
        step cargo install --git "$REPO_URL" --rev "$rev" alsp-cli --locked --force || return 1
    else
        step cargo install --git "$REPO_URL" alsp-cli --locked --force || return 1
    fi

    # The rustup shim for rust-analyzer exists even when the component does not, so run it.
    if ! rust-analyzer --version >/dev/null 2>&1; then
        say "rust-analyzer: missing"
        if have rustup; then
            step rustup component add rust-analyzer
        else
            warn "rust-analyzer is missing and rustup is not here; install it (https://rust-analyzer.github.io)"
            failed=1
        fi
    fi

    found=0
    for agent in claude codex agy; do
        if have "$agent"; then
            found=1
            "install_$agent"
        else
            say "$agent: not found, skipped"
        fi
    done
    [ "$found" = 1 ] || say "no agent CLI found; the binary is installed, add a plugin later by re-running this script"

    say "alsp doctor:"
    run alsp doctor || failed=1
    [ "$failed" = 0 ]
}

main "$@"
