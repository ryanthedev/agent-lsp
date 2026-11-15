//! Ad-hoc `alsp review` (no --base): default range, uncommitted work, tiers, recording.
//! All fixtures change only non-Rust files, so no indexer, coverage tool or model is ever started.
#![allow(non_snake_case)]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Ignore the developer's global git config and ignore file (a global `.env` rule would hide fixtures).
fn hermetic(c: &mut Command) -> &mut Command {
    c.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").env("XDG_CONFIG_HOME", "/nonexistent")
}
fn git(dir: &Path, args: &[&str]) -> String {
    let o = hermetic(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args)).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().into()
}
fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}
fn commit(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

struct Fx { repo: tempfile::TempDir, cache: tempfile::TempDir, c0: String }
impl Fx {
    /// `main` has one commit (a.txt); the checkout is on branch `feat`, still at that commit.
    fn new() -> Fx {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        write(repo.path(), "a.txt", "one\n");
        let c0 = commit(repo.path(), "c0");
        git(repo.path(), &["checkout", "-q", "-b", "feat"]);
        Fx { repo, cache: tempfile::tempdir().unwrap(), c0 }
    }
    fn p(&self) -> &Path { self.repo.path() }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.arg("review").arg("--repo").arg(self.p()).args(args).env("ALSP_CACHE_DIR", self.cache.path());
        hermetic(&mut c);
        c
    }
    fn run(&self, args: &[&str]) -> Output { self.cmd(args).output().unwrap() }
    /// Two commits after c0 (b.txt, c.txt) plus an uncommitted edit of a.txt and a staged new file d.txt.
    fn three_changes(&self) {
        write(self.p(), "b.txt", "b\n");
        commit(self.p(), "c1");
        write(self.p(), "c.txt", "c\n");
        commit(self.p(), "c2");
        write(self.p(), "a.txt", "one edited\n");
        write(self.p(), "d.txt", "staged\n");
        git(self.p(), &["add", "d.txt"]);
    }
}
fn stdout(o: &Output) -> String { String::from_utf8_lossy(&o.stdout).into_owned() }
fn stderr(o: &Output) -> String { String::from_utf8_lossy(&o.stderr).into_owned() }
fn first_line(o: &Output) -> String { stderr(o).lines().next().unwrap_or_default().to_string() }
fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    write(dir, name, &format!("#!/bin/sh\n{body}\n"));
    let p = dir.join(name);
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn test_DW_7_1_no_arguments_reviews_two_commits_and_an_uncommitted_edit_then_says_nothing_to_review() {
    let e = Fx::new();
    e.three_changes();
    // What the review would see: --print shows the prompt, with the diff, and records nothing.
    let p = e.run(&["--print"]);
    assert_eq!(p.status.code(), Some(0), "{}", stderr(&p));
    let prompt = stdout(&p);
    for f in ["b.txt", "c.txt", "a.txt", "d.txt"] { assert!(prompt.contains(&format!("diff --git a/{f} b/{f}")), "{f} missing from the diff:\n{prompt}"); }
    assert!(!e.p().join(".alsp/reviews.jsonl").exists(), "--print must not record a review");

    let first = e.run(&[]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert!(stdout(&first).starts_with("Review "), "{}", stdout(&first));
    let rec: serde_json::Value = serde_json::from_str(fs::read_to_string(e.p().join(".alsp/reviews.jsonl")).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!((rec["branch"].as_str(), rec["uncommitted"].as_bool()), (Some("feat"), Some(true)));
    assert_eq!(rec["head"], git(e.p(), &["rev-parse", "HEAD"]));
    assert_ne!(rec["reviewed"], rec["head"], "the reviewed commit is the work-tree snapshot");

    let second = e.run(&[]);
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(stdout(&second).trim(), "nothing to review");
    assert!(first_line(&second).contains("with its uncommitted changes"), "{}", first_line(&second));

    // A later edit is the only thing the next run shows.
    write(e.p(), "e.txt", "new\n");
    git(e.p(), &["add", "e.txt"]);
    let p = stdout(&e.run(&["--print"]));
    assert!(p.contains("diff --git a/e.txt b/e.txt"), "{p}");
    for f in ["b.txt", "c.txt", "d.txt"] { assert!(!p.contains(&format!("diff --git a/{f} b/{f}")), "{f} reviewed twice:\n{p}"); }
}

#[test]
fn test_DW_7_2_first_line_names_base_reason_tier_and_estimate() {
    let e = Fx::new();
    e.three_changes();
    let o = e.run(&[]);
    let line = first_line(&o);
    assert!(line.starts_with("alsp review: base "), "{line}");
    assert!(line.contains(&e.c0[..8]) && line.contains("merge-base with main"), "{line}");
    assert!(line.contains("default tier") && line.contains("agents none") && line.contains("seconds on a warm index, free"), "{line}");
    assert!(line.contains("uncommitted changes"), "{line}");
}

#[test]
fn test_DW_7_2_tiers_state_agents_and_cost_and_explicit_flags_override_them() {
    let e = Fx::new();
    e.three_changes();
    write(e.p(), ".alsp/enabled", "agent = claude,codex\n");
    let marker = e.p().join("../agent-ran");
    let agent = script(e.cache.path(), "agent.sh", &format!("touch '{}'\necho 'NO FINDINGS'", marker.display()));
    let run = |args: &[&str]| { let mut c = e.cmd(args); c.env("ALSP_AGENT_CLAUDE", &agent).env("ALSP_AGENT_CODEX", "/missing/agent"); c.output().unwrap() };

    let d = first_line(&run(&[]));
    assert!(d.contains("default tier, agents claude,codex: ~2-10 min, uses agent quota"), "{d}");
    assert!(marker.exists(), "the configured agent should have run in the default tier");
    fs::remove_file(&marker).unwrap();

    let q = run(&["--quick"]);
    assert!(first_line(&q).contains("quick tier, agents none: seconds on a warm index, free"), "{}", first_line(&q));
    assert!(!marker.exists(), "--quick starts no agent");

    let f = first_line(&run(&["--full"]));
    assert!(f.contains("full tier, agents claude,codex: ~2-10 min, uses agent quota, tests run, ~$0.02-0.10 Gemini via sdd"), "{f}");

    let w = first_line(&run(&["--quick", "--agent", "claude"]));
    assert!(w.contains("quick tier, agents claude: ~2-10 min"), "{w}");
    let c = first_line(&run(&["--quick", "--with", "coverage"]));
    assert!(c.contains("agents none: tests run") && !c.contains("Gemini"), "{c}");
    // Both tiers cannot be asked at once.
    assert_eq!(run(&["--quick", "--full"]).status.code(), Some(2));
}

#[test]
fn test_DW_7_2_an_unusable_settings_file_is_reported_and_the_review_still_runs() {
    let e = Fx::new();
    e.three_changes();
    write(e.p(), ".alsp/enabled", "agent = nobody\n");
    let o = e.run(&[]);
    assert_eq!(o.status.code(), Some(0));
    let err = stderr(&o);
    assert!(err.contains("ignoring review settings"), "{err}");
    assert!(err.lines().any(|l| l.starts_with("alsp review: base ") && l.contains("agents none")), "{err}");
}

#[test]
fn test_DW_7_3_a_rebased_away_base_falls_back_to_the_merge_base_with_a_note() {
    let e = Fx::new();
    write(e.p(), "b.txt", "b\n");
    let old = commit(e.p(), "c1");
    assert_eq!(e.run(&[]).status.code(), Some(0)); // records `old`
    // Rewrite history: the reviewed commit leaves the branch.
    git(e.p(), &["reset", "-q", "--hard", &e.c0]);
    write(e.p(), "b.txt", "b rewritten\n");
    commit(e.p(), "c1 again");
    let o = e.run(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let line = first_line(&o);
    assert!(line.contains(&e.c0[..8]) && line.contains("merge-base with main") && line.contains(&format!("last reviewed {} is no longer in this branch's history (rebased?)", &old[..8])), "{line}");
    assert!(stdout(&o).starts_with("Review "), "the rewritten commit is reviewed, not skipped:\n{}", stdout(&o));
}

#[test]
fn test_DW_7_3_no_earlier_review_and_no_default_branch_asks_for_base_and_exits_2() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "work"]);
    write(repo.path(), "a.txt", "x\n");
    commit(repo.path(), "c0");
    write(repo.path(), "a.txt", "y\n");
    let cache = tempfile::tempdir().unwrap();
    let o = hermetic(Command::new(env!("CARGO_BIN_EXE_alsp")).arg("review").arg("--repo").arg(repo.path()).env("ALSP_CACHE_DIR", cache.path())).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no default branch") && stderr(&o).contains("pass --base <rev>"), "{}", stderr(&o));
    // Passing --base works.
    let ok = hermetic(Command::new(env!("CARGO_BIN_EXE_alsp")).arg("review").arg("--repo").arg(repo.path()).args(["--base", "HEAD"]).env("ALSP_CACHE_DIR", cache.path())).output().unwrap();
    assert_eq!(ok.status.code(), Some(0), "{}", stderr(&ok));
    assert!(first_line(&ok).contains("(--base)"), "{}", first_line(&ok));
}

