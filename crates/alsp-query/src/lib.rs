//! `alsp-query`: ask about any symbol or line of a Rust repository, no diff needed.
//!
//! [`Workspace::open`] finds the repository, serves (or builds) the cached symbol index of its
//! working tree, and returns a handle that answers the five questions an agent has while writing
//! code: [`sym`](Workspace::sym) (where is it defined, how often is it used), [`refs`](Workspace::refs)
//! (who uses it, from tests or not, and which entry points reach it), [`tests`](Workspace::tests)
//! (which tests reach it, optionally which ran it), [`cover`](Workspace::cover) (was it run) and
//! [`gaps`](Workspace::gaps) (would the tests catch a bug here).
//!
//! Every command returns the same [`Answer`], which renders as compact text or JSON with each
//! fact's provenance. Targets are typed as a name or `file:line` and resolved once, in
//! [`Workspace::resolve`]. The index and the call-graph walk are the evidence layer's own
//! (`alsp-index`, `alsp-evidence`); coverage and mutant gaps reuse `alsp-coverage` and `alsp-gaps`
//! unchanged by describing the target to them as a one-symbol pack.

mod answer;
mod ask;
mod target;

pub use answer::*;
pub use target::{kind_str, Candidate, Target, TargetError};

use alsp_coverage::CoverageTool;
use alsp_evidence::{Git, Sources};
use alsp_gaps::MutantsTool;
use alsp_index::{cache_dir_for, default_cache_root, open_index_with, BuildPolicy, IndexError, IndexRequest, IndexState, IndexStatus, SymbolIndex, Tool, UnitReport};
use std::path::{Component, Path, PathBuf};

/// Most items a list shows unless the caller lifts the cap.
pub const DEFAULT_LIMIT: usize = 25;

pub struct OpenOptions {
    /// Any directory inside the repository's work tree.
    pub repo: PathBuf,
    /// Never run rust-analyzer: serve a fully fresh cached index or fail with [`OpenError::Unavailable`].
    pub no_build: bool,
    /// Parent of the cache directories; `alsp_index::default_cache_root()` when `None`.
    pub cache_root: Option<PathBuf>,
    pub tool: Tool,
    /// Cargo roots outside the root workspace to index too, relative to the repository (such as
    /// `apps/foo/rust`, a package with its own `[workspace]`). The root manifest's workspace is
    /// always indexed; a nested package that is not one of its members is not, unless listed here.
    pub extra_roots: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum OpenError {
    /// Not a git work tree, or no commit to describe.
    Repo(String),
    Index(IndexError),
    /// The index could not be served: stale under `no_build`, or every Cargo root failed. The
    /// state says why per root.
    Unavailable(IndexState),
}

impl std::fmt::Display for OpenError {
    /// Says what happened and what to do about it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Repo(m) => write!(f, "{m}; run inside a git repository with at least one commit, or pass --repo <path>"),
            OpenError::Index(IndexError::NoCargoRoot(p)) => write!(f, "no Cargo.toml found in {}; point --repo (or --root) at a Rust package", p.display()),
            OpenError::Index(e) => write!(f, "{e}"),
            OpenError::Unavailable(s) => {
                let why: Vec<String> = s.units.iter().filter(|u| !u.ok).map(|u| format!("{}: {}", u.path, u.detail.as_deref().unwrap_or("unknown"))).collect();
                // `rebuilt` is false only when the indexer was never allowed to run (`--no-build`).
                let next = if s.rebuilt { "check that rust-analyzer is installed and that the crate builds" } else { "rerun without --no-build to build it (about 30 s for a large repository the first time)" };
                write!(f, "no index could be served ({}; index {}): {next}", why.join("; "), s.status.as_str())
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Reads head-side files from the working tree; there is no base side in a query.
struct TreeSources<'a>(&'a Path);

impl Sources for TreeSources<'_> {
    fn head(&self, path: &str) -> Option<String> {
        let ok = Path::new(path).components().all(|c| matches!(c, Component::Normal(_)));
        ok.then(|| std::fs::read_to_string(self.0.join(path)).ok()).flatten()
    }
    fn base(&self, _: &str) -> Option<String> {
        None
    }
}

/// An opened repository: its index and what is needed to run coverage and mutant listing on it.
pub struct Workspace {
    pub(crate) tree: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) build_env: Vec<(String, String)>,
    pub(crate) state: IndexState,
    pub(crate) head: String,
    pub(crate) dirty: Vec<String>,
    pub(crate) index: SymbolIndex,
    /// Facts about the index worth showing with every answer (a root that failed to index).
    pub(crate) notes: Vec<String>,
    pub(crate) limit: usize,
    pub(crate) coverage_tool: CoverageTool,
    pub(crate) mutants_tool: MutantsTool,
    pub(crate) progress: fn(&str),
}

