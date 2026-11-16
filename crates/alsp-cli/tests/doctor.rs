//! `alsp doctor` end to end. Every tool is a shell script on a private PATH (plus /usr/bin for git),
//! so nothing real is probed. The repository is a throwaway git tree, never on PATH.
#![allow(non_snake_case)]
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Fx {
    repo: tempfile::TempDir,
    tools: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fx {
    /// A repository (with a build.rs that would leave `built-marker` if cargo ever built it) and
    /// every tool doctor looks for, all answering. Individual tests remove or break some.
    fn new() -> Fx {
        let (repo, tools, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let r = repo.path();
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(r).args(args).output().unwrap().status.success());
        git(&["init", "-q", "-b", "main"]);
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join("Cargo.toml"), "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nbuild = \"build.rs\"\n").unwrap();
        fs::write(r.join("build.rs"), "fn main() { std::fs::write(\"built-marker\", \"x\").unwrap(); }\n").unwrap();
        fs::write(r.join("src/lib.rs"), "pub fn api() {}\n").unwrap();
        let t = tools.path();
        script(t, "rust-analyzer", "echo 'rust-analyzer 1.0'");
        script(t, "cargo-llvm-cov", "echo 'cargo-llvm-cov 0.6.0'");
        script(t, "cargo-mutants", "echo 'cargo-mutants 25.0'");
        script(t, "claude", "echo 'alsp@agent-lsp'");
        script(t, "codex", "echo 'alsp@agent-lsp'");
        script(t, "agy", "echo 'alsp@agent-lsp'");
        script(t, "bun", "exit 0");
        fs::create_dir_all(home.path().join("sdd/src/cli")).unwrap();
        fs::write(home.path().join("sdd/src/cli/decide.ts"), "").unwrap();
        Fx { repo, tools, home }
    }

    fn remove(&self, tool: &str) {
        fs::remove_file(self.tools.path().join(tool)).unwrap();
    }

    fn doctor(&self, extra: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.arg("doctor").arg("--repo").arg(self.repo.path()).args(extra).current_dir(self.home.path());
        c.env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.tools.path().display()))
            .env("HOME", self.home.path())
            .env("TMPDIR", std::env::temp_dir())
            .env("ALSP_SDD_ROOT", self.home.path().join("sdd"))
            .env("ALSP_BUN", self.tools.path().join("bun"));
        c.output().unwrap()
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn checks(o: &Output) -> Vec<Value> {
    serde_json::from_slice::<Value>(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(o))).as_array().unwrap().clone()
}

fn check<'a>(all: &'a [Value], name: &str) -> &'a Value {
    all.iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no check {name}: {all:?}"))
}

#[test]
fn test_DW_4_1_all_tools_present_is_ready() {
    let fx = Fx::new();
    let o = fx.doctor(&[]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.trim_end().ends_with("alsp doctor: ready") && t.lines().last() == Some("alsp doctor: ready"), "{t}");
    assert!(t.contains("rust-analyzer") && t.contains("claude plugin"), "{t}");
}

#[test]
fn test_DW_4_2_missing_rust_analyzer_fails_and_prints_the_fix() {
    let fx = Fx::new();
    fx.remove("rust-analyzer");
    // No rustup either: the Phase 2 fix is the install instruction.
    let o = fx.doctor(&[]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(1), "{t}");
    assert!(t.contains("fix: install rust-analyzer") && t.ends_with("alsp doctor: 1 problem\n"), "{t}");
    // With a rustup that has no rust-analyzer, the fix is the rustup command.
    script(fx.tools.path(), "rustup", "case \"$1\" in toolchain) echo stable-x;; which) echo \"error: unknown binary 'rust-analyzer'\" >&2; exit 1;; esac");
    let t = text(&fx.doctor(&[]));
    assert!(t.contains("fix: rustup component add rust-analyzer --toolchain"), "{t}");
}

