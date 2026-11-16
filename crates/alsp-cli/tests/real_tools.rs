//! Real-tool checks on a small made-up crate: rust-analyzer, cargo-llvm-cov and `cargo mutants
//! --list` against hand-written expectations. All `#[ignore]`d: they need those three tools on
//! PATH and take a while.
//!
//! Run: `cargo test -p alsp-cli --test real_tools -- --ignored --test-threads=1 --nocapture`
//! cargo-mutants is only ever run with `--list`.
#![allow(non_snake_case)]

use alsp_coverage::{observe, CoverageTool};
use alsp_evidence::{build_pack_with_source, ChangeKind, EvidencePack, PackRequest, PackSource};
use alsp_gaps::{list_mutants, owner_of, MutantsTool};
use alsp_index::{BuildPolicy, Tool};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

const BASE_LIB: &str = "\
pub fn double(x: i32) -> i32 {
    x * 2
}

pub fn quad(x: i32) -> i32 {
    double(double(x))
}

pub fn sign(x: i32) -> i32 {
    if x > 0 { 1 } else { -1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_of_one_is_four() {
        assert_eq!(quad(1), 4);
    }
}
";

/// `double` and `sign` change, `triple` is new, `quad` is untouched.
const HEAD_LIB: &str = "\
pub fn double(x: i32) -> i32 {
    x + x
}

pub fn quad(x: i32) -> i32 {
    double(double(x))
}

pub fn triple(x: i32) -> i32 {
    double(x) + x
}

pub fn sign(x: i32) -> i32 {
    if x >= 0 { 1 } else { -1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_of_one_is_four() {
        assert_eq!(quad(1), 4);
    }
}
";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn demo_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("Cargo.toml"), MANIFEST).unwrap();
    std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(p.join("src/lib.rs"), BASE_LIB).unwrap();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "base"]);
    std::fs::write(p.join("src/lib.rs"), HEAD_LIB).unwrap();
    git(p, &["commit", "-q", "-am", "head"]);
    dir
}

fn pack_for(repo: &Path, cache: &Path) -> (EvidencePack, PackSource) {
    let req = PackRequest { repo: repo.into(), base: "HEAD~1".into(), head: "HEAD".into(), policy: BuildPolicy::Build, cache_root: Some(cache.into()), tool: Tool::default() };
    build_pack_with_source(&req).expect("pack")
}

fn fn_symbols(p: &EvidencePack) -> Vec<&alsp_evidence::SymbolEvidence> {
    p.symbols.iter().filter(|s| s.id.ends_with("().") && s.kind != ChangeKind::Removed).collect()
}

#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn real_rust_analyzer_finds_changed_functions_and_their_callers() {
    let (repo, cache) = (demo_repo(), tempfile::tempdir().unwrap());
    let (pack, _) = pack_for(repo.path(), cache.path());
    let changed: BTreeSet<(char, String)> = fn_symbols(&pack).iter().map(|s| (s.kind.letter(), s.name.clone())).collect();
    let want: BTreeSet<(char, String)> = [('M', "double"), ('A', "triple"), ('M', "sign")].iter().map(|(k, n)| (*k, n.to_string())).collect();
    assert_eq!(changed, want, "exactly the hand-written changed functions");

    // Hand-written from the source: `double` is called by `quad` (twice) and `triple`; nothing calls `triple` or `sign`.
    let callers = |name: &str| -> BTreeSet<String> {
        let s = fn_symbols(&pack).into_iter().find(|s| s.name == name).unwrap();
        s.callers.iter().map(|e| alsp_index::readable_name(&e.id).to_string()).collect()
    };
    assert_eq!(callers("double"), ["quad".to_string(), "triple".to_string()].into());
    assert!(callers("triple").is_empty() && callers("sign").is_empty());
}