impl Workspace {
    /// A workspace over an index the caller already holds (`tree` is its canonical root). Reports a
    /// fresh, fully built index of an unknown commit; [`Workspace::open`] sets the real state.
    pub fn new(tree: PathBuf, cache_dir: PathBuf, index: SymbolIndex) -> Workspace {
        let state = IndexState { status: IndexStatus::Fresh, rebuilt: false, units: vec![UnitReport { path: ".".into(), ok: true, detail: None }], millis: 0 };
        Workspace {
            tree,
            cache_dir,
            build_env: Vec::new(),
            state,
            head: "0".repeat(40),
            dirty: Vec::new(),
            index,
            notes: Vec::new(),
            limit: DEFAULT_LIMIT,
            coverage_tool: CoverageTool::default(),
            mutants_tool: MutantsTool::default(),
            progress: |_| {},
        }
    }

    /// Open the repository containing `o.repo`. The index is the one `alsp pack` keeps for the
    /// checked-out tree, so a pack and a query share one build.
    pub fn open(o: &OpenOptions) -> Result<Workspace, OpenError> {
        let git = Git::open(&o.repo).map_err(OpenError::Repo)?;
        let head = git.resolve("HEAD").map_err(OpenError::Repo)?;
        let dirty = git.dirty_sources().map_err(OpenError::Repo)?;
        let cache_root = o.cache_root.clone().unwrap_or_else(default_cache_root);
        let cache_dir = cache_dir_for(&cache_root, &git.root, "worktree");
        let policy = if o.no_build { BuildPolicy::NoBuild } else { BuildPolicy::Build };
        let opened = open_index_with(&IndexRequest { tree: &git.root, cache_dir: &cache_dir, policy, tool: &o.tool }, &o.extra_roots).map_err(OpenError::Index)?;
        let Some(index) = opened.index else { return Err(OpenError::Unavailable(opened.state)) };
        // Cargo runs on the work tree build into the cache, never into the repository.
        let mut build_env = o.tool.extra_env.clone();
        if std::env::var_os("CARGO_TARGET_DIR").is_none() && !build_env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
            build_env.push(("CARGO_TARGET_DIR".into(), cache_dir_for(&cache_root, &git.root, "target").to_string_lossy().into_owned()));
        }
        let mut ws = Workspace::new(git.root, cache_dir, index);
        ws.notes = opened.state.units.iter().filter(|u| !u.ok).map(|u| format!("root {} was not indexed: {}", u.path, u.detail.as_deref().unwrap_or("unknown"))).collect();
        if opened.state.status == IndexStatus::Stale && opened.state.rebuilt {
            ws.notes.push("the cached index was built from older sources; it was rebuilt for this query and matches the working tree".into());
        }
        ws.build_env = build_env;
        ws.state = opened.state;
        ws.head = head;
        ws.dirty = dirty;
        Ok(ws)
    }

    /// Cap lists at `n` items (`usize::MAX` lifts the cap).
    pub fn with_limit(mut self, n: usize) -> Workspace {
        self.limit = n;
        self
    }

    /// Replace the cargo wrappers (tests point them at a fake cargo).
    pub fn with_tools(mut self, coverage: CoverageTool, mutants: MutantsTool) -> Workspace {
        self.coverage_tool = coverage;
        self.mutants_tool = mutants;
        self
    }

    /// Environment for the cargo runs of `cover`, `tests --run` and `gaps`.
    pub fn with_build_env(mut self, env: Vec<(String, String)>) -> Workspace {
        self.build_env = env;
        self
    }

    /// Where a long step reports what it did, e.g. how long a coverage run took.
    pub fn with_progress(mut self, f: fn(&str)) -> Workspace {
        self.progress = f;
        self
    }

    /// Why the index holds nothing for `file`, and what to do about it.
    pub(crate) fn unindexed_note(&self, file: &str) -> String {
        let mut dir = self.tree.join(file);
        while dir.pop() && dir != self.tree && dir.starts_with(&self.tree) {
            if dir.join("Cargo.toml").is_file() {
                let rel = dir.strip_prefix(&self.tree).unwrap_or(&dir).to_string_lossy().replace('\\', "/");
                let covered = self.state.units.iter().any(|u| u.path == rel);
                if !covered {
                    return format!("{file} is in the Cargo package {rel}, which the index does not cover (a package outside the root workspace): rerun with --root {rel}");
                }
                break;
            }
        }
        format!("the index holds no symbols for {file} (a file with no items, one outside every crate's module tree, or generated code)")
    }

    pub(crate) fn freshness(&self) -> Freshness {
        Freshness { status: self.state.status.as_str().into(), rebuilt: self.state.rebuilt, head: self.head.clone(), dirty: self.dirty.clone() }
    }
}

#[cfg(test)]
mod tests;
