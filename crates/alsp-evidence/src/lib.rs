//! `alsp-evidence`: turns a git diff into an evidence pack.
//!
//! [`build_pack`] is the whole interface: give it a repository and two revisions and it returns
//! the changed symbols with their type-resolved callers and the entry points that reach them, each
//! fact labelled with how it was obtained. Everything else (diff parsing, syntax reading, the
//! index, the call-graph walk) is hidden behind it.
//!
//! Which tree the symbols describe: the index describes one tree. When `head` is the commit that is
//! checked out, that is the working tree (uncommitted Rust changes are reported in the pack's
//! notes). Otherwise `head`'s tree is exported to the cache and indexed there, so the answer is
//! exact for any `head` without touching the repository.

mod changed;
mod diff;
mod git;
mod graph;
mod render;
mod syntax;
mod types;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use changed::Sources;
pub use graph::{Graph, Reach};
pub use render::{approx_tokens, escape, CALLERS_SHOWN, ENTRY_POINTS_SHOWN, GAPS_SHOWN};
pub use types::*;

use alsp_index::{cache_dir_for, default_cache_root, open_index, BuildPolicy, IndexRequest, IndexState, SymbolIndex, Tool};
pub use git::{command as git_command, Git, MAX_UNTRACKED_BYTES};
use std::path::{Path, PathBuf};

pub struct PackRequest {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub policy: BuildPolicy,
    /// Parent of the cache directories; `alsp_index::default_cache_root()` when `None`.
    pub cache_root: Option<PathBuf>,
    pub tool: Tool,
}

#[derive(Debug)]
pub enum PackError {
    Git(String),
    Index(alsp_index::IndexError),
    Io(String),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::Git(m) | PackError::Io(m) => write!(f, "{m}"),
            PackError::Index(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PackError {}

struct RepoSources<'a> {
    git: &'a Git,
    base: &'a str,
    tree: &'a Path,
}

impl Sources for RepoSources<'_> {
    fn head(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.tree.join(path)).ok()
    }
    fn base(&self, path: &str) -> Option<String> {
        self.git.show(self.base, path)
    }
}

/// Where the pack's facts came from, for the steps that run or read things after the pack is built
/// (coverage, mutant gaps). They fill the pack's `coverage` and `gaps`; they never rebuild it.
#[derive(Clone, Debug)]
pub struct PackSource {
    /// The tree the symbols describe: the working tree, or the exported snapshot of `head`.
    pub tree: PathBuf,
    /// This tree's own cache directory (parent of the index); steps keep their caches below it.
    pub cache_dir: PathBuf,
    /// Environment for cargo runs on `tree`, always with `CARGO_TARGET_DIR` set (the process's own
    /// value, else a build directory in the cache) so measuring never writes into the repository.
    pub build_env: Vec<(String, String)>,
    /// File -> head-side lines the diff added or changed, sorted.
    pub changed_lines: std::collections::BTreeMap<String, Vec<u32>>,
}

impl PackSource {
    pub fn is_changed(&self, file: &str, line: u32) -> bool {
        self.changed_lines.get(file).is_some_and(|l| l.binary_search(&line).is_ok())
    }
}

fn changed_lines_of(diffs: &[diff::FileDiff]) -> std::collections::BTreeMap<String, Vec<u32>> {
    let mut out: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
    for d in diffs {
        if let Some(p) = &d.new_path {
            let v = out.entry(p.clone()).or_default();
            v.extend(d.hunks.iter().flat_map(|h| h.new_lines()));
            v.sort_unstable();
            v.dedup();
        }
    }
    out
}

/// The text of the `#[test]` function whose name is on `name_line` of `file_text` (attributes and
/// doc comments included), or `None` when that line names no test function or the file does not
/// parse. The index says which function an entry point is; this says what its body is.
pub fn test_fn_text(file_text: &str, name_line: u32) -> Option<String> {
    let syn = syntax::analyze(file_text)?;
    let item = syn.item_at_name_line(name_line).filter(|i| i.is_test)?;
    let lines: Vec<&str> = file_text.lines().collect();
    lines.get(item.start as usize - 1..(item.end as usize).min(lines.len())).map(|l| l.join("\n"))
}