#[test]
fn test_DW_4_3_missing_optional_tools_are_optional_with_fixes_and_do_not_fail() {
    let fx = Fx::new();
    for t in ["cargo-mutants", "cargo-llvm-cov", "bun", "agy"] {
        fx.remove(t);
    }
    fs::remove_dir_all(fx.home.path().join("sdd")).unwrap();
    let mut c = fx.doctor(&["--json"]);
    // ALSP_BUN pointed at the removed bun and ALSP_SDD_ROOT at the removed checkout: sdd is missing.
    assert_eq!(c.status.code(), Some(0), "{}", text(&c));
    let all = checks(&c);
    for (name, fix) in [("cargo-mutants", "cargo install cargo-mutants"), ("cargo-llvm-cov", "cargo install cargo-llvm-cov"), ("sdd decide + bun", "ALSP_SDD_ROOT"), ("agy CLI", "agy")] {
        let ch = check(&all, name);
        assert_eq!((ch["status"].as_str(), ch["required"].as_bool()), (Some("missing"), Some(false)), "{ch}");
        assert!(ch["fix"].as_str().unwrap().contains(fix), "{ch}");
    }
    assert_eq!(check(&all, "agy plugin")["status"], "skipped");
    assert!(check(&all, "cargo-llvm-cov")["detail"].as_str().unwrap().contains("`cover` and `tests --run`"));
    c = fx.doctor(&[]);
    assert!(text(&c).ends_with("alsp doctor: ready\n"), "{}", text(&c));
}

#[test]
fn test_DW_4_4_json_has_one_object_per_check() {
    let fx = Fx::new();
    let all = checks(&fx.doctor(&["--json"]));
    for name in ["alsp", "git", "repository", "rust-analyzer", "cargo-llvm-cov", "cargo-mutants", "sdd decide + bun", "claude CLI", "claude plugin", "codex CLI", "codex plugin", "agy CLI", "agy plugin"] {
        let c = check(&all, name);
        for key in ["name", "status", "required", "fix"] {
            assert!(c.get(key).is_some(), "{name} lacks {key}: {c}");
        }
        assert!(c["required"].is_boolean() && c["status"].is_string(), "{c}");
    }
    assert_eq!(all.len(), 13, "{all:?}");
    assert_eq!((check(&all, "rust-analyzer")["required"].as_bool(), check(&all, "cargo-llvm-cov")["required"].as_bool()), (Some(true), Some(false)));
    assert_eq!(check(&all, "git")["fix"], Value::Null);
}

#[test]
fn test_DW_4_5_a_hanging_probe_is_cut_off_and_reported() {
    let fx = Fx::new();
    script(fx.tools.path(), "cargo-llvm-cov", "sleep 60");
    script(fx.tools.path(), "codex", "sleep 60");
    let started = Instant::now();
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("doctor").arg("--repo").arg(fx.repo.path()).arg("--json").env_clear().env("ALSP_DOCTOR_TIMEOUT_SECS", "1");
    c.env("PATH", format!("{}:/usr/bin:/bin", fx.tools.path().display())).env("HOME", fx.home.path()).env("ALSP_SDD_ROOT", fx.home.path().join("sdd"));
    let o = c.output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    let all = checks(&o);
    assert_eq!(check(&all, "cargo-llvm-cov")["status"], "timed-out");
    assert_eq!(check(&all, "codex plugin")["status"], "timed-out");
    assert_eq!(o.status.code(), Some(0), "both hung tools are optional");
}

#[test]
fn test_DW_4_6_doctor_does_not_run_build_scripts() {
    let fx = Fx::new();
    // Even a cargo on PATH that would build: doctor must not call it.
    script(fx.tools.path(), "cargo", "touch \"$ALSP_CARGO_CALLED\"; echo built");
    let called = fx.home.path().join("cargo-called");
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("doctor").arg("--repo").arg(fx.repo.path()).env("ALSP_CARGO_CALLED", &called);
    c.env("PATH", format!("{}:/usr/bin:/bin", fx.tools.path().display())).env("HOME", fx.home.path());
    let o = c.output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(!fx.repo.path().join("built-marker").exists(), "build.rs ran");
    assert!(!called.exists(), "cargo was run");
    assert!(!fx.repo.path().join("target").exists() && !fx.repo.path().join(".alsp").exists());
}

