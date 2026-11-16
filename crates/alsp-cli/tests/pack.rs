//! `alsp pack` end to end, offline: real git repositories, a fake `rust-analyzer` that serves a
//! SCIP file built from the fixture sources (alsp-evidence's testkit), a throwaway cache.
#![allow(non_snake_case)]

use alsp_evidence::testkit::scip_for;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

struct Env {
    repo: tempfile::TempDir,
    /// fake rust-analyzer, its fixtures and run log
    fx: tempfile::TempDir,
    ra: PathBuf,
    cache: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let repo = tempfile::tempdir().unwrap();
        let fx = tempfile::tempdir().unwrap();
        let ra = fx.path().join("ra.sh");
        fs::write(
            &ra,
            "#!/bin/sh\n\
             if [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\n\
             echo run >> \"$ALSP_TEST_FX/runs.log\"\n\
             name=$(basename \"$2\")\n\
             if [ -f \"$ALSP_TEST_FX/$name.scip\" ]; then cp \"$ALSP_TEST_FX/$name.scip\" \"$4\"\n\
             elif [ -f \"$ALSP_TEST_FX/default.scip\" ]; then cp \"$ALSP_TEST_FX/default.scip\" \"$4\"\n\
             else echo \"cannot load $name\" >&2; exit 1; fi\n",
        )
        .unwrap();
        fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        Env { repo, fx, ra, cache: tempfile::tempdir().unwrap() }
    }

    fn dir(&self) -> &Path {
        self.repo.path()
    }

    /// The fake indexer will serve an index of these files (the head tree's Rust sources).
    fn serve(&self, name: &str, files: &[(&str, &str)]) {
        fs::write(self.fx.path().join(format!("{name}.scip")), scip_for(files)).unwrap();
    }

    fn runs(&self) -> usize {
        fs::read_to_string(self.fx.path().join("runs.log")).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn pack(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_alsp"))
            .arg("pack")
            .arg("--repo")
            .arg(self.dir())
            .arg("--cache-dir")
            .arg(self.cache.path())
            .args(args)
            .env("ALSP_RUST_ANALYZER", &self.ra)
            .env("ALSP_TEST_FX", self.fx.path())
            .output()
            .unwrap()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const CARGO: &str = "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n";
const LIB1: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    1;\n}\n";
const LIB2: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    2;\n}\n";

fn two_commit_repo(e: &Env) -> (String, String) {
    write(e.dir(), "Cargo.toml", CARGO);
    write(e.dir(), "src/lib.rs", LIB1);
    let c1 = commit(e.dir(), "c1");
    write(e.dir(), "src/lib.rs", LIB2);
    let c2 = commit(e.dir(), "c2");
    (c1, c2)
}

#[test]
fn test_DW_2_5_no_rust_changes_is_an_empty_pack_exit_0_and_never_runs_the_indexer() {
    let e = Env::new();
    write(e.dir(), "README.md", "a\n");
    write(e.dir(), "Cargo.toml", CARGO);
    commit(e.dir(), "c1");
    write(e.dir(), "README.md", "b\n");
    commit(e.dir(), "c2");
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("index=skipped") && t.contains("symbols=0") && t.contains("! no Rust files changed"), "{t}");
    assert_eq!(e.runs(), 0);
    let j: serde_json::Value = serde_json::from_str(&out(&e.pack(&["--base", "HEAD~1", "--format", "json"]))).unwrap();
    assert_eq!(j["symbols"].as_array().unwrap().len(), 0);
    assert_eq!(j["index"]["status"], "skipped");
}

#[test]
fn modified_function_shows_callers_and_entry_points_in_text_and_json() {
    let e = Env::new();
    two_commit_repo(&e);
    e.serve("default", &[("src/lib.rs", LIB2)]);
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("index=missing+rebuilt roots=1/1 symbols=1"), "{t}");
    assert!(t.contains("M lib::helper src/lib.rs:4-6"), "{t}");
    assert!(t.contains("callers(1): lib::api src/lib.rs:2"), "{t}");
    assert!(t.contains("entry(1): lib::api src/lib.rs:1~"), "pub fn is a heuristic entry point: {t}");
    let j: serde_json::Value = serde_json::from_str(&out(&e.pack(&["--base", "HEAD~1", "--format", "json"]))).unwrap();
    let s = &j["symbols"][0];
    assert_eq!(s["kind"], "Modified");
    assert_eq!(s["callers"][0]["provenance"], "resolved");
    assert_eq!(s["entry_points"][0]["provenance"], "heuristic");
    assert_eq!(s["coverage"], serde_json::Value::Null);
}

