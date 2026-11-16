# Finding rust-analyzer

alsp indexes with `rust-analyzer scip`. It looks for the program in this order and uses the first that answers `--version`:

1. `ALSP_RUST_ANALYZER`. A broken value is an error naming it; alsp does not fall back.
2. Each `rust-analyzer` on `PATH`. A rustup shim is tried under the toolchain the repository pins (`RUSTUP_TOOLCHAIN`, else `rust-toolchain(.toml)`), so a pinned toolchain without the component is noticed here, not halfway through indexing.
3. The `rust-analyzer` of another installed rustup toolchain (`stable` first), run by its real path. The repository's own toolchain still drives cargo. The `Index:` line says when this happened.

If none runs, the index fails (commands exit 3) and the message lists what was tried and ends with `fix: <command>`, for example `rustup component add rust-analyzer --toolchain nightly-2026-07-22`. alsp never installs anything.

Trust rules: relative `PATH` entries, and any file that resolves inside the repository (or the snapshot being indexed), are never run. Probes start in a neutral directory, never in the repository, so a repository's rust-toolchain file cannot choose what runs. A toolchain name read from that file is used only if it is a plain name (letters, digits, `.`, `_`, `-`).

From code: `alsp_index::locate_rust_analyzer(repo)` returns `Located { path, version, source }` or a `LocateError` with the `tried` candidates and `fix`.
