//! `alsp sym|refs|tests|cover|gaps` end to end, offline: a real git repository, a fake
//! rust-analyzer (SCIP from alsp-evidence's testkit), a fake cargo for coverage and mutant
//! listing (alsp-coverage's testkit), and a fake sdd `decide` command (alsp_decide::stub::FakeSdd).
#![allow(non_snake_case)]

use alsp_coverage::testkit::{install_fake_cargo, lcov, metadata_json};
use alsp_decide::stub::FakeSdd;
use alsp_evidence::testkit::scip_for;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// api 1-3, helper 4-6, a comment on 7, leaf 8-10.
const LIB: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    leaf();\n}\n// between items\nfn leaf() -> u32 {\n    1\n}\n";
const IT: &str = "#[test]\nfn t_api() {\n    api();\n}\n#[test]\nfn t_leaf() {\n    leaf();\n}\n";

struct Env {
    repo: PathBuf,
    _root: tempfile::TempDir,
    fx: PathBuf,
    cache: PathBuf,
    cargo: PathBuf,
    ra: PathBuf,
    config_home: PathBuf,
}

impl Env {
    fn new(lib: &str, extra: &[(&str, &str)]) -> Env {
        let root = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let (repo, fx, cache, config_home) = (base.join("repo"), base.join("fx"), base.join("cache"), base.join("config"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&config_home).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        write(&repo, "Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n");
        write(&repo, "src/lib.rs", lib);
        write(&repo, "tests/it.rs", IT);
        for (p, c) in extra {
            write(&repo, p, c);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c1"]);
        let cargo = install_fake_cargo(&fx);
        fs::write(fx.join("metadata.json"), metadata_json(&repo)).unwrap();
        fs::write(fx.join("out.lcov"), lcov(&[("src/lib.rs", &[(2, 1), (5, 1), (8, 4), (9, 0), (10, 0)], &[(8, &[true, false])])])).unwrap();
        let mj = |l: u32, c: u32, genre: &str, d: &str| {
            format!(r#"{{"file":"src/lib.rs","package":"fx","genre":"{genre}","name":"src/lib.rs:{l}:{c}: {d}","replacement":"R","span":{{"start":{{"line":{l},"column":{c}}},"end":{{"line":{l},"column":{}}}}},"diff":"D"}}"#, c + 1)
        };
        fs::write(fx.join("mutants.json"), format!("[{}]", [mj(8, 4, "BinaryOperator", "replace a with b in leaf"), mj(9, 5, "FnValue", "replace 1 with 0 in leaf")].join(","))).unwrap();
        let ra = fx.join("ra.sh");
        fs::write(&ra, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\necho run >> \"$ALSP_FX/ra.log\"\ncp \"$ALSP_FX/default.scip\" \"$4\"\n").unwrap();
        fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
        let mut files = vec![("src/lib.rs", lib), ("tests/it.rs", IT)];
        files.extend_from_slice(extra);
        fs::write(fx.join("default.scip"), scip_for(&files)).unwrap();
        Env { repo, _root: root, fx, cache, cargo, ra, config_home }
    }

    fn run(&self, args: &[&str], sdd: Option<&FakeSdd>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.args(args).arg("--repo").arg(&self.repo).arg("--cache-dir").arg(&self.cache);
        c.env("ALSP_RUST_ANALYZER", &self.ra).env("ALSP_CARGO", &self.cargo).env("ALSP_FX", &self.fx).env("ALSP_TREE", &self.repo);
        // the real sdd must never be reached by a test: no fake means ALSP_SDD_ROOT points at nothing
        match sdd {
            Some(f) => c.env("ALSP_SDD_ROOT", &f.root).env("ALSP_BUN", &f.bun),
            None => c.env("ALSP_SDD_ROOT", self.config_home.join("no-sdd")).env("ALSP_BUN", self.config_home.join("no-bun")),
        };
        c.output().unwrap()
    }

    fn ra_runs(&self) -> usize {
        fs::read_to_string(self.fx.join("ra.log")).map(|s| s.lines().count()).unwrap_or(0)
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn json(o: &Output) -> serde_json::Value {
    serde_json::from_str(&out(o)).unwrap_or_else(|e| panic!("{e}: {}", out(o)))
}

#[test]
fn test_DW_4_4_no_arguments_and_help_agents_print_the_same_short_cheat_sheet() {
    let bare = Command::new(env!("CARGO_BIN_EXE_alsp")).output().unwrap();
    assert_eq!(bare.status.code(), Some(0), "{}", err(&bare));
    let sheet = out(&bare);
    assert!(sheet.lines().count() < 40, "{} lines", sheet.lines().count());
    assert!(sheet.contains("alsp refs <target>") && sheet.contains("Exit codes:") && sheet.contains("docs/query.md"), "{sheet}");
    let help = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["help", "agents"]).output().unwrap();
    assert_eq!((help.status.code(), out(&help)), (Some(0), sheet));
    let bad = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["help", "nope"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(err(&bad).contains("alsp help agents"), "{}", err(&bad));
    let refs = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["help", "refs"]).output().unwrap();
    assert!(out(&refs).contains("--json") && out(&refs).contains("--all"));
    let top = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["help"]).output().unwrap();
    assert!(out(&top).contains("sym") && out(&top).contains("gaps"), "{}", out(&top));
}

#[test]
fn sym_answers_with_definition_kind_and_reference_count_and_reuses_the_cached_index() {
    let e = Env::new(LIB, &[]);
    let o = e.run(&["sym", "leaf"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.starts_with("sym index=missing+rebuilt @"), "{t}");
    assert!(t.contains("matches(1) [resolved]\n  fn lib::leaf src/lib.rs:8 refs=2\n"), "{t}");
    assert_eq!(e.ra_runs(), 1);
    let again = out(&e.run(&["sym", "leaf"], None));
    assert!(again.starts_with("sym index=fresh @"), "{again}");
    assert_eq!(e.ra_runs(), 1, "the cached index is used");
}

#[test]
fn test_DW_4_2_text_marks_provenance_on_every_section() {
    let e = Env::new(LIB, &[]);
    for args in [&["sym", "leaf"][..], &["refs", "leaf"], &["tests", "leaf", "--run"], &["cover", "leaf"]] {
        let o = e.run(args, None);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", err(&o));
        let t = out(&o);
        for line in t.lines().skip(1).filter(|l| !l.starts_with("  ") && !l.starts_with('!')) {
            let tag = line.rsplit_once('[').map(|(_, r)| r.trim_end_matches(']')).unwrap_or("");
            assert!(["resolved", "observed", "predicted", "heuristic", "resolved+heuristic"].contains(&tag), "{args:?}: no provenance on `{line}` in\n{t}");
        }
    }
    let refs = out(&e.run(&["refs", "leaf"], None));
    assert!(refs.contains("refs(2 in 2 symbols) [resolved]\n  lib::helper src/lib.rs:5\n  t_leaf tests/it.rs:7 [test]\n"), "{refs}");
    assert!(refs.contains("entry(3) [resolved+heuristic]\n  test t_leaf tests/it.rs:6\n  pub lib::api src/lib.rs:1~\n  test t_api tests/it.rs:2\n"), "{refs}");
    let g = out(&e.run(&["gaps", "leaf"], None));
    assert!(g.contains("[observed]") || g.contains("[predicted]"), "{g}");
}

#[test]
fn json_has_the_documented_schema_with_provenance_on_each_fact() {
    let e = Env::new(LIB, &[]);
    let j = json(&e.run(&["refs", "leaf", "--json"], None));
    assert_eq!((j["schema"].clone(), j["command"].clone()), (1.into(), "refs".into()));
    assert_eq!(j["target"]["kind"], "fn");
    assert_eq!(j["index"]["status"], "missing");
    assert_eq!(j["refs"]["provenance"], "resolved");
    assert_eq!(j["refs"]["items"][1]["test"], true);
    assert_eq!(j["entry_points"]["items"][1]["provenance"], "heuristic");
    assert_eq!(j["reference_count"], 2);
    assert!(j.get("coverage").is_none() && j.get("gaps").is_none());
    let c = json(&e.run(&["cover", "leaf", "--json"], None));
    assert_eq!((c["coverage"]["provenance"].clone(), c["coverage"]["lines_hit"].clone(), c["coverage"]["uncovered_lines"].clone()), ("observed".into(), 1.into(), serde_json::json!([9, 10])));
}

#[test]
fn test_DW_4_3_an_ambiguous_name_lists_candidates_and_exits_4_in_text_and_json() {
    let e = Env::new(LIB, &[("src/other.rs", "fn leaf() {}\n")]);
    let o = e.run(&["refs", "leaf"], None);
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("`leaf` is ambiguous: 2 candidates\n  fn lib::leaf src/lib.rs:8\n  fn other::leaf src/other.rs:1\n"), "{t}");
    assert!(err(&o).contains("repeat the command with a longer name such as `lib::leaf`"), "{}", err(&o));
    let j = json(&e.run(&["refs", "leaf", "--json"], None));
    assert_eq!((j["error"].clone(), j["candidates"].as_array().unwrap().len()), ("ambiguous".into(), 2));
    // the exact name picks one; `sym` lists both and succeeds
    assert_eq!(e.run(&["refs", "lib::leaf"], None).status.code(), Some(0));
    let s = e.run(&["sym", "leaf"], None);
    assert_eq!(s.status.code(), Some(0));
    assert!(out(&s).contains("matches(2) [resolved]"), "{}", out(&s));
}

#[test]
fn test_DW_4_3_no_match_exits_4_with_the_nearest_names() {
    let e = Env::new(LIB, &[]);
    for cmd in ["sym", "refs", "tests", "cover", "gaps"] {
        let o = e.run(&[cmd, "lef"], None);
        assert_eq!(o.status.code(), Some(4), "{cmd}: {}", err(&o));
        assert!(out(&o).contains("no match for `lef`\nnearest:\n  lib::leaf\n"), "{cmd}: {}", out(&o));
        assert!(err(&o).contains("alsp sym <part of the name>"), "{}", err(&o));
    }
    let j = json(&e.run(&["sym", "lef", "--json"], None));
    assert_eq!((j["error"].clone(), j["nearest"][0].clone()), ("no_match".into(), "lib::leaf".into()));
    let missing_file = e.run(&["refs", "nope.rs:3"], None);
    assert_eq!(missing_file.status.code(), Some(4));
}

#[test]
fn test_DW_4_3_a_line_outside_any_symbol_answers_for_the_file() {
    let e = Env::new(LIB, &[]);
    let o = e.run(&["refs", "src/lib.rs:7"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("target file src/lib.rs:1-end [resolved]"), "{t}");
    assert!(t.contains("refs(2 in 2 symbols) [resolved]\n  t_api tests/it.rs:3 [test]\n  t_leaf tests/it.rs:7 [test]\n"), "{t}");
    // inside a symbol the same file answers for the symbol
    assert!(out(&e.run(&["refs", "src/lib.rs:9"], None)).contains("target fn src/lib.rs:8-10"));
}

#[test]
fn test_DW_4_3_an_unavailable_index_exits_3_and_says_what_to_do() {
    let e = Env::new(LIB, &[]);
    let o = e.run(&["refs", "leaf", "--no-build"], None);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("no index could be served") && err(&o).contains("rerun without --no-build"), "{}", err(&o));
    assert_eq!(e.ra_runs(), 0, "--no-build never runs the indexer");
    // a built index serves --no-build while it is fresh, then a source edit makes it unavailable
    assert_eq!(e.run(&["refs", "leaf"], None).status.code(), Some(0));
    assert_eq!(e.run(&["refs", "leaf", "--no-build"], None).status.code(), Some(0));
    write(&e.repo, "src/lib.rs", &format!("{LIB}// edit\n"));
    let o = e.run(&["refs", "leaf", "--no-build"], None);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("index stale"), "{}", err(&o));
    // an indexer that fails everywhere is also exit 3
    let broken = Env::new(LIB, &[]);
    fs::write(&broken.ra, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\necho boom >&2; exit 1\n").unwrap();
    let o = broken.run(&["sym", "leaf"], None);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("rust-analyzer is installed"), "{}", err(&o));
}

#[test]
fn outside_a_repository_or_a_rust_project_the_error_says_what_to_do() {
    let d = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["sym", "x", "--repo"]).arg(d.path()).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("pass --repo <path>"), "{}", err(&o));
    let repo = d.path().join("r");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, "README.md", "x\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    let o = Command::new(env!("CARGO_BIN_EXE_alsp")).args(["sym", "x", "--cache-dir"]).arg(d.path().join("c")).arg("--repo").arg(&repo).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("point --repo (or --root) at a Rust package"), "{}", err(&o));
}