/// Symbols -> evidence, over an opened index. No git, no filesystem beyond `src`.
fn assemble(base: String, head: String, diffs: &[diff::FileDiff], index: IndexState, idx: Option<&SymbolIndex>, src: &dyn Sources) -> EvidencePack {
    let mut pack = EvidencePack { base, head, index, symbols: Vec::new(), notes: Vec::new() };
    let Some(idx) = idx else {
        pack.notes.push("no index could be served, so no symbols are reported (an empty list here means unknown, not unchanged)".into());
        return pack;
    };
    let changes = changed::changed_symbols(diffs, idx, src);
    pack.notes.extend(changes.notes);
    let mut graph = graph::Graph::new(idx, src);
    let mut trait_impls = Vec::new();
    let mut ambiguous = Vec::new();
    let mut truncated = Vec::new();
    for c in changes.symbols {
        let (callers, entry_points) = match &c.def {
            Some(d) if d.kind != alsp_index::SymKind::Module => {
                let reach = graph.entry_points(d);
                if reach.truncated {
                    truncated.push(c.name.clone());
                }
                if d.is_ambiguous() {
                    ambiguous.push(format!("{} ({} unattributed)", c.name, idx.unattributed_refs(d)));
                }
                if d.kind.is_fn() && c.name.contains(" as ") {
                    trait_impls.push(c.name.clone());
                }
                (graph.callers(&d.key), reach.entry_points)
            }
            _ => (Vec::new(), Vec::new()),
        };
        pack.symbols.push(SymbolEvidence {
            id: c.id,
            name: c.name,
            file: c.file,
            span: c.span,
            kind: c.kind,
            callers,
            entry_points,
            coverage: None,
            gaps: Vec::new(),
        });
    }
    pack.symbols.sort_by(|a, b| (&a.file, a.span.start, &a.id).cmp(&(&b.file, b.span.start, &b.id)));
    if !truncated.is_empty() {
        pack.notes.push(format!(
            "entry-point search hit its limit (depth {}, {} symbols, {} results) for: {}",
            graph::MAX_DEPTH,
            graph::MAX_VISITED,
            graph::MAX_ENTRY_POINTS,
            truncated.join(", ")
        ));
    }
    if !trait_impls.is_empty() {
        pack.notes.push(format!("trait impl methods: calls made through the trait are not attributed to the impl: {}", trait_impls.join(", ")));
    }
    if !ambiguous.is_empty() {
        ambiguous.sort();
        ambiguous.dedup();
        pack.notes.push(format!(
            "the index gives these the same symbol id as items in other crate roots (tests/*.rs, bins), so only same-file references count as callers, and references from files defining none of the copies are left out (callers may be incomplete by that count): {}",
            ambiguous.join(", ")
        ));
    }
    if pack.symbols.iter().any(|s| s.kind == ChangeKind::Removed) {
        pack.notes.push("removed symbols come from reading the base file's syntax (heuristic); they have no callers here".into());
    }
    pack
}

pub fn build_pack(req: &PackRequest) -> Result<EvidencePack, PackError> {
    build_pack_with_source(req).map(|(pack, _)| pack)
}

