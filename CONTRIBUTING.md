# Contributing

## Build and test

```
cargo build --workspace
cargo test --workspace
```

You need a Rust toolchain and `git` 2.31 or newer. Many tests build small throwaway repositories and use stub tools, so most run without `rust-analyzer`, `cargo-llvm-cov`, `cargo-mutants`, sdd or any agent CLI installed. Tests that read documentation (the trust wording in `docs/query.md` and `docs/review.md`, and the JSON keys listed in `docs/query.md`) fail when you change that text without updating the code, and the other way round.

## Trust

`alsp` runs the code it analyses: indexing runs build scripts and proc macros, and coverage and gaps run the repository's tests. The same applies when you run its test suite or try your changes on a repository you did not write. Do that only in a sandbox or throwaway VM unless you trust the code. Changes that touch how `alsp` starts `git` or an agent must keep the rules in `docs/query.md` ("Trust and cost") and `docs/review.md` true, and add a test that plants the hostile case.

## Reporting issues

Open an issue on GitHub with the command you ran, `alsp`'s full output including the `!` note lines and exit code, your `rust-analyzer --version`, and whether the repository is a single workspace or has packages with their own `[workspace]`. Do not paste code you are not allowed to share. Report a security problem (a way to make `alsp` run something the docs say it will not) privately to the repository owner rather than in a public issue.

## Pull requests

Keep a change to one topic, with tests that assert the intended behavior. Update the docs in the same change; `AGENTS.md` lists the conventions. By contributing you agree your work is released under the repository's license (MIT).
