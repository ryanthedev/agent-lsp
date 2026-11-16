//! `alsp-index`: the symbol index of a Rust tree.
//!
//! One call, [`open_index`], does the whole job: find the Cargo roots of the tree, reuse each
//! root's cached `rust-analyzer scip` output when its content hash still matches, rebuild it when
//! not, and return a [`SymbolIndex`] together with an [`IndexState`] saying what happened. A root
//! that fails to index is reported and left out; the other roots still answer.
//!
//! The cache lives outside the indexed tree (see [`cache_dir_for`]), so indexing never touches a
//! repository's files.

mod build;
mod hash;
mod locate;
mod model;
mod scip_read;

pub use build::{find_units, Tool};
pub use locate::{locate_rust_analyzer, locate_rust_analyzer_avoiding, LocateError, Located, Tried};
pub use hash::{tracked_files, tree_hash};
pub use model::{readable_name, Def, Linked, RefSite, Span, SymKind, SymbolIndex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Bump when the cached file format or the way facts are read from it changes.
const SCHEMA: &str = "alsp-index-v1";

/// What the cache held when the call started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexStatus {
    /// Every root's cached index matched the tree.
    Fresh,
    /// A cached index existed but the sources, manifests, lockfile or indexer changed.
    Stale,
    /// Nothing cached for at least one root (first run).
    Missing,
    /// No index was needed or consulted (the diff touches no Rust).
    Skipped,
}

impl IndexStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexStatus::Fresh => "fresh",
            IndexStatus::Stale => "stale",
            IndexStatus::Missing => "missing",
            IndexStatus::Skipped => "skipped",
        }
    }
}

/// One Cargo root of the tree. `path` is relative to the tree, `.` for the tree itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitReport {
    pub path: String,
    pub ok: bool,
    /// Why the root is missing from the index (or how it was served, for the ones that are in).
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexState {
    pub status: IndexStatus,
    /// An indexer ran during this call.
    pub rebuilt: bool,
    pub units: Vec<UnitReport>,
    pub millis: u64,
    /// Caveats about how the index was (or could not be) obtained, e.g. which rust-analyzer was
    /// tried. Rendered next to the status by `alsp pack` and `alsp review`; rows written before
    /// this field existed read as empty.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl IndexState {
    pub fn skipped() -> Self {
        IndexState { status: IndexStatus::Skipped, rebuilt: false, units: Vec::new(), millis: 0, notes: Vec::new() }
    }
    /// Every root is in the index.
    pub fn complete(&self) -> bool {
        self.units.iter().all(|u| u.ok)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildPolicy {
    /// Rebuild what is missing or stale.
    Build,
    /// Never run the indexer: serve a fully fresh index or nothing.
    NoBuild,
}

pub struct IndexRequest<'a> {
    pub tree: &'a Path,
    /// Directory for this tree's cached files; created on demand.
    pub cache_dir: &'a Path,
    pub policy: BuildPolicy,
    pub tool: &'a Tool,
}

pub struct OpenedIndex {
    pub state: IndexState,
    /// `None` when nothing may be served: stale under `NoBuild`, or every root failed.
    pub index: Option<SymbolIndex>,
}

#[derive(Debug)]
pub enum IndexError {
    Io(String),
    NoCargoRoot(PathBuf),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Io(m) => write!(f, "{m}"),
            IndexError::NoCargoRoot(p) => write!(f, "no Cargo.toml found in {}", p.display()),
        }
    }
}

impl std::error::Error for IndexError {}

fn io<T>(r: std::io::Result<T>, what: &str) -> Result<T, IndexError> {
    r.map_err(|e| IndexError::Io(format!("{what}: {e}")))
}

/// Default parent of all cache directories: `$ALSP_CACHE_DIR`, else `$XDG_CACHE_HOME/alsp`, else
/// `$HOME/.cache/alsp`.
pub fn default_cache_root() -> PathBuf {
    cache_root_from(|k| std::env::var_os(k))
}

fn cache_root_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    let var = |k| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("ALSP_CACHE_DIR")
        .or_else(|| var("XDG_CACHE_HOME").map(|d| d.join("alsp")))
        .or_else(|| var("HOME").map(|d| d.join(".cache").join("alsp")))
        .unwrap_or_else(|| std::env::temp_dir().join("alsp-cache"))
}