#[test]
fn test_DW_2_3_second_pack_is_cached_and_an_edit_makes_it_stale_and_rebuilds() {
    let e = Env::new();
    two_commit_repo(&e);
    e.serve("default", &[("src/lib.rs", LIB2)]);
    let first = out(&e.pack(&["--base", "HEAD~1"]));
    assert!(first.contains("index=missing+rebuilt"), "{first}");
    assert_eq!(e.runs(), 1);
    let second = out(&e.pack(&["--base", "HEAD~1"]));
    assert!(second.contains("index=fresh ") && !second.contains("rebuilt"), "{second}");
    assert_eq!(e.runs(), 1, "the cached index is used; the indexer does not run");
    // Edit one source file in the working tree.
    let edited = LIB2.replace("2;", "3;");
    write(e.dir(), "src/lib.rs", &edited);
    e.serve("default", &[("src/lib.rs", &edited)]);
    let third = out(&e.pack(&["--base", "HEAD~1"]));
    assert!(third.contains("index=stale+rebuilt"), "{third}");
    assert!(third.contains("! the working tree has uncommitted changes to 1 Rust/Cargo file(s) (src/lib.rs)"), "{third}");
    assert_eq!(e.runs(), 2);
    // --no-build refuses stale facts: exit 3, the pack says why
    write(e.dir(), "src/lib.rs", &edited.replace("3;", "4;"));
    let o = e.pack(&["--base", "HEAD~1", "--no-build"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(out(&o).contains("index=stale") && out(&o).contains("symbols=0") && out(&o).contains("means unknown"), "{}", out(&o));
    assert!(err(&o).contains("no index could be served"));
    assert_eq!(e.runs(), 2, "--no-build never runs the indexer");
}

#[test]
fn test_DW_2_5_renamed_file_keeps_identity_and_reports_the_edit_only() {
    let e = Env::new();
    write(e.dir(), "Cargo.toml", CARGO);
    write(e.dir(), "src/old.rs", LIB1);
    commit(e.dir(), "c1");
    git(e.dir(), &["mv", "src/old.rs", "src/new.rs"]);
    write(e.dir(), "src/new.rs", LIB2);
    commit(e.dir(), "rename and edit");
    e.serve("default", &[("src/new.rs", LIB2)]);
    let t = out(&e.pack(&["--base", "HEAD~1"]));
    assert!(t.contains("symbols=1") && t.contains("M new::helper src/new.rs:4-6"), "{t}");
    assert!(!t.contains("\nA ") && !t.contains("\nR "), "a renamed, edited file adds and removes nothing: {t}");
}

#[test]
fn test_DW_2_5_hunk_outside_a_function_is_attributed_to_the_module() {
    let e = Env::new();
    write(e.dir(), "Cargo.toml", CARGO);
    write(e.dir(), "src/lib.rs", LIB1);
    commit(e.dir(), "c1");
    let with_use = format!("use std::fmt;\n{LIB1}");
    write(e.dir(), "src/lib.rs", &with_use);
    commit(e.dir(), "c2");
    e.serve("default", &[("src/lib.rs", &with_use)]);
    let t = out(&e.pack(&["--base", "HEAD~1"]));
    assert!(t.contains("symbols=1") && t.contains("M lib src/lib.rs:1-7"), "{t}");
}

#[test]
fn test_DW_2_5_one_crate_index_failure_keeps_the_others() {
    let e = Env::new();
    // no root manifest: `good` and `bad` are independent Cargo roots
    for d in ["good", "bad"] {
        write(e.dir(), &format!("{d}/Cargo.toml"), CARGO);
        write(e.dir(), &format!("{d}/src/lib.rs"), LIB1);
    }
    commit(e.dir(), "c1");
    for d in ["good", "bad"] {
        write(e.dir(), &format!("{d}/src/lib.rs"), LIB2);
    }
    commit(e.dir(), "c2");
    e.serve("good", &[("src/lib.rs", LIB2)]); // nothing served for `bad`: its indexer fails
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("roots=1/2"), "{t}");
    assert!(t.contains("Index: ") && t.contains("root bad failed: exit 1: cannot load bad"), "{t}");
    assert!(t.contains("M lib::helper good/src/lib.rs:4-6"), "good's symbols are still served: {t}");
    assert!(t.contains("bad/src/lib.rs") && t.contains("not in the index"), "bad's changed file is called out, not silently dropped: {t}");
}

#[test]
fn every_root_failing_exits_3_with_the_reasons() {
    let e = Env::new();
    two_commit_repo(&e); // fake indexer has nothing to serve
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(3));
    let t = out(&o);
    assert!(t.contains("roots=0/1") && t.contains("Index: ") && t.contains("root . failed: exit 1: cannot load") && t.contains("means unknown"), "{t}");
}