/// [`build_pack`], also returning where its facts came from.
pub fn build_pack_with_source(req: &PackRequest) -> Result<(EvidencePack, PackSource), PackError> {
    let git = Git::open(&req.repo).map_err(PackError::Git)?;
    let base = git.resolve(&req.base).map_err(PackError::Git)?;
    let head = git.resolve(&req.head).map_err(PackError::Git)?;
    let diffs = diff::parse_diff(&git.diff(&base, &head).map_err(PackError::Git)?);
    if !diffs.iter().any(|d| d.is_rust()) {
        let mut p = assemble(base, head, &diffs, IndexState::skipped(), None, &NoSources);
        p.notes = vec!["no Rust files changed".into()];
        let src = PackSource { tree: git.root.clone(), cache_dir: PathBuf::new(), build_env: Vec::new(), changed_lines: changed_lines_of(&diffs) };
        return Ok((p, src));
    }

    let cache_root = req.cache_root.clone().unwrap_or_else(default_cache_root);
    let checked_out = git.resolve("HEAD").map_err(PackError::Git)?;
    let mut notes = Vec::new();
    let mut tool = req.tool.clone();
    let (tree, index_dir) = if head == checked_out {
        let dirty = git.dirty_sources().map_err(PackError::Git)?;
        if !dirty.is_empty() {
            notes.push(format!(
                "the working tree has uncommitted changes to {} Rust/Cargo file(s) ({}); symbols describe the working tree, not {}",
                dirty.len(),
                dirty.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
                &head[..8]
            ));
        }
        (git.root.clone(), cache_dir_for(&cache_root, &git.root, "worktree"))
    } else {
        let label = format!("tree-{}", &head[..12]);
        let dir = cache_dir_for(&cache_root, &git.root, &label);
        let src = dir.join("src");
        if !src.is_dir() {
            // Unpack beside the final name, then rename, so a crash never leaves a half tree.
            let tmp = dir.join(format!("src.tmp-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            git.export(&head, &tmp).map_err(PackError::Git)?;
            std::fs::rename(&tmp, &src).map_err(|e| PackError::Io(format!("cannot place the exported tree: {e}")))?;
        }
        notes.push(format!("{} is not checked out: symbols describe an exported snapshot of it", &head[..8]));
        // Exported trees share one build directory so dependencies compile once, not per commit.
        if std::env::var_os("CARGO_TARGET_DIR").is_none() {
            let shared = cache_dir_for(&cache_root, &git.root, "target");
            tool.extra_env.push(("CARGO_TARGET_DIR".into(), shared.to_string_lossy().into_owned()));
        }
        (src, dir.join("index"))
    };
    let tree_cache = if head == checked_out { index_dir.clone() } else { index_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| index_dir.clone()) };
    let mut build_env = tool.extra_env.clone();
    if std::env::var_os("CARGO_TARGET_DIR").is_none() && !build_env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
        build_env.push(("CARGO_TARGET_DIR".into(), cache_dir_for(&cache_root, &git.root, "target").to_string_lossy().into_owned()));
    }

    let opened = open_index(&IndexRequest { tree: &tree, cache_dir: &index_dir, policy: req.policy, tool: &tool }).map_err(PackError::Index)?;
    let src = RepoSources { git: &git, base: &base, tree: &tree };
    let mut pack = assemble(base.clone(), head, &diffs, opened.state, opened.index.as_ref(), &src);
    notes.extend(std::mem::take(&mut pack.notes));
    pack.notes = notes;
    Ok((pack, PackSource { tree, cache_dir: tree_cache, build_env, changed_lines: changed_lines_of(&diffs) }))
}

/// For the no-Rust path, where no file is ever read.
struct NoSources;
impl Sources for NoSources {
    fn head(&self, _: &str) -> Option<String> {
        None
    }
    fn base(&self, _: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileDiff, Hunk};
    use crate::testkit::index_for;
    use alsp_index::{IndexStatus, UnitReport};
    use std::collections::HashMap;

    struct Mem(HashMap<String, String>, HashMap<String, String>);
    impl Sources for Mem {
        fn head(&self, p: &str) -> Option<String> {
            self.0.get(p).cloned()
        }
        fn base(&self, p: &str) -> Option<String> {
            self.1.get(p).cloned()
        }
    }
    fn mem(head: &[(&str, &str)], base: &[(&str, &str)]) -> Mem {
        let c = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        Mem(c(head), c(base))
    }
    fn modified(path: &str, lines: (u32, u32)) -> FileDiff {
        FileDiff {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            hunks: vec![Hunk { old_start: lines.0, old_len: lines.1 - lines.0 + 1, new_start: lines.0, new_len: lines.1 - lines.0 + 1 }],
        }
    }
    fn state() -> IndexState {
        IndexState { status: IndexStatus::Fresh, rebuilt: false, units: vec![UnitReport { path: ".".into(), ok: true, detail: None }], millis: 0 }
    }