#[test]
#[ignore = "needs rust-analyzer and cargo-llvm-cov on PATH"]
fn real_llvm_cov_counts_hit_and_missed_lines_per_symbol() {
    let (repo, cache) = (demo_repo(), tempfile::tempdir().unwrap());
    let (mut pack, src) = pack_for(repo.path(), cache.path());
    let obs = observe(&pack, &src, &CoverageTool::default());
    assert!(obs.runs().iter().all(|r| r.error.is_none()), "{:?}", obs.runs());
    obs.attach(&mut pack, &src);
    let cov = |name: &str| fn_symbols(&pack).into_iter().find(|s| s.name == name).unwrap().coverage.clone().unwrap_or_else(|| panic!("{name} has no coverage"));
    // The test calls quad(1): `double` runs fully. `triple` and `sign` are never called.
    let d = cov("double");
    assert!(d.lines_total > 0 && d.lines_hit == d.lines_total, "double: {d:?}");
    for name in ["triple", "sign"] {
        let c = cov(name);
        assert!(c.lines_total > 0 && c.lines_hit == 0, "{name}: {c:?}");
    }
}

#[test]
#[ignore = "needs rust-analyzer and cargo-mutants on PATH"]
fn real_cargo_mutants_list_lands_each_mutant_in_one_symbol() {
    let (repo, cache) = (demo_repo(), tempfile::tempdir().unwrap());
    let (pack, src) = pack_for(repo.path(), cache.path());
    let fns: Vec<(&str, alsp_index::Span)> = fn_symbols(&pack).iter().map(|s| (s.file.as_str(), s.span)).collect();
    assert_eq!(fns.len(), 3);
    let (mutants, malformed) = list_mutants(&MutantsTool::default(), &src.tree, &["src/lib.rs"], &src.build_env).expect("cargo mutants --list");
    assert_eq!(malformed, 0);
    assert!(!mutants.is_empty());
    for m in &mutants {
        let holders: Vec<usize> = (0..fns.len()).filter(|&i| fns[i].1.start <= m.start.0 && m.end.0 <= fns[i].1.end).collect();
        match owner_of(&fns, &m.file, (m.start.0, m.end.0)) {
            Some(o) => assert!(holders.contains(&o), "{:?}", m.description),
            None => assert!(holders.is_empty(), "{:?} is held by a symbol but joined to none", m.description),
        }
    }
    // `double` and `sign` and `triple` each hold at least one mutant (return-value replacements).
    let owned: BTreeSet<usize> = mutants.iter().filter_map(|m| owner_of(&fns, &m.file, (m.start.0, m.end.0))).collect();
    assert_eq!(owned.len(), 3, "every changed function owns a mutant: {owned:?}");
}

/// Trait/impl links against what rust-analyzer really writes: calls on a concrete type resolve to
/// the impl's method, calls on `dyn T` to the trait's. The fixture is `testkit::TRAIT_LIB`.
#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn real_rust_analyzer_links_trait_and_impl_methods() {
    use alsp_query::{OpenOptions, Workspace};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("Cargo.toml"), "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(p.join("src/lib.rs"), alsp_evidence::testkit::TRAIT_LIB).unwrap();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "c"]);
    let cache = tempfile::tempdir().unwrap();
    let ws = Workspace::open(&OpenOptions { repo: p.into(), no_build: false, cache_root: Some(cache.path().into()), tool: Tool::default(), extra_roots: vec![] }).expect("opens");
    let sites = |q: &str| -> BTreeSet<u32> { ws.refs(q).unwrap().refs.unwrap().items.iter().flat_map(|g| g.lines.clone()).collect() };
    // hand-written from TRAIT_LIB: x.m() 17, a.m() 20, b.m() 23, A.m() 30
    assert_eq!(sites("T::m"), [17, 20, 23, 30].into(), "the three calls on concrete types are found through the impl links");
    assert_eq!(sites("<A as T>::m"), [17, 20, 30].into(), "A's direct calls plus the dyn call, not B's");
    assert_eq!(sites("<B as T>::m"), [17, 23].into());
    let tests = ws.tests("T::m", false).unwrap();
    let names: BTreeSet<String> = tests.tests.unwrap().items.iter().map(|t| t.name.clone()).collect();
    assert_eq!(names, ["t_direct_a".to_string(), "t_dyn".to_string()].into());
    // Display is not defined here: no links, and format!'s call to std's fmt is nobody's site
    let fmt = ws.refs("<A as Display>::fmt").unwrap();
    assert!(fmt.refs.unwrap().items.iter().all(|g| !g.lines.contains(&26)) && fmt.reference_count == Some(0), "{:?}", fmt.notes);
}