#[test]
fn test_empty_range_exits_0_with_nothing_to_review_and_hints_at_base() {
    let e = Fx::new(); // feat == main, clean tree
    let o = e.run(&[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o).trim(), "nothing to review");
    assert!(first_line(&o).contains("pass --base to look further back"), "{}", first_line(&o));
    // JSON consumers get JSON.
    let j = e.run(&["--format", "json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&j)).unwrap();
    assert_eq!(v["nothing_to_review"], true);
}

#[test]
fn test_untracked_files_are_not_reviewed_unless_asked_and_the_banner_says_so() {
    let e = Fx::new();
    write(e.p(), "new/dir/n.txt", "fresh\n");
    let o = e.run(&[]);
    assert_eq!(stdout(&o).trim(), "nothing to review", "{}", stdout(&o));
    assert!(first_line(&o).contains("1 untracked file(s) not reviewed (pass --untracked): new/dir/n.txt"), "{}", first_line(&o));
    let p = stdout(&e.run(&["--untracked", "--print"]));
    assert!(p.contains("diff --git a/new/dir/n.txt b/new/dir/n.txt"), "{p}");
    // More than five names: five are listed, the rest counted.
    for i in 0..7 { write(e.p(), &format!("u{i}.txt"), "x\n"); }
    let line = first_line(&e.run(&["--print"]));
    assert!(line.contains("8 untracked file(s) not reviewed") && line.contains("+3 more"), "{line}");
}

#[test]
fn test_untracked_env_and_new_source_stay_out_of_the_default_diff_and_agent_export() {
    let e = Fx::new();
    write(e.p(), "a.txt", "one edited\n");
    write(e.p(), ".env", "TOKEN=hunter2\n");
    write(e.p(), "new.rs", "fn new() {}\n");
    let o = e.run(&["--print"]);
    let p = stdout(&o);
    assert!(p.contains("diff --git a/a.txt") && !p.contains("hunter2") && !p.contains("new.rs") && !p.contains(".env"), "{p}");
    assert!(first_line(&o).contains("2 untracked file(s) not reviewed (pass --untracked): .env, new.rs"), "{}", first_line(&o));
    let seen = e.cache.path().join("seen");
    let agent = script(e.cache.path(), "agent.sh", &format!("ls -a > '{}'\necho 'NO FINDINGS'", seen.display()));
    let mut c = e.cmd(&["--agent", "codex"]);
    c.env("ALSP_AGENT_CODEX", &agent);
    assert_eq!(c.output().unwrap().status.code(), Some(0));
    let seen = fs::read_to_string(seen).unwrap();
    assert!(!seen.lines().any(|l| l == ".env" || l == "new.rs"), "untracked files reached the agent export:\n{seen}");
}

#[test]
fn test_untracked_flag_includes_new_files_but_skips_and_names_secrets_and_large_files() {
    let e = Fx::new();
    write(e.p(), "new.txt", "fn new() {}\n");
    write(e.p(), ".env", "TOKEN=hunter2\n");
    write(e.p(), "sub/server.PEM", "KEY\n");
    write(e.p(), "big.txt", &"x".repeat(1024 * 1024 + 1));
    let o = e.run(&["--untracked", "--print"]);
    let (p, err) = (stdout(&o), stderr(&o));
    assert!(p.contains("diff --git a/new.txt b/new.txt") && !p.contains("hunter2") && !p.contains("diff --git a/big.txt") && !p.contains("diff --git a/sub/server.PEM"), "{p}");
    for want in ["skipped untracked .env (looks like a secret)", "skipped untracked sub/server.PEM (looks like a secret)", "skipped untracked big.txt (over 1 MB)"] { assert!(err.contains(want), "{want} missing:\n{err}"); }
    assert!(!first_line(&o).contains("not reviewed"), "included files are not reported as left out: {}", first_line(&o));
    // Only skipped files: the range is empty, and they are still named.
    let e2 = Fx::new();
    write(e2.p(), ".env", "x\n");
    let o = e2.run(&["--untracked"]);
    assert_eq!(stdout(&o).trim(), "nothing to review");
    assert!(stderr(&o).contains("skipped untracked .env"), "{}", stderr(&o));
}

#[test]
fn test_committed_flag_ignores_uncommitted_and_untracked_work() {
    let e = Fx::new();
    e.three_changes();
    let o = e.run(&["--committed", "--print"]);
    let p = stdout(&o);
    assert!(p.contains("diff --git a/b.txt") && p.contains("diff --git a/c.txt"), "{p}");
    assert!(!p.contains("diff --git a/a.txt") && !p.contains("d.txt"), "{p}");
    assert!(!first_line(&o).contains("uncommitted"), "{}", first_line(&o));
    // Recorded as committed-only; the dirty work is still waiting for the next default run.
    assert_eq!(e.run(&["--committed"]).status.code(), Some(0));
    assert_eq!(e.run(&["--committed"]).stdout, b"nothing to review\n");
    let next = stdout(&e.run(&["--print"]));
    assert!(next.contains("diff --git a/d.txt") && !next.contains("diff --git a/b.txt"), "{next}");
}

#[test]
fn test_ignored_files_and_dot_alsp_are_not_reviewed() {
    let e = Fx::new();
    write(e.p(), ".gitignore", "secret.txt\n");
    write(e.p(), "secret.txt", "s3cr3t\n");
    write(e.p(), ".alsp/enabled", "# nothing\n");
    let p = stdout(&e.run(&["--untracked", "--print"]));
    assert!(p.contains("diff --git a/.gitignore") && !p.contains("diff --git a/secret.txt") && !p.contains("diff --git a/.alsp"), "{p}");
}

#[test]
fn test_snapshot_leaves_checkout_index_and_refs_untouched() {
    let e = Fx::new();
    e.three_changes();
    let status = git(e.p(), &["status", "--porcelain"]);
    let refs = git(e.p(), &["for-each-ref"]);
    let staged = git(e.p(), &["diff", "--cached", "--name-only"]);
    assert_eq!(e.run(&[]).status.code(), Some(0));
    assert_eq!(git(e.p(), &["status", "--porcelain"]), status);
    assert_eq!(git(e.p(), &["for-each-ref"]), refs);
    assert_eq!(git(e.p(), &["diff", "--cached", "--name-only"]), staged);
    assert_eq!(fs::read_to_string(e.p().join("a.txt")).unwrap(), "one edited\n");
    assert!(fs::read_dir(e.p().join(".git")).unwrap().all(|f| !f.unwrap().file_name().to_string_lossy().starts_with("alsp-snapshot-index")), "temporary index left behind");
}

#[test]
fn test_agents_see_the_working_tree_in_a_private_export_never_the_checkout() {
    let e = Fx::new();
    e.three_changes();
    let seen = e.cache.path().join("seen");
    let agent = script(e.cache.path(), "agent.sh", &format!("pwd > '{0}'\nls -a >> '{0}'\ncat a.txt >> '{0}'\ntouch agent-was-here\necho 'NO FINDINGS'", seen.display()));
    let mut c = e.cmd(&["--agent", "codex"]);
    c.env("ALSP_AGENT_CODEX", &agent);
    assert_eq!(c.output().unwrap().status.code(), Some(0));
    let seen = fs::read_to_string(seen).unwrap();
    let cwd = seen.lines().next().unwrap();
    assert!(!Path::new(cwd).starts_with(fs::canonicalize(e.p()).unwrap()), "agent ran inside the checkout: {cwd}");
    assert!(seen.contains("d.txt") && seen.contains("one edited"), "the export should hold the uncommitted work:\n{seen}");
    assert!(!seen.lines().any(|l| l == ".git"), "no .git in the export:\n{seen}");
    assert!(!e.p().join("agent-was-here").exists());
}

#[test]
fn test_records_are_per_branch_and_other_branches_start_from_the_default_branch() {
    let e = Fx::new();
    write(e.p(), "b.txt", "b\n");
    commit(e.p(), "c1");
    assert_eq!(e.run(&[]).status.code(), Some(0));
    assert_eq!(e.run(&[]).stdout, b"nothing to review\n");
    git(e.p(), &["checkout", "-q", "-b", "other", &e.c0]);
    write(e.p(), "z.txt", "z\n");
    commit(e.p(), "z");
    let o = e.run(&[]);
    assert!(first_line(&o).contains("merge-base with main") && stdout(&o).starts_with("Review "), "{}", first_line(&o));
    // Detached HEAD has its own key and still works.
    git(e.p(), &["checkout", "-q", "--detach"]);
    let d = e.run(&[]);
    assert_eq!(d.status.code(), Some(0), "{}", stderr(&d));
    assert!(fs::read_to_string(e.p().join(".alsp/reviews.jsonl")).unwrap().contains("\"(detached)\""));
}

#[test]
fn test_a_tracked_dot_alsp_is_not_trusted_for_the_base() {
    let e = Fx::new();
    write(e.p(), "b.txt", "b\n");
    let c1 = commit(e.p(), "c1");
    // A repository-provided record claiming c1 was already reviewed.
    write(e.p(), ".alsp/reviews.jsonl", &format!("{{\"branch\":\"feat\",\"head\":\"{c1}\",\"reviewed\":\"{c1}\",\"uncommitted\":false}}\n"));
    git(e.p(), &["add", "-f", ".alsp/reviews.jsonl"]);
    git(e.p(), &["commit", "-q", "-m", "plant"]);
    let o = e.run(&[]);
    assert!(first_line(&o).contains("tracked by git") && first_line(&o).contains("merge-base with main"), "{}", first_line(&o));
    assert!(stdout(&o).starts_with("Review "), "{}", stdout(&o));
}

#[test]
fn test_a_symlinked_review_record_is_refused_without_failing_the_review() {
    let e = Fx::new();
    write(e.p(), "b.txt", "b\n");
    commit(e.p(), "c1");
    let target = e.cache.path().join("elsewhere");
    fs::write(&target, "").unwrap();
    fs::create_dir_all(e.p().join(".alsp")).unwrap();
    std::os::unix::fs::symlink(&target, e.p().join(".alsp/reviews.jsonl")).unwrap();
    let o = e.run(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(first_line(&o).contains("cannot read .alsp/reviews.jsonl"), "{}", first_line(&o));
    assert!(stderr(&o).contains("cannot record this review"), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(&target).unwrap(), "", "never written through the link");
}

#[test]
fn test_unreadable_untracked_file_fails_the_snapshot_and_names_committed_as_the_way_out() {
    if unsafe { libc_geteuid() } == 0 { return; } // root reads everything
    let e = Fx::new();
    write(e.p(), "locked.txt", "x\n");
    fs::set_permissions(e.p().join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    let o = e.run(&["--untracked"]);
    fs::set_permissions(e.p().join("locked.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("cannot snapshot uncommitted changes") && stderr(&o).contains("--committed"), "{}", stderr(&o));
    assert_eq!(e.run(&["--committed"]).status.code(), Some(0));
}
extern "C" { #[link_name = "geteuid"] fn libc_geteuid() -> u32; }

#[test]
fn test_explicit_base_still_includes_uncommitted_work_but_explicit_head_does_not() {
    let e = Fx::new();
    e.three_changes();
    let with_dirty = e.run(&["--base", &e.c0, "--print"]);
    assert!(stdout(&with_dirty).contains("diff --git a/d.txt") && first_line(&with_dirty).contains("(--base)"), "{}", first_line(&with_dirty));
    let committed = e.run(&["--base", &e.c0, "--head", "HEAD", "--print"]);
    assert!(!stdout(&committed).contains("d.txt"), "{}", stdout(&committed));
    assert!(!first_line(&committed).contains("uncommitted"));
}

#[test]
fn test_explicit_head_that_is_not_checked_out_records_nothing() {
    let e = Fx::new();
    write(e.p(), "b.txt", "b\n");
    commit(e.p(), "c1");
    let o = e.run(&["--base", &e.c0, "--head", &e.c0]);
    assert_eq!(o.status.code(), Some(0));
    assert!(!e.p().join(".alsp/reviews.jsonl").exists());
}

#[test]
fn test_agents_are_not_started_for_an_empty_ad_hoc_range() {
    let e = Fx::new();
    let marker = e.cache.path().join("ran");
    let agent = script(e.cache.path(), "agent.sh", &format!("touch '{}'\necho 'NO FINDINGS'", marker.display()));
    let mut c = e.cmd(&["--agent", "codex"]);
    c.env("ALSP_AGENT_CODEX", &agent);
    let o = c.output().unwrap();
    assert_eq!(stdout(&o).trim(), "nothing to review");
    assert!(!marker.exists());
}

#[test]
fn test_DW_8_1_help_lists_no_hook_or_queue_command() {
    let help = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_alsp")).args(args).output().unwrap();
    let o = help(&["--help"]);
    assert!(o.status.success());
    let commands: Vec<String> = stdout(&o).lines().skip_while(|l| !l.starts_with("Commands:")).skip(1).take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string)).collect();
    assert!(commands.iter().any(|c| c == "review"), "{commands:?}");
    assert!(!commands.iter().any(|c| c == "hook" || c == "queue"), "{commands:?}");
    for gone in ["hook", "queue"] { assert_eq!(help(&[gone]).status.code(), Some(2), "`alsp {gone}` should be an unknown command"); }
}

#[test]
fn test_a_tracked_dot_alsp_never_supplies_agents_to_a_review() {
    let e = Fx::new();
    write(e.p(), ".alsp/enabled", "agent = claude\n");
    git(e.p(), &["add", "-f", ".alsp/enabled"]);
    commit(e.p(), "track settings");
    write(e.p(), "b.txt", "b\n");
    let marker = e.p().join("../tracked-agent-ran");
    let agent = script(e.cache.path(), "agent.sh", &format!("touch '{}'\necho 'NO FINDINGS'", marker.display()));
    let mut c = e.cmd(&["--base", &e.c0]);
    c.env("ALSP_AGENT_CLAUDE", &agent);
    let o = c.output().unwrap();
    assert!(stderr(&o).contains("tracked by git"), "{}", stderr(&o));
    assert!(!marker.exists(), "a repository-provided file must not start an agent");
}