    const LIB: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    1;\n}\n";

    #[test]
    fn modified_function_gets_callers_and_entry_points_and_modules_get_none() {
        let src = mem(&[("src/lib.rs", LIB)], &[("src/lib.rs", LIB)]);
        let idx = index_for(&[("src/lib.rs", LIB)]);
        let p = assemble("b".into(), "h".into(), &[modified("src/lib.rs", (5, 5))], state(), Some(&idx), &src);
        assert_eq!(p.symbols.len(), 1);
        let s = &p.symbols[0];
        assert_eq!((s.name.as_str(), s.kind, s.span.start, s.span.end), ("lib::helper", ChangeKind::Modified, 4, 6));
        assert_eq!(s.callers.len(), 1);
        assert_eq!(s.entry_points[0].provenance, Provenance::Heuristic);
        assert!(s.coverage.is_none() && s.gaps.is_empty());
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        // a hunk on the module-level only (the file's first line is inside `api`, so use a blank-free file)
        let src2 = mem(&[("src/m.rs", "use a;\nfn f() {}\n")], &[("src/m.rs", "fn f() {}\n")]);
        let idx2 = index_for(&[("src/m.rs", "use a;\nfn f() {}\n")]);
        let d = FileDiff { old_path: Some("src/m.rs".into()), new_path: Some("src/m.rs".into()), hunks: vec![Hunk { old_start: 0, old_len: 0, new_start: 1, new_len: 1 }] };
        let p2 = assemble("b".into(), "h".into(), &[d], state(), Some(&idx2), &src2);
        assert_eq!(p2.symbols[0].name, "m");
        assert!(p2.symbols[0].callers.is_empty() && p2.symbols[0].entry_points.is_empty());
    }

    #[test]
    fn symbols_are_sorted_by_file_then_line_and_removed_ones_are_noted_with_no_edges() {
        let base_a = "fn x() {}\nfn gone() {}\n";
        let head_a = "fn x() { 1; }\n";
        let src = mem(&[("src/a.rs", head_a), ("src/b.rs", "fn y() { 2; }\n")], &[("src/a.rs", base_a), ("src/b.rs", "fn y() {}\n")]);
        let idx = index_for(&[("src/a.rs", head_a), ("src/b.rs", "fn y() { 2; }\n")]);
        let diffs = [
            modified("src/b.rs", (1, 1)),
            FileDiff { old_path: Some("src/a.rs".into()), new_path: Some("src/a.rs".into()), hunks: vec![Hunk { old_start: 1, old_len: 2, new_start: 1, new_len: 1 }] },
        ];
        let p = assemble("b".into(), "h".into(), &diffs, state(), Some(&idx), &src);
        let order: Vec<_> = p.symbols.iter().map(|s| (s.file.as_str(), s.kind)).collect();
        assert_eq!(order, vec![("src/a.rs", ChangeKind::Modified), ("src/a.rs", ChangeKind::Removed), ("src/b.rs", ChangeKind::Modified)]);
        let removed = &p.symbols[1];
        assert!(removed.callers.is_empty() && removed.entry_points.is_empty());
        assert!(p.notes.iter().any(|n| n.contains("removed symbols come from reading the base file's syntax (heuristic)")));
    }

    #[test]
    fn no_index_means_no_symbols_and_a_note_saying_unknown() {
        let src = mem(&[], &[]);
        let mut st = state();
        st.units[0].ok = false;
        let p = assemble("b".into(), "h".into(), &[modified("src/a.rs", (1, 1))], st, None, &src);
        assert!(p.symbols.is_empty() && !p.facts_served());
        assert!(p.notes[0].contains("unknown, not unchanged"));
    }

    #[test]
    fn trait_impl_methods_and_ambiguous_ids_are_called_out() {
        let t = "struct S;\nimpl Show for S {\n    fn show(&self) {\n        1;\n    }\n}\n";
        let src = mem(&[("src/t.rs", t)], &[("src/t.rs", t)]);
        let idx = index_for(&[("src/t.rs", t)]);
        let p = assemble("b".into(), "h".into(), &[modified("src/t.rs", (4, 4))], state(), Some(&idx), &src);
        assert!(p.notes.iter().any(|n| n.contains("trait impl methods") && n.contains("t::<S as Show>::show")), "{:?}", p.notes);

        let a = "fn helper() {\n    1;\n}\nfn use_a() {\n    helper();\n}\n";
        let b = "fn helper() {}\nfn use_b() {\n    helper();\n}\n";
        let files = [("tests/a.rs", a), ("tests/b.rs", b)];
        let idx = index_for(&files);
        let src = mem(&files, &files);
        let p = assemble("b".into(), "h".into(), &[modified("tests/a.rs", (2, 2))], state(), Some(&idx), &src);
        let helper = p.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(helper.callers.len(), 1, "only the same-file caller, not use_b: {:?}", helper.callers);
        assert!(helper.callers[0].file == "tests/a.rs");
        assert!(p.notes.iter().any(|n| n.contains("same symbol id as items in other crate roots") && n.contains("helper (0 unattributed)")), "{:?}", p.notes);
    }

    #[test]
    fn truncated_entry_point_search_is_noted() {
        let mut s = String::from("fn target() {\n    1;\n}\n");
        for i in 0..(graph::MAX_ENTRY_POINTS + 2) {
            s.push_str(&format!("#[test]\nfn t{i:03}() {{\n    target();\n}}\n"));
        }
        let src = mem(&[("src/lib.rs", &s)], &[("src/lib.rs", &s)]);
        let idx = index_for(&[("src/lib.rs", &s)]);
        let p = assemble("b".into(), "h".into(), &[modified("src/lib.rs", (2, 2))], state(), Some(&idx), &src);
        assert_eq!(p.symbols[0].entry_points.len(), graph::MAX_ENTRY_POINTS);
        assert!(p.notes.iter().any(|n| n.contains("entry-point search hit its limit") && n.contains("lib::target")));
    }

    #[test]
    fn display_of_errors_and_sources_without_files() {
        assert_eq!(PackError::Git("x".into()).to_string(), "x");
        assert_eq!(PackError::Io("y".into()).to_string(), "y");
        assert!(PackError::Index(alsp_index::IndexError::NoCargoRoot("/p".into())).to_string().contains("no Cargo.toml"));
        assert!(NoSources.head("a").is_none() && NoSources.base("a").is_none());
    }

    #[test]
    fn changed_lines_are_head_side_sorted_and_deduplicated_per_file() {
        let d = |p: &str, hunks: Vec<(u32, u32)>| FileDiff {
            old_path: Some(p.into()),
            new_path: Some(p.into()),
            hunks: hunks.into_iter().map(|(s, l)| Hunk { old_start: s, old_len: l, new_start: s, new_len: l }).collect(),
        };
        let gone = FileDiff { old_path: Some("old.rs".into()), new_path: None, hunks: vec![Hunk { old_start: 1, old_len: 2, new_start: 0, new_len: 0 }] };
        let m = changed_lines_of(&[d("a.rs", vec![(10, 2), (3, 1), (11, 2)]), gone]);
        assert_eq!(m["a.rs"], vec![3, 10, 11, 12]);
        assert!(!m.contains_key("old.rs"));
        let src = PackSource { tree: ".".into(), cache_dir: ".".into(), build_env: vec![], changed_lines: m };
        assert!(src.is_changed("a.rs", 11) && !src.is_changed("a.rs", 4) && !src.is_changed("b.rs", 3));
    }

    #[test]
    fn test_fn_text_returns_only_test_functions_with_their_attributes() {
        let f = "fn helper() {}\n\n/// doc\n#[test]\nfn t_one() {\n    helper();\n}\n";
        assert_eq!(test_fn_text(f, 5).unwrap(), "/// doc\n#[test]\nfn t_one() {\n    helper();\n}");
        assert_eq!(test_fn_text(f, 1), None, "not a test");
        assert_eq!(test_fn_text(f, 99), None, "no item there");
        assert_eq!(test_fn_text("fn (", 1), None, "does not parse");
    }

    // --- build_pack / build_pack_with_source / RepoSources against a real two-commit repo ---

    const CARGO: &str = "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n";
    const LIB2: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    2;\n}\n";

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with two commits (helper's body changes) and a fake rust-analyzer that serves a SCIP
    /// index of `LIB2`. The tool is passed explicitly, so no process environment is touched.
    struct Fixture {
        repo: tempfile::TempDir,
        _fx: tempfile::TempDir,
        cache: tempfile::TempDir,
        tool: Tool,
        c1: String,
        c2: String,
    }

    fn fixture() -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let (repo, fx, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(fx.path().join("default.scip"), crate::testkit::scip_for(&[("src/lib.rs", LIB2)])).unwrap();
        let ra = fx.path().join("ra.sh");
        std::fs::write(&ra, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\ncp \"{}/default.scip\" \"$4\"\n", fx.path().display())).unwrap();
        std::fs::set_permissions(&ra, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = repo.path();
        git_cmd(d, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("Cargo.toml"), CARGO).unwrap();
        std::fs::write(d.join("src/lib.rs"), LIB).unwrap();
        git_cmd(d, &["add", "-A"]);
        git_cmd(d, &["commit", "-q", "-m", "c1"]);
        let c1 = git_cmd(d, &["rev-parse", "HEAD"]);
        std::fs::write(d.join("src/lib.rs"), LIB2).unwrap();
        git_cmd(d, &["add", "-A"]);
        git_cmd(d, &["commit", "-q", "-m", "c2"]);
        let c2 = git_cmd(d, &["rev-parse", "HEAD"]);
        let tool = Tool { rust_analyzer: ra, ..Tool::default() };
        Fixture { repo, _fx: fx, cache, tool, c1, c2 }
    }

    fn request(f: &Fixture, base: &str, head: &str) -> PackRequest {
        PackRequest {
            repo: f.repo.path().to_path_buf(),
            base: base.into(),
            head: head.into(),
            policy: BuildPolicy::Build,
            cache_root: Some(f.cache.path().to_path_buf()),
            tool: f.tool.clone(),
        }
    }

    #[test]
    fn build_pack_with_source_describes_the_checked_out_tree_and_repo_sources_read_both_sides() {
        let f = fixture();
        let (pack, src) = build_pack_with_source(&request(&f, &f.c1, "HEAD")).unwrap();
        assert_eq!(pack.head, f.c2);
        assert!(pack.facts_served(), "{:?}", pack.index);
        assert_eq!(pack.symbols.len(), 1, "{:?}", pack.notes);
        assert_eq!((pack.symbols[0].name.as_str(), pack.symbols[0].kind), ("lib::helper", ChangeKind::Modified));
        assert_eq!(pack.symbols[0].callers.len(), 1);
        assert_eq!(src.tree, Git::open(f.repo.path()).unwrap().root);
        assert!(src.is_changed("src/lib.rs", 5) && !src.is_changed("src/lib.rs", 1));
        assert!(src.build_env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"));

        let git = Git::open(f.repo.path()).unwrap();
        let rs = RepoSources { git: &git, base: &f.c1, tree: &src.tree };
        assert_eq!(rs.head("src/lib.rs").as_deref(), Some(LIB2), "head reads the tree");
        assert_eq!(rs.base("src/lib.rs").as_deref(), Some(LIB), "base reads the base commit");
        assert_eq!(rs.head("src/missing.rs"), None);
        assert_eq!(rs.base("src/missing.rs"), None);
    }

    #[test]
    fn build_pack_exports_a_head_that_is_not_checked_out() {
        let f = fixture();
        // head = c1 is not checked out (HEAD is c2): the pack describes an exported snapshot of it.
        let pack = build_pack(&request(&f, &f.c2, &f.c1)).unwrap();
        assert_eq!(pack.head, f.c1);
        assert!(pack.facts_served(), "{:?}", pack.index);
        assert!(pack.notes.iter().any(|n| n.contains("is not checked out")), "{:?}", pack.notes);
        // an unknown revision is a Git error, not a panic
        assert!(matches!(build_pack(&request(&f, "nope-not-a-rev", "HEAD")), Err(PackError::Git(_))));
    }
}