/// `<root>/<16 hex of the repository path>/<label>`: one directory per repository and tree.
pub fn cache_dir_for(root: &Path, repo: &Path, label: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(repo.to_string_lossy().as_bytes()));
    root.join(&key[..16]).join(label)
}

fn slug(unit: &Path) -> String {
    if unit.as_os_str().is_empty() {
        "root".into()
    } else {
        unit.to_string_lossy().replace(['/', '\\'], "_")
    }
}

fn unit_name(unit: &Path) -> String {
    if unit.as_os_str().is_empty() { ".".into() } else { unit.to_string_lossy().replace('\\', "/") }
}

/// What the cache holds for one root.
enum Cached {
    Fresh(PathBuf),
    Stale,
    Missing,
}

fn cached(cache_dir: &Path, slug: &str, hash: &str) -> Cached {
    let want = cache_dir.join(format!("{slug}.{hash}.scip"));
    if want.is_file() {
        return Cached::Fresh(want);
    }
    let older = std::fs::read_dir(cache_dir).ok().into_iter().flatten().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.ends_with(".scip") && n.strip_prefix(slug).is_some_and(|r| r.starts_with('.'))
    });
    if older { Cached::Stale } else { Cached::Missing }
}

fn remove_older(cache_dir: &Path, slug: &str, keep: &Path) {
    for e in std::fs::read_dir(cache_dir).ok().into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        // `<slug>.<16 hex>.scip` only: `a` must not eat `a_b`'s files.
        let hex_mid = n.strip_prefix(slug).and_then(|r| r.strip_prefix('.')).and_then(|r| r.strip_suffix(".scip"));
        if e.path() != keep && hex_mid.is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Build an index straight from SCIP bytes (paths as the indexer wrote them). For callers that
/// already hold a SCIP file, and for tests.
pub fn index_from_scip(bytes: &[u8]) -> Result<SymbolIndex, String> {
    scip_read::read_scip(bytes, "").map(|p| scip_read::merge(vec![p]))
}

type Parsed = (Vec<Def>, std::collections::HashMap<String, Vec<RefSite>>);

pub fn open_index(req: &IndexRequest) -> Result<OpenedIndex, IndexError> {
    open_index_with(req, &[])
}

/// [`open_index`] plus Cargo roots the tree's own manifest does not reach, given relative to the
/// tree (`apps/foo/rust`: a package with its own `[workspace]`). Each is indexed and cached as a
/// root of its own; one that has no `Cargo.toml` is [`IndexError::NoCargoRoot`].
pub fn open_index_with(req: &IndexRequest, extra_roots: &[PathBuf]) -> Result<OpenedIndex, IndexError> {
    let started = Instant::now();
    let mut units = io(find_units(req.tree), "listing Cargo roots")?;
    for extra in extra_roots {
        let inside = !extra.as_os_str().is_empty() && extra.components().all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            return Err(IndexError::Io(format!("{} is not a directory inside the repository", extra.display())));
        }
        if !req.tree.join(extra).join("Cargo.toml").is_file() {
            return Err(IndexError::NoCargoRoot(req.tree.join(extra)));
        }
        if !units.contains(extra) {
            units.push(extra.clone());
        }
    }
    if units.is_empty() {
        return Err(IndexError::NoCargoRoot(req.tree.to_path_buf()));
    }
    io(std::fs::create_dir_all(req.cache_dir), "creating the cache directory")?;
    // The indexer to run, and what goes into the cache key: a different rust-analyzer can read the
    // same sources differently. `Err` is why none could be found; it fails every root that needs a build.
    let mut notes = Vec::new();
    let resolved: Result<(Tool, String), String> = if req.tool.locate {
        match locate_rust_analyzer_avoiding(req.tree, req.tool.repo_root.as_slice()) {
            Ok(l) => {
                if l.is_fallback() {
                    notes.push(format!("rust-analyzer: {} at {} ({})", l.source, l.path.display(), l.version));
                }
                let key = format!("{}\n{}", l.version, l.path.display());
                Ok((Tool { rust_analyzer: l.path, locate: false, ..req.tool.clone() }, key))
            }
            Err(e) => Err(e.to_string()),
        }
    } else {
        req.tool.version(req.cache_dir).map(|v| (req.tool.clone(), v))
    };
    let salt = format!("{SCHEMA}\n{}", resolved.as_ref().map_or("indexer-unavailable", |(_, key)| key));

    struct Unit {
        rel: PathBuf,
        slug: String,
        hash: String,
        cached: Cached,
    }
    let mut state: Vec<Unit> = Vec::new();
    for rel in units {
        let hash = io(tree_hash(&req.tree.join(&rel), &salt), "hashing sources")?;
        let slug = slug(&rel);
        let c = cached(req.cache_dir, &slug, &hash);
        state.push(Unit { rel, slug, hash, cached: c });
    }
    let status = if state.iter().all(|u| matches!(u.cached, Cached::Fresh(_))) {
        IndexStatus::Fresh
    } else if state.iter().any(|u| matches!(u.cached, Cached::Stale)) {
        IndexStatus::Stale
    } else {
        IndexStatus::Missing
    };

    if status != IndexStatus::Fresh && req.policy == BuildPolicy::NoBuild {
        if let (true, Err(why)) = (req.tool.locate, &resolved) {
            notes.push(why.clone());
        }
        let units = state
            .iter()
            .map(|u| UnitReport {
                path: unit_name(&u.rel),
                ok: false,
                detail: Some(format!("{}; not rebuilt because the indexer is disabled", if matches!(u.cached, Cached::Fresh(_)) { "cached" } else { "no current index" })),
            })
            .collect();
        return Ok(OpenedIndex {
            state: IndexState { status, rebuilt: false, units, millis: started.elapsed().as_millis() as u64, notes },
            index: None,
        });
    }

    let mut reports = Vec::new();
    let mut parsed: Vec<Parsed> = Vec::new();
    let mut rebuilt = false;
    for u in &state {
        let prefix = if u.rel.as_os_str().is_empty() { String::new() } else { unit_name(&u.rel) };
        let name = unit_name(&u.rel);
        // A cached file that no longer parses is rebuilt like a missing one.
        let from_cache = match &u.cached {
            Cached::Fresh(p) => scip_read::read_scip_file(p, &prefix).ok(),
            _ => None,
        };
        if let Some(p) = from_cache {
            parsed.push(p);
            reports.push(UnitReport { path: name, ok: true, detail: None });
            continue;
        }
        rebuilt = true;
        let target = req.cache_dir.join(format!("{}.{}.scip", u.slug, u.hash));
        let tmp = req.cache_dir.join(format!("{}.{}.scip.tmp-{}", u.slug, u.hash, std::process::id()));
        let dir = if u.rel.as_os_str().is_empty() { req.tree.to_path_buf() } else { req.tree.join(&u.rel) };
        let built = resolved
            .as_ref()
            .map_err(|e| e.clone())
            .and_then(|(tool, _)| build::run_scip(tool, &dir, &tmp))
            .and_then(|_| scip_read::read_scip_file(&tmp, &prefix))
            .and_then(|p| std::fs::rename(&tmp, &target).map(|_| p).map_err(|e| format!("cannot store the index: {e}")));
        match built {
            Ok(p) => {
                remove_older(req.cache_dir, &u.slug, &target);
                parsed.push(p);
                reports.push(UnitReport { path: name, ok: true, detail: None });
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                reports.push(UnitReport { path: name, ok: false, detail: Some(e) });
            }
        }
    }
    let index = (!parsed.is_empty()).then(|| scip_read::merge(parsed));
    Ok(OpenedIndex {
        state: IndexState { status, rebuilt, units: reports, millis: started.elapsed().as_millis() as u64, notes },
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::fake_tool;
    use scip::types::{symbol_information::Kind, Document};
    use std::fs;

    const P: &str = "rust-analyzer cargo c 0.0.1 ";

    /// A fake rust-analyzer: copies `$FIXTURE/<dir name>.scip` to `--output`, or fails when the
    /// fixture is missing. Lets the tests run offline with exact, hand-built indexes.
    fn fake_ra(dir: &Path, fixtures: &Path) -> Tool {
        fake_tool(
            dir,
            &format!(
                "if [ \"$1\" = --version ]; then echo 'fake-ra 1'; exit 0; fi\n\
                 src=\"$2\"; name=$(basename \"$src\"); [ \"$src\" = \"{t}\" ] && name=root\n\
                 echo run >> \"{fx}/runs.log\"\n\
                 [ -f \"{fx}/$name.scip\" ] || {{ echo \"cannot load $name\" >&2; exit 1; }}\n\
                 cp \"{fx}/$name.scip\" \"$4\"",
                t = dir.join("tree").display(),
                fx = fixtures.display()
            ),
        )
    }

    fn scip_with_fn(name: &str) -> Vec<u8> {
        let f = format!("{P}m/{name}().");
        let mut doc = Document::new();
        doc.relative_path = "src/m.rs".into();
        doc.occurrences = vec![scip_read::occ(vec![2, 3, 4], &f, 1, vec![1, 0, 6, 1])];
        doc.symbols = vec![scip_read::info(&f, Kind::Function)];
        scip_read::encode(vec![doc])
    }

    fn runs(fx: &Path) -> usize {
        fs::read_to_string(fx.join("runs.log")).map(|s| s.lines().count()).unwrap_or(0)
    }

    struct Env {
        _d: tempfile::TempDir,
        tree: PathBuf,
        cache: PathBuf,
        fx: PathBuf,
        tool: Tool,
    }

    fn env_with_units(units: &[&str]) -> Env {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path().join("tree");
        let fx = d.path().join("fx");
        fs::create_dir_all(&fx).unwrap();
        for u in units {
            let p = if *u == "." { tree.clone() } else { tree.join(u) };
            fs::create_dir_all(p.join("src")).unwrap();
            fs::write(p.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
            fs::write(p.join("src/m.rs"), "fn a(){}\n").unwrap();
        }
        let tool = fake_ra(d.path(), &fx);
        let cache = d.path().join("cache");
        Env { tree, cache, fx, tool, _d: d }
    }

    fn open(e: &Env, policy: BuildPolicy) -> OpenedIndex {
        open_index(&IndexRequest { tree: &e.tree, cache_dir: &e.cache, policy, tool: &e.tool }).unwrap()
    }

    #[test]
    fn first_run_builds_second_is_fresh_edit_makes_it_stale_and_rebuilds() {
        let e = env_with_units(&["."]);
        fs::write(e.fx.join("root.scip"), scip_with_fn("a")).unwrap();
        let r1 = open(&e, BuildPolicy::Build);
        assert_eq!((r1.state.status, r1.state.rebuilt), (IndexStatus::Missing, true));
        assert!(r1.index.as_ref().unwrap().def(&format!("{P}m/a().")).is_some());
        assert_eq!(runs(&e.fx), 1);

        let r2 = open(&e, BuildPolicy::Build);
        assert_eq!((r2.state.status, r2.state.rebuilt), (IndexStatus::Fresh, false));
        assert!(r2.index.unwrap().def(&format!("{P}m/a().")).is_some());
        assert_eq!(runs(&e.fx), 1, "the indexer must not run again");

        fs::write(e.tree.join("src/m.rs"), "fn b(){}\n").unwrap();
        fs::write(e.fx.join("root.scip"), scip_with_fn("b")).unwrap();
        let r3 = open(&e, BuildPolicy::Build);
        assert_eq!((r3.state.status, r3.state.rebuilt), (IndexStatus::Stale, true));
        let idx = r3.index.unwrap();
        assert!(idx.def(&format!("{P}m/b().")).is_some() && idx.def(&format!("{P}m/a().")).is_none());
        assert_eq!(runs(&e.fx), 2);
        let scips = fs::read_dir(&e.cache).unwrap().flatten().filter(|f| f.file_name().to_string_lossy().ends_with(".scip")).count();
        assert_eq!(scips, 1, "the superseded index is removed");
    }

    #[test]
    fn no_build_serves_fresh_but_never_stale_or_missing() {
        let e = env_with_units(&["."]);
        fs::write(e.fx.join("root.scip"), scip_with_fn("a")).unwrap();
        let none = open(&e, BuildPolicy::NoBuild);
        assert_eq!((none.state.status, none.state.rebuilt), (IndexStatus::Missing, false));
        assert!(none.index.is_none() && !none.state.complete());
        assert_eq!(runs(&e.fx), 0);
        open(&e, BuildPolicy::Build);
        assert!(open(&e, BuildPolicy::NoBuild).index.is_some());
        fs::write(e.tree.join("src/m.rs"), "fn changed(){}\n").unwrap();
        let stale = open(&e, BuildPolicy::NoBuild);
        assert_eq!(stale.state.status, IndexStatus::Stale);
        assert!(stale.index.is_none());
        assert_eq!(runs(&e.fx), 1);
    }

    /// The only thing `--no-build` may spawn is `rust-analyzer --version` (it keys the cache), and
    /// it must start outside the repository: rustup picks the binary from the directory it runs in.
    #[test]
    fn no_build_spawns_only_version_and_outside_the_repository() {
        let e = env_with_units(&["."]);
        fs::write(e.fx.join("root.scip"), scip_with_fn("a")).unwrap();
        open(&e, BuildPolicy::Build);
        let log = e.fx.join("spawns.log");
        let spy = fake_tool(e.fx.as_path(), &format!("echo \"$PWD|$*\" >> \"{}\"\necho 'fake-ra 1'", log.display()));
        let req = |policy| IndexRequest { tree: &e.tree, cache_dir: &e.cache, policy, tool: &spy };
        assert!(open_index(&req(BuildPolicy::NoBuild)).unwrap().index.is_some(), "the cache key still matches");
        fs::write(e.tree.join("src/m.rs"), "fn changed(){}\n").unwrap();
        assert!(open_index(&req(BuildPolicy::NoBuild)).unwrap().index.is_none());
        let tree = e.tree.canonicalize().unwrap();
        let spawned = fs::read_to_string(&log).unwrap();
        assert_eq!(spawned.lines().count(), 2, "{spawned}");
        for line in spawned.lines() {
            let (cwd, args) = line.split_once('|').unwrap();
            assert_eq!(args, "--version", "{line}");
            assert!(!Path::new(cwd).canonicalize().unwrap().starts_with(&tree), "started inside the repository: {line}");
        }
    }

    #[test]
    fn one_failing_root_is_reported_and_the_others_still_answer() {
        let e = env_with_units(&["good", "bad"]);
        fs::write(e.fx.join("good.scip"), scip_with_fn("a")).unwrap(); // no bad.scip: that root fails
        let r = open(&e, BuildPolicy::Build);
        assert_eq!(r.state.units.len(), 2);
        let bad = r.state.units.iter().find(|u| u.path == "bad").unwrap();
        assert!(!bad.ok && bad.detail.as_deref().unwrap().contains("cannot load bad"), "{bad:?}");
        assert!(r.state.units.iter().find(|u| u.path == "good").unwrap().ok);
        assert!(!r.state.complete());
        let idx = r.index.expect("the good root is still served");
        let d = idx.def(&format!("{P}m/a().")).unwrap();
        assert_eq!(d.file, "good/src/m.rs", "paths carry the root's directory");
        // The failed root is retried next time; the good one is served from cache.
        let r2 = open(&e, BuildPolicy::Build);
        assert_eq!(r2.state.status, IndexStatus::Missing);
        assert!(r2.state.rebuilt && r2.index.is_some());
        assert_eq!(runs(&e.fx), 3, "good once, bad twice");
    }

    #[test]
    fn extra_roots_are_indexed_as_roots_of_their_own_and_validated() {
        let e = env_with_units(&["."]);
        let extra = e.tree.join("apps/x");
        fs::create_dir_all(extra.join("src")).unwrap();
        fs::write(extra.join("Cargo.toml"), "[package]\nname=\"x\"\n[workspace]\n").unwrap();
        fs::write(e.fx.join("root.scip"), scip_with_fn("a")).unwrap();
        fs::write(e.fx.join("x.scip"), scip_with_fn("b")).unwrap();
        let req = IndexRequest { tree: &e.tree, cache_dir: &e.cache, policy: BuildPolicy::Build, tool: &e.tool };
        let roots = [PathBuf::from("apps/x")];
        let r = open_index_with(&req, &roots).unwrap();
        assert_eq!(r.state.units.iter().map(|u| (u.path.as_str(), u.ok)).collect::<Vec<_>>(), vec![(".", true), ("apps/x", true)]);
        let idx = r.index.unwrap();
        assert_eq!(idx.def(&format!("{P}m/b().")).unwrap().file, "apps/x/src/m.rs", "paths carry the root's directory");
        assert!(idx.def(&format!("{P}m/a().")).is_some());
        // cached per root: the second open runs nothing
        let again = open_index_with(&req, &roots).unwrap();
        assert_eq!((again.state.status, runs(&e.fx)), (IndexStatus::Fresh, 2));
        // listing a root twice, or the tree's own root, adds nothing
        assert_eq!(open_index_with(&req, &[roots[0].clone(), roots[0].clone()]).unwrap().state.units.len(), 2);
        assert!(matches!(open_index_with(&req, &[PathBuf::from("nope")]), Err(IndexError::NoCargoRoot(_))));
        for bad in ["../x", "", "/etc"] {
            assert!(matches!(open_index_with(&req, &[PathBuf::from(bad)]), Err(IndexError::Io(_))), "{bad}");
        }
    }

    #[test]
    fn every_root_failing_serves_nothing() {
        let e = env_with_units(&["only"]);
        let r = open(&e, BuildPolicy::Build);
        assert!(r.index.is_none() && !r.state.units[0].ok);
    }

    #[test]
    fn corrupt_cache_file_is_rebuilt() {
        let e = env_with_units(&["."]);
        fs::write(e.fx.join("root.scip"), scip_with_fn("a")).unwrap();
        open(&e, BuildPolicy::Build);
        for f in fs::read_dir(&e.cache).unwrap().flatten() {
            fs::write(f.path(), b"\xff\xff garbage").unwrap();
        }
        let r = open(&e, BuildPolicy::Build);
        assert!(r.state.rebuilt && r.index.is_some());
    }

    #[test]
    fn indexer_garbage_output_is_a_root_failure() {
        let e = env_with_units(&["."]);
        fs::write(e.fx.join("root.scip"), b"\xff\xff not scip").unwrap();
        let r = open(&e, BuildPolicy::Build);
        assert!(r.index.is_none());
        assert!(r.state.units[0].detail.as_deref().unwrap().contains("not a SCIP index"));
        assert!(fs::read_dir(&e.cache).unwrap().flatten().all(|f| !f.file_name().to_string_lossy().contains(".scip")), "no bad file is cached");
    }

    #[test]
    fn a_tree_without_cargo_roots_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let tool = Tool::default();
        let err = open_index(&IndexRequest { tree: d.path(), cache_dir: &d.path().join("c"), policy: BuildPolicy::Build, tool: &tool });
        assert!(matches!(err, Err(IndexError::NoCargoRoot(_))));
        assert!(err.err().unwrap().to_string().contains("no Cargo.toml"));
    }

    #[test]
    fn cache_dir_is_per_repo_and_label_and_slug_does_not_collide() {
        let r = Path::new("/c");
        let a = cache_dir_for(r, Path::new("/repo/a"), "worktree");
        assert_eq!(a, cache_dir_for(r, Path::new("/repo/a"), "worktree"));
        assert_ne!(a, cache_dir_for(r, Path::new("/repo/b"), "worktree"));
        assert_ne!(a, cache_dir_for(r, Path::new("/repo/a"), "commit-1"));
        assert_eq!(slug(Path::new("a/b")), "a_b");
        assert_eq!(slug(Path::new("")), "root");
        // removing old files for slug `a` must not touch `a_b`'s
        let d = tempfile::tempdir().unwrap();
        for n in ["a.0123456789abcdef.scip", "a.1111111111111111.scip", "a_b.0123456789abcdef.scip"] {
            fs::write(d.path().join(n), "x").unwrap();
        }
        remove_older(d.path(), "a", &d.path().join("a.1111111111111111.scip"));
        let mut left: Vec<_> = fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["a.1111111111111111.scip", "a_b.0123456789abcdef.scip"]);
    }

    #[test]
    fn index_from_scip_bytes() {
        let idx = index_from_scip(&scip_with_fn("a")).unwrap();
        assert_eq!(idx.defs_in("src/m.rs").count(), 1);
        assert!(index_from_scip(b"\xff\xff").is_err());
    }

    #[test]
    fn cache_root_precedence() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(v))
        };
        let all = env(&[("ALSP_CACHE_DIR", "/a"), ("XDG_CACHE_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(cache_root_from(all), PathBuf::from("/a"));
        assert_eq!(cache_root_from(env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])), PathBuf::from("/x/alsp"));
        assert_eq!(cache_root_from(env(&[("HOME", "/h")])), PathBuf::from("/h/.cache/alsp"));
        assert_eq!(cache_root_from(env(&[("ALSP_CACHE_DIR", ""), ("HOME", "/h")])), PathBuf::from("/h/.cache/alsp"), "empty counts as unset");
        assert_eq!(cache_root_from(env(&[])), std::env::temp_dir().join("alsp-cache"));
        assert!(default_cache_root().is_absolute());
    }
}