#[test]
fn test_DW_4_6_a_tool_inside_the_repository_is_never_run() {
    let fx = Fx::new();
    fx.remove("cargo-llvm-cov");
    let marker = fx.repo.path().join("ran-marker");
    script(&fx.repo.path().join("bin"), "cargo-llvm-cov", &format!("touch {}; echo 'cargo-llvm-cov 9'", marker.display()));
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("doctor").arg("--repo").arg(fx.repo.path()).arg("--json");
    c.env("PATH", format!("{}:{}:/usr/bin:/bin", fx.repo.path().join("bin").display(), fx.tools.path().display())).env("HOME", fx.home.path());
    let o = c.output().unwrap();
    assert!(!marker.exists(), "a repository binary ran");
    assert_eq!(check(&checks(&o), "cargo-llvm-cov")["status"], "missing");
    assert_eq!(o.status.code(), Some(0), "cargo-llvm-cov is optional");
}

#[test]
fn test_DW_4_7_no_agent_clis_still_ready() {
    let fx = Fx::new();
    for t in ["claude", "codex", "agy"] {
        fx.remove(t);
    }
    let o = fx.doctor(&[]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.ends_with("alsp doctor: ready\n") && t.contains("claude CLI") && t.contains("optional"), "{t}");
}

#[test]
fn outside_a_repository_repository_checks_are_skipped_and_said_so() {
    let fx = Fx::new();
    let outside = tempfile::tempdir().unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("doctor").arg("--repo").arg(outside.path()).env_clear().env("HOME", fx.home.path()).env("ALSP_SDD_ROOT", fx.home.path().join("sdd"));
    c.env("PATH", format!("{}:/usr/bin:/bin", fx.tools.path().display()));
    let o = c.output().unwrap();
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.contains("skipped") && t.contains("not inside a git work tree") && t.ends_with("alsp doctor: ready\n"), "{t}");
}

#[test]
fn a_git_older_than_2_31_is_a_problem() {
    let fx = Fx::new();
    // A fake git ahead of the real one answers --version only; the old version is what is reported.
    script(fx.tools.path(), "git", "echo 'git version 2.30.1'");
    let o = fx.doctor(&["--json"]);
    let all = checks(&o);
    assert_eq!(check(&all, "git")["status"], "missing");
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn test_DW_4_6_a_git_in_the_repository_found_through_a_relative_path_entry_is_never_run() {
    let fx = Fx::new();
    let marker = fx.repo.path().join("git-ran-marker");
    script(fx.repo.path(), "git", &format!("touch {}; echo 'git version 9.9.9'", marker.display()));
    for path in [".", "", "bin"] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.arg("doctor").current_dir(fx.repo.path()).env_clear().env("HOME", fx.home.path()).env("ALSP_SDD_ROOT", fx.home.path().join("sdd"));
        c.env("PATH", format!("{path}:{}:/usr/bin:/bin", fx.tools.path().display()));
        let o = c.output().unwrap();
        assert!(!marker.exists(), "the repository's git ran with PATH entry {path:?}: {}", text(&o));
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    }
}

#[test]
fn test_DW_4_5_a_hanging_git_does_not_stall_repository_detection() {
    let fx = Fx::new();
    script(fx.tools.path(), "git", "sleep 30");
    let started = Instant::now();
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("doctor").arg("--repo").arg(fx.repo.path()).arg("--json").env_clear().env("ALSP_DOCTOR_TIMEOUT_SECS", "1");
    c.env("PATH", format!("{}:/usr/bin:/bin", fx.tools.path().display())).env("HOME", fx.home.path()).env("ALSP_SDD_ROOT", fx.home.path().join("sdd"));
    let o = c.output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
    let all = checks(&o);
    assert_eq!(check(&all, "git")["status"], "timed-out");
    assert_eq!(check(&all, "repository")["status"], "ready", "the repository is found without running git");
    assert_eq!(o.status.code(), Some(1));
}