#[test]
fn a_package_outside_the_root_workspace_is_explained_and_indexed_with_root() {
    let e = Env::new(LIB, &[]);
    write(&e.repo, "apps/x/Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.0.1\"\n\n[workspace]\n");
    write(&e.repo, "apps/x/src/lib.rs", "pub fn xfn() {}\n");
    git(&e.repo, &["add", "-A"]);
    git(&e.repo, &["commit", "-q", "-m", "c2"]);
    fs::write(e.fx.join("x.scip"), scip_for(&[("src/lib.rs", "pub fn xfn() {}\n")])).unwrap();
    fs::write(&e.ra, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\nname=$(basename \"$2\")\nif [ -f \"$ALSP_FX/$name.scip\" ]; then cp \"$ALSP_FX/$name.scip\" \"$4\"; else cp \"$ALSP_FX/default.scip\" \"$4\"; fi\n").unwrap();
    let o = e.run(&["sym", "apps/x/src/lib.rs:1"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("! apps/x/src/lib.rs is in the Cargo package apps/x, which the index does not cover") && out(&o).contains("rerun with --root apps/x"), "{}", out(&o));
    assert_eq!(e.run(&["sym", "xfn"], None).status.code(), Some(4));
    let o = e.run(&["sym", "xfn", "--root", "apps/x"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("matches(1) [resolved]\n  fn lib::xfn apps/x/src/lib.rs:1 refs=0\n"), "{}", out(&o));
    let o = e.run(&["sym", "xfn", "--root", "apps/nope"], None);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("no Cargo.toml found in") && err(&o).contains("--root"), "{}", err(&o));
    // a file with no items inside an indexed crate gets the generic reason
    write(&e.repo, "src/notes.rs", "// nothing\n");
    assert!(out(&e.run(&["sym", "src/notes.rs:1"], None)).contains("the index holds no symbols for src/notes.rs"));
}

#[test]
fn test_DW_4_3_the_result_cap_counts_the_rest_and_all_lifts_it() {
    let callers: String = (0..30).map(|i| format!("fn c{i}() {{\n    leaf();\n}}\n")).collect();
    let e = Env::new(&format!("{LIB}{callers}"), &[]);
    let t = out(&e.run(&["refs", "leaf"], None));
    assert!(t.contains("refs(32 in 32 symbols) [resolved]"), "{t}");
    assert_eq!(t.lines().filter(|l| l.starts_with("  lib::c") || l.starts_with("  lib::helper") || l.starts_with("  t_leaf")).count(), 25, "{t}");
    assert!(t.contains("  +7 more (--all lists everything)"), "{t}");
    let all = out(&e.run(&["refs", "leaf", "--all"], None));
    assert!(!all.contains("more (--all") && all.contains("lib::c29 "), "{all}");
    let j = json(&e.run(&["refs", "leaf", "--json"], None));
    assert_eq!((j["refs"]["items"].as_array().unwrap().len(), j["refs"]["omitted"].clone(), j["refs"]["total"].clone()), (25, 7.into(), 32.into()));
    assert_eq!(json(&e.run(&["refs", "leaf", "--json", "--all"], None))["refs"]["omitted"], 0);
}

#[test]
fn an_edited_working_tree_is_reported_next_to_the_answer() {
    let e = Env::new(LIB, &[]);
    assert!(!out(&e.run(&["sym", "leaf"], None)).contains("uncommitted"));
    write(&e.repo, "src/lib.rs", &format!("{LIB}// edit\n"));
    let o = e.run(&["sym", "leaf"], None);
    let t = out(&o);
    assert!(t.contains("index=stale+rebuilt") && t.contains("! the working tree has uncommitted changes to 1 Rust/Cargo file(s) (src/lib.rs); the index describes the working tree, not"), "{t}");
    assert!(t.contains("! the cached index was built from older sources; it was rebuilt for this query"), "{t}");
    let j = json(&e.run(&["sym", "leaf", "--json"], None));
    assert_eq!(j["index"]["dirty"], serde_json::json!(["src/lib.rs"]));
}

#[test]
fn tests_lists_direct_and_indirect_tests_and_run_reports_execution() {
    let e = Env::new(LIB, &[]);
    let t = out(&e.run(&["tests", "leaf"], None));
    assert!(t.contains("tests(2) [resolved]\n  t_leaf tests/it.rs:6 direct\n  t_api tests/it.rs:2 indirect\n"), "{t}");
    assert!(!t.contains("executed:"));
    let o = e.run(&["tests", "leaf", "--run"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("executed: yes (1/3 lines ran) [observed]"), "{}", out(&o));
    assert!(err(&o).contains("alsp: coverage fx: measured in"), "{}", err(&o));
}

#[test]
fn cover_reports_coverage_and_exits_5_when_the_tests_cannot_run() {
    let e = Env::new(LIB, &[]);
    let o = e.run(&["cover", "leaf"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("lines 1/3 covered, branches 1/2 [observed]\nuncovered: 9-10 [observed]\n"), "{}", out(&o));
    fs::write(e.fx.join("fail"), "").unwrap();
    let broken = Env::new(LIB, &[]);
    fs::write(broken.fx.join("fail"), "").unwrap();
    let o = broken.run(&["cover", "leaf"], None);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(out(&o).contains("! coverage unavailable for package fx") && out(&o).contains("! unavailable: coverage could not be measured"), "{}", out(&o));
    assert!(err(&o).contains("fix the cause and rerun"), "{}", err(&o));
}

#[test]
fn gaps_lists_predicted_and_unrun_mutants_through_the_decision_backend_and_never_runs_mutants() {
    let e = Env::new(LIB, &[]);
    let stub = { let f = FakeSdd::new("ok"); f.p(0.2); f.tokens(1500); f };
    let o = e.run(&["gaps", "leaf"], Some(&stub));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("gaps(2) [predicted+observed]\n  src/lib.rs:8 replace a with b in leaf (col 4) p=0.20 [predicted]\n  src/lib.rs:9 replace 1 with 0 in leaf (col 5) uncovered [observed]\n"), "{t}");
    assert_eq!((stub.calls(), stub.items_sent()), (1, 1), "one sdd job with the one covered mutant");
    assert!(stub.request(1).contains("fn t_leaf()"), "the reaching test was sent");
    assert!(err(&o).contains("1500 input tokens"), "{}", err(&o));
    let calls = fs::read_to_string(e.fx.join("calls.log")).unwrap();
    assert!(calls.lines().filter(|c| c.contains("mutants")).all(|c| c.contains("--list")), "{calls}");
}

#[test]
fn gaps_without_sdd_keeps_the_unrun_gaps_and_exits_5() {
    let e = Env::new(LIB, &[]);
    let o = e.run(&["gaps", "leaf"], None);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("uncovered [observed]") && t.contains("! predicted gaps unavailable: "), "{t}");
    let bad = e.run(&["gaps", "leaf", "--gap-threshold", "2"], None);
    assert_eq!(bad.status.code(), Some(2));
}

/// Every key the commands emit is named in docs/query.md, so the documented schema cannot drift.
#[test]
fn test_DW_4_2_every_json_key_is_documented() {
    let docs = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/query.md")).expect("docs/query.md exists");
    let e = Env::new(LIB, &[("src/other.rs", "fn leaf() {}\n")]);
    let stub = { let f = FakeSdd::new("ok"); f.p(0.2); f.tokens(10); f };
    let mut seen = std::collections::BTreeSet::new();
    fn keys(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
        match v {
            serde_json::Value::Object(m) => m.iter().for_each(|(k, v)| {
                out.insert(k.clone());
                keys(v, out);
            }),
            serde_json::Value::Array(a) => a.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }
    let runs: Vec<(Vec<&str>, Option<&FakeSdd>)> = vec![
        (vec!["sym", "lib::leaf", "--json"], None),
        (vec!["refs", "lib::leaf", "--json"], None),
        (vec!["tests", "lib::leaf", "--run", "--json"], None),
        (vec!["cover", "lib::leaf", "--json"], None),
        (vec!["gaps", "lib::leaf", "--json"], Some(&stub)),
        (vec!["refs", "leaf", "--json"], None),
        (vec!["sym", "lef", "--json"], None),
    ];
    for (args, sdd) in runs {
        keys(&json(&e.run(&args, sdd)), &mut seen);
    }
    assert!(seen.len() > 30, "{seen:?}");
    let missing: Vec<&String> = seen.iter().filter(|k| !docs.contains(&format!("`{k}`"))).collect();
    assert!(missing.is_empty(), "undocumented JSON keys: {missing:?}");
}