#[test]
fn head_that_is_not_checked_out_is_exported_and_indexed_as_a_snapshot() {
    let e = Env::new();
    let (c1, c2) = two_commit_repo(&e);
    write(e.dir(), "src/lib.rs", "pub fn api() {}\n");
    commit(e.dir(), "c3");
    e.serve("default", &[("src/lib.rs", LIB2)]); // what c2's tree looks like to the indexer
    let o = e.pack(&["--base", &c1, "--head", &c2]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains(&format!("pack {}..{}", &c1[..8], &c2[..8])), "{t}");
    assert!(t.contains(&format!("! {} is not checked out: symbols describe an exported snapshot", &c2[..8])), "{t}");
    assert!(t.contains("M lib::helper src/lib.rs:4-6") && t.contains("callers(1): lib::api"), "{t}");
    // the work tree is untouched by the export
    assert_eq!(fs::read_to_string(e.dir().join("src/lib.rs")).unwrap(), "pub fn api() {}\n");
    assert!(!e.dir().join(".alsp").exists());
}

#[test]
fn bad_revisions_and_non_repositories_exit_2_without_running_anything() {
    let e = Env::new();
    two_commit_repo(&e);
    for args in [vec!["--base", "no-such-rev"], vec!["--base=-p"], vec!["--base", "HEAD~1", "--head=--output=x"]] {
        let o = e.pack(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", err(&o));
        assert!(err(&o).starts_with("alsp: "), "{}", err(&o));
    }
    let o = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["pack", "--repo", "/definitely/not/here", "--base", "HEAD"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(e.runs(), 0);
}

#[test]
fn test_DW_1_1_pack_without_an_index_prints_the_index_line_and_json_carries_status_and_notes() {
    let e = Env::new();
    two_commit_repo(&e); // fake indexer has nothing to serve: it exits 1
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(3));
    let t = out(&o);
    assert!(t.contains("Index: missing+rebuilt, 0/1 roots indexed; root . failed: exit 1") && t.contains("unknown, not unchanged"), "{t}");
    let o = e.pack(&["--base", "HEAD~1", "--format", "json"]);
    assert_eq!(o.status.code(), Some(3));
    let j: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!((j["index"]["status"].as_str(), j["index"]["units"][0]["ok"].as_bool()), (Some("missing"), Some(false)));
    assert!(j["index"]["notes"].is_array() && j["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("unknown, not unchanged")), "{j}");
}

#[test]
fn test_DW_1_3_a_partial_index_exits_0_and_names_the_failed_unit() {
    let e = Env::new();
    for d in ["good", "bad"] {
        write(e.dir(), &format!("{d}/Cargo.toml"), CARGO);
        write(e.dir(), &format!("{d}/src/lib.rs"), LIB1);
    }
    commit(e.dir(), "c1");
    for d in ["good", "bad"] {
        write(e.dir(), &format!("{d}/src/lib.rs"), LIB2);
    }
    commit(e.dir(), "c2");
    e.serve("good", &[("src/lib.rs", LIB2)]);
    let o = e.pack(&["--base", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("Index: missing+rebuilt, 1/2 roots indexed; root bad failed:"), "{}", out(&o));
}

#[test]
fn test_DW_1_5_version_prints_the_crate_version_and_exits_0() {
    for flag in ["--version", "-V"] {
        let o = Command::new(env!("CARGO_BIN_EXE_alsp")).arg(flag).output().unwrap();
        assert_eq!(o.status.code(), Some(0));
        assert_eq!(out(&o).trim(), format!("alsp {}", env!("CARGO_PKG_VERSION")));
    }
}
