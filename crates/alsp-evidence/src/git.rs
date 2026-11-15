//! The only module that builds `git` commands (other crates call [`command`]). Revisions are resolved
//! to full commit ids first, and every later command uses those ids, so a hostile ref name can never
//! reach a command line as an option. Every command is hardened against the repository's own
//! `.git/config` (see [`command`]), because a repository handed over as an archive or a directory
//! carries a config its recipient never wrote.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct Git {
    /// Top level of the work tree.
    pub root: PathBuf,
}

/// Settings a repository's own config must not decide: each one can make git run a program. They are
/// passed through git's `GIT_CONFIG_COUNT` environment, which outranks every config file (this is how
/// per-name filter overrides below can carry any characters). `diff.external` is emptied on purpose:
/// git then fails ("cannot run") instead of running the repository's program for any `diff` that forgot
/// `--no-ext-diff`.
const NEUTRALIZED: &[(&str, &str)] = &[
    ("core.fsmonitor", "false"),   // runs a hook on status, diff, add...
    ("core.hooksPath", "/dev/null"), // hooks directory
    ("core.pager", "cat"),
    ("core.sshCommand", ""),
    ("core.askPass", ""),
    ("diff.external", ""),         // external diff program
    ("commit.gpgsign", "false"),   // would run gpg.program on commit-tree
    ("tag.gpgsign", "false"),
    ("core.gitProxy", ""),         // program for git:// connections
    ("credential.helper", ""),     // programs a remote fetch would consult
    ("submodule.recurse", "false"),
    // Transports: a missing object in a partial (promisor) clone makes git fetch from the remote, and
    // the repository's config chooses that remote's URL (`ext::` runs a program) and program (`uploadpack`).
    // Nothing alsp does needs the network, so every transport is refused (per-protocol settings outrank
    // `protocol.allow`, hence the explicit list).
    ("protocol.allow", "never"),
    ("protocol.file.allow", "never"),
    ("protocol.ext.allow", "never"),
    ("protocol.ssh.allow", "never"),
    ("protocol.git.allow", "never"),
    ("protocol.http.allow", "never"),
    ("protocol.https.allow", "never"),
];
/// A `git -C <root>` command that cannot run programs named by the repository's config: the settings
/// in [`NEUTRALIZED`] are forced off; the clean, smudge and process commands of every `filter.<name>`
/// the repository's own config defines are emptied (git skips a filter whose command is empty, and
/// `required` is cleared so that does not fail); every transport is refused and a missing object is never
/// fetched (see below and `GIT_NO_LAZY_FETCH` in [`base`]); no system config, pager, prompt or optional index refresh. Callers
/// must still pass `--no-ext-diff --no-textconv` to `diff`/`show`/`log` (which stops a `diff.<driver>.textconv`
/// from a `.gitattributes` file in the tree itself) and `--ignore-submodules=all` to any command that
/// compares the work tree (git would look inside each submodule, under that submodule's own config).
/// User-level (global) config is trusted and left alone, as is the caller's environment apart from
/// `GIT_EXTERNAL_DIFF`. Filters named only by the global git config (git-lfs, say) keep working; system config is dropped
/// (`GIT_CONFIG_NOSYSTEM`), so a filter set up only there does not run.
///
/// Remote programs (`remote.<name>.uploadpack`, an `ext::` URL) are not overridden one by one, because git
/// keeps the first `uploadpack` it reads, which is the repository's own; refusing every transport stops them
/// before any program starts.
///
/// Fails closed: when git cannot list the repository's filters the command is refused,
/// because running without those overrides is exactly what this function exists to prevent.
pub fn command(root: &Path) -> Result<Command, String> {
    let mut overrides: Vec<(String, String)> = NEUTRALIZED.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for name in repo_filter_names(root)? {
        for var in ["clean", "smudge", "process"] {
            overrides.push((format!("filter.{name}.{var}"), String::new()));
        }
        overrides.push((format!("filter.{name}.required"), "false".into()));
    }
    let mut c = base(root);
    c.env("GIT_CONFIG_COUNT", overrides.len().to_string());
    for (i, (k, v)) in overrides.iter().enumerate() {
        c.env(format!("GIT_CONFIG_KEY_{i}"), k).env(format!("GIT_CONFIG_VALUE_{i}"), v);
    }
    Ok(c)
}

/// `git -C <root>` with the environment pinned: no system config, pager or prompt, no optional index
/// refresh (so read-only commands leave the real index alone), no external diff from the caller's env,
/// no lazy fetch of missing objects (`GIT_NO_LAZY_FETCH`, git 2.44+; older git ignores it and relies on
/// the refused transports in [`NEUTRALIZED`]) and no transport the caller did not choose.
fn base(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").env("GIT_PAGER", "cat").env("GIT_OPTIONAL_LOCKS", "0").env("GIT_NO_LAZY_FETCH", "1").env("GIT_PROTOCOL_FROM_USER", "0").env_remove("GIT_EXTERNAL_DIFF");
    c.stdin(Stdio::null());
    c
}

/// Older git ignores `GIT_CONFIG_COUNT`, which would silently leave every override of [`command`] off,
/// so a git before 2.31 is refused rather than trusted. Checked once per process.
fn require_env_config_support() -> Result<(), String> {
    static VERDICT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    VERDICT
        .get_or_init(|| {
            let out = Command::new("git").arg("--version").stdin(Stdio::null()).output().map_err(|e| format!("cannot run git: {e}"))?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if supports_env_config(&text) { Ok(()) } else { Err(format!("alsp needs git 2.31 or newer to neutralize repository config; found {:?}", text.trim())) }
        })
        .clone()
}

/// Whether `git --version` output names a git of at least 2.31 (`GIT_CONFIG_COUNT` exists).
fn supports_env_config(version_output: &str) -> bool {
    let mut nums = version_output.trim().strip_prefix("git version ").unwrap_or("").split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse::<u32>().ok());
    matches!((nums.next(), nums.next()), (Some(major), Some(minor)) if (major, minor) >= (2, 31))
}

/// Names of `filter.<name>` sections defined in the repository's own config (`local`, `worktree` or
/// `command` scope, includes followed). Reading config runs nothing. `git config` exits 1 when nothing
/// matches, which is a valid answer; any other failure is an error, never an empty list.
fn repo_filter_names(root: &Path) -> Result<Vec<String>, String> {
    let out = base(root).args(["config", "--show-scope", "--name-only", "--get-regexp", r"^filter\."]).output().map_err(|e| format!("cannot run git: {e}"))?;
    parse_filter_names(out.status.code(), &out.stdout, &out.stderr).map_err(|e| format!("cannot list the repository's git filters, so git was not run: {e}"))
}

/// Interprets `git config --show-scope --name-only --get-regexp` output. Exit 0 lists keys, exit 1 means
/// no matches, anything else (including death by signal) is a failure.
fn parse_filter_names(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<Vec<String>, String> {
    match code {
        Some(1) => return Ok(vec![]),
        Some(0) => {}
        other => return Err(format!("git config exited {other:?}: {}", String::from_utf8_lossy(stderr).trim())),
    }
    let mut names: Vec<String> = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(scope, _)| matches!(*scope, "local" | "worktree" | "command"))
        .filter_map(|(_, key)| key.strip_prefix("filter.")?.rsplit_once('.').map(|(n, _)| n.to_string()))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run_env(root, args, &[])
}

fn run_env(root: &Path, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Result<Vec<u8>, String> {
    let out = command(root)?
        .args(["-c", "core.quotepath=off", "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false"])
        .args(args)
        .envs(env.iter().copied())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("git {} failed: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn text(v: Vec<u8>) -> String {
    String::from_utf8_lossy(&v).trim().to_string()
}

impl Git {
    /// The work tree containing `path`.
    pub fn open(path: &Path) -> Result<Git, String> {
        if !path.exists() {
            return Err(format!("{} does not exist", path.display()));
        }
        require_env_config_support()?;
        let top = text(run(path, &["rev-parse", "--show-toplevel"]).map_err(|e| format!("{} is not a git work tree ({e})", path.display()))?);
        Ok(Git { root: std::fs::canonicalize(top).map_err(|e| e.to_string())? })
    }

    /// Full commit id of `rev`. Rejects names that look like options.
    pub fn resolve(&self, rev: &str) -> Result<String, String> {
        if rev.is_empty() || rev.starts_with('-') || rev.contains('\0') {
            return Err(format!("not a usable revision: {rev:?}"));
        }
        let spec = format!("{rev}^{{commit}}");
        let sha = text(run(&self.root, &["rev-parse", "--verify", "--quiet", "--end-of-options", &spec]).map_err(|_| format!("unknown revision: {rev}"))?);
        Ok(sha)
    }

    /// `git diff -U0 -M base head` as text. Both arguments are commit ids from `resolve`.
    pub fn diff(&self, base: &str, head: &str) -> Result<String, String> {
        run(&self.root, &["diff", "-U0", "-M", "--no-color", "--no-ext-diff", "--no-textconv", "--end-of-options", base, head, "--"]).map(text_keep)
    }

    /// File content at a commit, `None` when the path does not exist there.
    pub fn show(&self, sha: &str, path: &str) -> Option<String> {
        let spec = format!("{sha}:{path}");
        run(&self.root, &["show", "--no-ext-diff", "--no-textconv", "--end-of-options", &spec]).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Tracked Rust and Cargo files whose working-tree content differs from `HEAD`.
    pub fn dirty_sources(&self) -> Result<Vec<String>, String> {
        let out = run(&self.root, &["diff", "--name-only", "--no-ext-diff", "--no-textconv", "--ignore-submodules=all", "HEAD", "--"])?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter(|p| p.ends_with(".rs") || p.ends_with("Cargo.toml") || p.ends_with("Cargo.lock"))
            .map(str::to_string)
            .collect())
    }

    /// The checked-out branch's short name; `None` when HEAD is detached.
    pub fn branch(&self) -> Option<String> {
        run(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().map(text).filter(|b| !b.is_empty())
    }

    /// Best common ancestor of two commit ids from `resolve`; `None` when they share no history.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        run(&self.root, &["merge-base", a, b]).ok().map(text).filter(|s| !s.is_empty())
    }

    /// Whether commit id `ancestor` is reachable from commit id `descendant` (equal ids count).
    /// Both come from `resolve`. `git merge-base --is-ancestor` exits 1 for "no", anything else
    /// non-zero is a real failure.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        let out = command(&self.root)?
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!("git merge-base failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        }
    }

    /// The branch changes are usually merged into, as `(name, commit id)`: the target of
    /// `origin/HEAD`, else a local `main`, else `master`.
    pub fn default_branch(&self) -> Option<(String, String)> {
        if let Some(full) = run(&self.root, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]).ok().map(text) {
            if let Some(sha) = self.resolve(&full).ok() {
                return Some((full.strip_prefix("refs/remotes/").unwrap_or(&full).to_string(), sha));
            }
        }
        ["main", "master"].into_iter().find_map(|b| self.resolve(&format!("refs/heads/{b}")).ok().map(|sha| (b.to_string(), sha)))
    }

    /// Untracked files that are not ignored (`.alsp/` excluded), repo-relative.
    pub fn untracked_files(&self) -> Result<Vec<String>, String> {
        let out = run(&self.root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        Ok(out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).filter(|p| p != ".alsp" && !p.starts_with(".alsp/")).collect())
    }

    /// A commit holding the work tree as it is now, with `HEAD` as its parent: tracked files as edited or
    /// deleted, plus files staged as new. Untracked files are left out unless `include_untracked`, and
    /// then files that look like secrets or exceed [`MAX_UNTRACKED_BYTES`] are still left out. Returns the
    /// commit (`None` when the result matches `HEAD`) and one `path (reason)` entry per skipped untracked file.
    /// Built in a throwaway copy of the index, so neither the real index, the work tree nor any ref
    /// changes; the only trace is unreferenced objects in `.git` (collected by `git gc`). The commit is
    /// deterministic (fixed identity and date), so an unchanged work tree gives the same id.
    pub fn snapshot_worktree(&self, include_untracked: bool) -> Result<(Option<String>, Vec<String>), String> {
        let untracked_mode = if include_untracked { "--untracked-files=normal" } else { "--untracked-files=no" };
        let (mut skipped, mut chosen) = (Vec::new(), Vec::new());
        if include_untracked {
            for path in self.untracked_files()? {
                match skip_reason(&self.root, &path) {
                    Some(why) => skipped.push(format!("{path} ({why})")),
                    None => chosen.push(path),
                }
            }
        }
        let dirty = !run(&self.root, &["status", "--porcelain", "--ignore-submodules=all", untracked_mode, "--", ".", ":(exclude).alsp"])?.is_empty();
        if !dirty {
            return Ok((None, skipped));
        }
        let head = self.resolve("HEAD")?;
        let path_of = |what: &str| -> Result<PathBuf, String> {
            let p = PathBuf::from(text(run(&self.root, &["rev-parse", "--git-path", what])?));
            Ok(if p.is_absolute() { p } else { self.root.join(p) })
        };
        let pid = std::process::id();
        let (tmp_index, list) = (path_of(&format!("alsp-snapshot-index-{pid}"))?, path_of(&format!("alsp-snapshot-paths-{pid}"))?);
        let result = std::fs::write(&list, chosen.join("\0")).map_err(|e| format!("cannot write {}: {e}", list.display()))
            .and_then(|_| self.snapshot_with_index(&head, &path_of("index")?, &tmp_index, &list, !chosen.is_empty()));
        let _ = std::fs::remove_file(&tmp_index);
        let _ = std::fs::remove_file(&list);
        result.map(|c| (c, skipped))
    }

    fn snapshot_with_index(&self, head: &str, real_index: &Path, tmp_index: &Path, untracked_list: &Path, add_untracked: bool) -> Result<Option<String>, String> {
        // Start from a copy of the real index so unchanged files keep their cached stat data and staged
        // additions are kept. Without one (no index yet), start from HEAD's tree.
        if std::fs::copy(real_index, tmp_index).is_err() {
            let _ = std::fs::remove_file(tmp_index);
        }
        let env: [(&str, &std::ffi::OsStr); 2] = [("GIT_INDEX_FILE", tmp_index.as_os_str()), ("GIT_LITERAL_PATHSPECS", "1".as_ref())];
        if !tmp_index.exists() {
            run_env(&self.root, &["read-tree", "--end-of-options", head], &env)?;
        }
        // `--update` touches only paths already in the index: edits and deletions, never new files. Submodules
        // are left out: git would look inside each one, under that submodule's own config, whose clean filters
        // and other programs alsp cannot name ahead of time. Their recorded commit stays as in `HEAD`.
        let mut update: Vec<String> = ["add", "--update", "--", "."].map(String::from).into();
        let staged = run_env(&self.root, &["ls-files", "--stage", "-z"], &env)?;
        update.extend(gitlink_paths(&staged).into_iter().map(|p| format!(":(exclude,literal){p}")));
        let update: Vec<&str> = update.iter().map(String::as_str).collect();
        run_env(&self.root, &update, &[env[0], ("GIT_LITERAL_PATHSPECS", "0".as_ref())])?;
        if add_untracked {
            let from = format!("--pathspec-from-file={}", untracked_list.display());
            run_env(&self.root, &["add", &from, "--pathspec-file-nul"], &env)?;
        }
        let tree = text(run_env(&self.root, &["write-tree"], &env)?);
        if tree == text(run(&self.root, &["rev-parse", "--verify", &format!("{head}^{{tree}}")])?) {
            return Ok(None);
        }
        let id: [(&str, &std::ffi::OsStr); 6] = [
            ("GIT_AUTHOR_NAME", "alsp".as_ref()),
            ("GIT_AUTHOR_EMAIL", "alsp@localhost".as_ref()),
            ("GIT_AUTHOR_DATE", "1970-01-01T00:00:00Z".as_ref()),
            ("GIT_COMMITTER_NAME", "alsp".as_ref()),
            ("GIT_COMMITTER_EMAIL", "alsp@localhost".as_ref()),
            ("GIT_COMMITTER_DATE", "1970-01-01T00:00:00Z".as_ref()),
        ];
        let commit = run_env(&self.root, &["-c", "commit.gpgsign=false", "commit-tree", &tree, "-p", head, "-m", "alsp work tree snapshot"], &id)?;
        Ok(Some(text(commit)))
    }

    /// Unpack the tree of `sha` into `dest` (created; must not exist) without touching the work tree.
    pub fn export(&self, sha: &str, dest: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
        let mut archive = command(&self.root)?
            .args(["archive", "--format=tar", "--end-of-options", sha])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git archive: {e}"))?;
        let tar = Command::new("tar")
            .arg("-xf")
            .arg("-")
            .arg("-C")
            .arg(dest)
            .stdin(archive.stdout.take().expect("stdout is piped"))
            .output()
            .map_err(|e| format!("cannot run tar: {e}"))?;
        let a = archive.wait_with_output().map_err(|e| e.to_string())?;
        if !a.status.success() {
            return Err(format!("git archive failed: {}", String::from_utf8_lossy(&a.stderr).trim()));
        }
        if !tar.status.success() {
            return Err(format!("tar failed: {}", String::from_utf8_lossy(&tar.stderr).trim()));
        }
        Ok(())
    }
}

/// Paths of submodule entries (mode 160000) in `git ls-files --stage -z` output.
fn gitlink_paths(staged: &[u8]) -> Vec<String> {
    staged
        .split(|&b| b == 0)
        .filter_map(|rec| rec.strip_prefix(b"160000 "))
        .filter_map(|rec| rec.iter().position(|&b| b == b'\t').map(|t| String::from_utf8_lossy(&rec[t + 1..]).into_owned()))
        .collect()
}

/// Untracked files above this size are never sent for review.
pub const MAX_UNTRACKED_BYTES: u64 = 1024 * 1024;

/// Why an untracked file must not be included in a snapshot: it looks like a credential (by file name,
/// any letter case) or is too large. `None` when it may be included.
fn skip_reason(root: &Path, rel: &str) -> Option<String> {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    let secret = name == ".env" || name.starts_with(".env.") || name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12")
        || name.starts_with("id_rsa") || name.starts_with("id_ed25519") || name.starts_with("credentials") || name.contains("secret");
    if secret {
        return Some("looks like a secret".into());
    }
    match std::fs::symlink_metadata(root.join(rel)) {
        Ok(m) if m.len() > MAX_UNTRACKED_BYTES => Some(format!("over {} MB", MAX_UNTRACKED_BYTES / 1024 / 1024)),
        Ok(_) => None,
        Err(e) => Some(format!("unreadable: {e}")),
    }
}

/// Diff text must keep its leading spaces and blank lines; only the final newline is irrelevant.
fn text_keep(v: Vec<u8>) -> String {
    String::from_utf8_lossy(&v).into_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
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

    /// A repo with two commits: c1 has `a.rs`, c2 edits it and adds `Cargo.toml`.
    pub(crate) fn repo() -> (tempfile::TempDir, Git, String, String) {
        let d = tempfile::tempdir().unwrap();
        sh(d.path(), &["init", "-q", "-b", "main"]);
        fs::write(d.path().join("a.rs"), "fn a() {}\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "c1"]);
        let c1 = sh(d.path(), &["rev-parse", "HEAD"]);
        fs::write(d.path().join("a.rs"), "fn a() { 1; }\n").unwrap();
        fs::write(d.path().join("Cargo.toml"), "[package]\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "c2"]);
        let c2 = sh(d.path(), &["rev-parse", "HEAD"]);
        let g = Git::open(d.path()).unwrap();
        (d, g, c1, c2)
    }

    #[test]
    fn open_resolve_diff_show() {
        let (d, g, c1, c2) = repo();
        assert_eq!(g.root, fs::canonicalize(d.path()).unwrap());
        assert_eq!(g.resolve("HEAD").unwrap(), c2);
        assert_eq!(g.resolve("HEAD~1").unwrap(), c1);
        assert_eq!(g.resolve(&c1[..8]).unwrap(), c1);
        let diff = g.diff(&c1, &c2).unwrap();
        assert!(diff.contains("diff --git a/a.rs b/a.rs") && diff.contains("@@ -1 +1 @@"), "{diff}");
        assert_eq!(g.show(&c1, "a.rs").unwrap(), "fn a() {}\n");
        assert!(g.show(&c1, "Cargo.toml").is_none());
        assert!(g.show(&c1, "nope.rs").is_none());
        // a subdirectory opens the same work tree
        fs::create_dir(d.path().join("sub")).unwrap();
        assert_eq!(Git::open(&d.path().join("sub")).unwrap().root, g.root);
    }

    #[test]
    fn hostile_and_unknown_revisions_are_rejected_before_git_sees_them() {
        let (_d, g, _, _) = repo();
        for bad in ["", "--output=/tmp/evil", "-p", "nope", "a\0b", "HEAD; touch /tmp/x"] {
            assert!(g.resolve(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn not_a_repo_and_missing_path() {
        let d = tempfile::tempdir().unwrap();
        assert!(Git::open(d.path()).unwrap_err().contains("not a git work tree"));
        assert!(Git::open(&d.path().join("missing")).unwrap_err().contains("does not exist"));
    }

    #[test]
    fn dirty_sources_lists_changed_rust_and_cargo_files_only() {
        let (d, g, _, _) = repo();
        assert!(g.dirty_sources().unwrap().is_empty());
        fs::write(d.path().join("a.rs"), "fn a() { 2; }\n").unwrap();
        fs::write(d.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(d.path().join("untracked.rs"), "x").unwrap();
        let mut dirty = g.dirty_sources().unwrap();
        dirty.sort();
        assert_eq!(dirty, vec!["Cargo.toml", "a.rs"]);
    }

    #[test]
    fn export_unpacks_a_commit_without_touching_the_work_tree() {
        let (d, g, c1, _) = repo();
        let dest = tempfile::tempdir().unwrap();
        let out = dest.path().join("tree");
        g.export(&c1, &out).unwrap();
        assert_eq!(fs::read_to_string(out.join("a.rs")).unwrap(), "fn a() {}\n");
        assert!(!out.join("Cargo.toml").exists());
        assert_eq!(fs::read_to_string(d.path().join("a.rs")).unwrap(), "fn a() { 1; }\n");
        assert!(g.export("deadbeef", &dest.path().join("bad")).unwrap_err().contains("git archive failed"));
    }

    #[test]
    fn branch_names_the_checked_out_branch_and_is_none_when_detached() {
        let (d, g, c1, _) = repo();
        assert_eq!(g.branch().as_deref(), Some("main"));
        sh(d.path(), &["checkout", "-q", "--detach", &c1]);
        assert_eq!(g.branch(), None);
    }

    #[test]
    fn merge_base_and_is_ancestor_follow_history() {
        let (d, g, c1, c2) = repo();
        assert_eq!(g.merge_base(&c1, &c2).as_deref(), Some(c1.as_str()));
        assert!(g.is_ancestor(&c1, &c2).unwrap());
        assert!(g.is_ancestor(&c2, &c2).unwrap(), "equal ids count");
        assert!(!g.is_ancestor(&c2, &c1).unwrap());
        // A separate root commit shares no history.
        sh(d.path(), &["checkout", "-q", "--orphan", "island"]);
        fs::write(d.path().join("i.rs"), "").unwrap();
        sh(d.path(), &["add", "i.rs"]);
        sh(d.path(), &["commit", "-q", "-m", "island"]);
        let island = sh(d.path(), &["rev-parse", "HEAD"]);
        assert_eq!(g.merge_base(&island, &c2), None);
        // Not a commit id: a real failure, not "no".
        assert!(g.is_ancestor("0000000000000000000000000000000000000000", &c2).is_err());
    }

    #[test]
    fn default_branch_prefers_origin_head_then_main_then_master() {
        let (d, g, c1, c2) = repo();
        assert_eq!(g.default_branch(), Some(("main".into(), c2.clone())));
        sh(d.path(), &["branch", "-m", "main", "master"]);
        assert_eq!(g.default_branch(), Some(("master".into(), c2.clone())));
        sh(d.path(), &["update-ref", "refs/remotes/origin/trunk", &c1]);
        sh(d.path(), &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
        assert_eq!(g.default_branch(), Some(("origin/trunk".into(), c1)));
        sh(d.path(), &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        sh(d.path(), &["branch", "-m", "master", "work"]);
        assert_eq!(g.default_branch(), None);
    }

    #[test]
    fn snapshot_is_none_for_a_clean_tree_and_for_ignored_or_alsp_only_changes() {
        let (d, g, _, _) = repo();
        assert_eq!(g.snapshot_worktree(true).unwrap().0, None);
        fs::write(d.path().join(".gitignore"), "skip.rs\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "ignore"]);
        fs::write(d.path().join("skip.rs"), "ignored").unwrap();
        fs::create_dir(d.path().join(".alsp")).unwrap();
        fs::write(d.path().join(".alsp/queue"), "x").unwrap();
        assert_eq!(g.snapshot_worktree(true).unwrap().0, None);
    }

    #[test]
    fn snapshot_holds_edits_deletions_and_untracked_files_on_top_of_head_and_touches_nothing() {
        let (d, g, _, c2) = repo();
        fs::write(d.path().join("a.rs"), "fn a() { 2; }\n").unwrap();
        fs::remove_file(d.path().join("Cargo.toml")).unwrap();
        fs::write(d.path().join("new.rs"), "fn n() {}\n").unwrap();
        let before = (sh(d.path(), &["status", "--porcelain"]), sh(d.path(), &["for-each-ref"]));
        let snap = g.snapshot_worktree(true).unwrap().0.unwrap();
        assert_eq!(sh(d.path(), &["rev-parse", &format!("{snap}^")]), c2, "HEAD is the parent");
        assert_eq!(g.show(&snap, "a.rs").unwrap(), "fn a() { 2; }\n");
        assert_eq!(g.show(&snap, "new.rs").unwrap(), "fn n() {}\n");
        assert!(g.show(&snap, "Cargo.toml").is_none(), "deletion is in the snapshot");
        assert_eq!(g.snapshot_worktree(true).unwrap().0.unwrap(), snap, "same tree, same commit id");
        assert_eq!((sh(d.path(), &["status", "--porcelain"]), sh(d.path(), &["for-each-ref"])), before);
        assert_eq!(sh(d.path(), &["diff", "--cached", "--name-only"]), "", "the real index is untouched");
        // The diff the review uses shows exactly the dirty work.
        let diff = g.diff(&c2, &snap).unwrap();
        assert!(diff.contains("a/new.rs") && diff.contains("a/Cargo.toml") && diff.contains("@@ -1 +1 @@"), "{diff}");
    }

    /// A developer's global ignore file may list `.env` or `*.pem`; this repo's own rules override it.
    fn unignore_secrets(d: &Path) {
        fs::create_dir_all(d.join(".git/info")).unwrap();
        fs::write(d.join(".git/info/exclude"), "!.env\n!.env.*\n!*.pem\n!*.key\n!*.p12\n!*secret*\n!*Secret*\n!credentials*\n!id_*\n").unwrap();
    }

    #[test]
    fn snapshot_without_untracked_holds_edits_deletions_and_staged_additions_only() {
        let (d, g, _, c2) = repo();
        unignore_secrets(d.path());
        fs::write(d.path().join("a.rs"), "fn a() { 3; }\n").unwrap();
        fs::write(d.path().join("staged.rs"), "s").unwrap();
        sh(d.path(), &["add", "staged.rs"]);
        fs::write(d.path().join("untracked.rs"), "u").unwrap();
        fs::write(d.path().join(".env"), "TOKEN=1").unwrap();
        let (snap, skipped) = g.snapshot_worktree(false).unwrap();
        let snap = snap.unwrap();
        assert!(skipped.is_empty());
        assert_eq!(g.show(&snap, "a.rs").unwrap(), "fn a() { 3; }\n");
        assert_eq!(g.show(&snap, "staged.rs").unwrap(), "s");
        assert!(g.show(&snap, "untracked.rs").is_none() && g.show(&snap, ".env").is_none());
        assert_eq!(sh(d.path(), &["rev-parse", &format!("{snap}^")]), c2);
        let mut left = g.untracked_files().unwrap();
        left.sort();
        assert_eq!(left, [".env", "untracked.rs"]);
        // Only untracked files: nothing to snapshot by default.
        sh(d.path(), &["reset", "-q", "--hard"]);
        assert_eq!(g.snapshot_worktree(false).unwrap().0, None);
    }

    #[test]
    fn snapshot_with_untracked_skips_secret_names_and_big_files_and_lists_them() {
        let (d, g, _, _) = repo();
        unignore_secrets(d.path());
        for secret in [".env", ".env.local", "k.pem", "k.KEY", "id_rsa", "id_rsa.pub", "id_ed25519", "x.p12", "credentials.json", "my_Secret_notes.txt", "sub/.env"] {
            let p = d.path().join(secret);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "s").unwrap();
        }
        fs::write(d.path().join("env.rs"), "fine").unwrap();
        fs::write(d.path().join("big.rs"), vec![b'x'; MAX_UNTRACKED_BYTES as usize + 1]).unwrap();
        fs::write(d.path().join("exact.rs"), vec![b'x'; MAX_UNTRACKED_BYTES as usize]).unwrap();
        let (snap, skipped) = g.snapshot_worktree(true).unwrap();
        let snap = snap.unwrap();
        assert_eq!(skipped.len(), 12, "{skipped:?}");
        assert!(skipped.iter().any(|s| s == "big.rs (over 1 MB)") && skipped.iter().any(|s| s == "sub/.env (looks like a secret)"));
        assert!(g.show(&snap, "env.rs").is_some() && g.show(&snap, "exact.rs").is_some(), "plain files and files at the limit are kept");
        for gone in [".env", "k.pem", "id_rsa.pub", "big.rs", "sub/.env"] { assert!(g.show(&snap, gone).is_none(), "{gone}"); }
        // Everything skipped: no snapshot, but the list is still returned.
        sh(d.path(), &["clean", "-fdq"]);
        fs::write(d.path().join(".env"), "x").unwrap();
        let (none, skipped) = g.snapshot_worktree(true).unwrap();
        assert_eq!((none, skipped.len()), (None, 1));
    }

    #[test]
    fn snapshot_falls_back_to_read_tree_when_the_index_cannot_be_copied_and_errors_without_commits() {
        let (d, g, _, c2) = repo();
        fs::write(d.path().join("new.rs"), "x").unwrap();
        // Missing real index: the copy fails and the snapshot starts from HEAD's tree instead.
        let tmp = d.path().join(".git/alsp-test-index");
        let list = d.path().join(".git/alsp-test-list");
        fs::write(&list, "new.rs").unwrap();
        let snap = g.snapshot_with_index(&c2, &d.path().join(".git/no-such-index"), &tmp, &list, true).unwrap().unwrap();
        assert_eq!(g.show(&snap, "new.rs").unwrap(), "x");
        assert!(g.show(&snap, "a.rs").is_some());
        let _ = fs::remove_file(&tmp);
        let empty = tempfile::tempdir().unwrap();
        sh(empty.path(), &["init", "-q", "-b", "main"]);
        fs::write(empty.path().join("f.rs"), "x").unwrap();
        assert!(Git::open(empty.path()).unwrap().snapshot_worktree(true).unwrap_err().contains("unknown revision: HEAD"));
    }

    // --- hardening against the repository's own config ---

    /// Plants programs that append their name to the returned marker file, in `.git/config`, an included
    /// file, and `.gitattributes` (committed in the next commit by the caller's `sh`).
    fn plant(d: &Path) -> PathBuf {
        let marks = d.join(".git").join("marks");
        let mark = d.join(".git").join("mark.sh");
        fs::write(&mark, format!("#!/bin/sh\necho \"$1\" >> {}\ncat\n", marks.display())).unwrap();
        fs::set_permissions(&mark, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let inc = d.join(".git").join("inc.cfg");
        fs::write(&inc, format!("[filter \"a.b=c\"]\n\tclean = {} dotted-clean\n", mark.display())).unwrap();
        let m = mark.display();
        for (k, v) in [
            ("core.fsmonitor", format!("{m} fsmonitor")),
            ("diff.external", format!("{m} extdiff")),
            ("diff.evil.textconv", format!("{m} textconv")),
            ("filter.evil.clean", format!("{m} clean")),
            ("filter.evil.smudge", format!("{m} smudge")),
            ("filter.evil.process", format!("{m} process")),
            ("include.path", inc.display().to_string()),
        ] {
            sh(d, &["config", "--local", k, &v]);
        }
        marks
    }

    #[test]
    fn every_git_method_ignores_programs_named_in_the_repository_config() {
        let (d, g, c1, c2) = repo();
        fs::write(d.path().join(".gitattributes"), "*.rs filter=evil diff=evil\n*.txt filter=a.b=c\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "attrs"]);
        let head = sh(d.path(), &["rev-parse", "HEAD"]);
        let marks = plant(d.path());
        fs::write(d.path().join("a.rs"), "fn a() { 2; }\n").unwrap();
        fs::write(d.path().join("n.txt"), "new\n").unwrap();
        // Control: plain git does run them, so an empty marker file below means hardened, not blind.
        let _ = Command::new("git").arg("-C").arg(d.path()).args(["status", "--porcelain"]).output();
        let _ = Command::new("git").arg("-C").arg(d.path()).args(["diff", "--no-ext-diff", "--textconv", &c1, &c2]).output();
        let live = fs::read_to_string(&marks).unwrap_or_default();
        assert!(live.contains("fsmonitor") && live.contains("textconv"), "control did not fire: {live:?}");
        fs::remove_file(&marks).unwrap();

        g.resolve("HEAD").unwrap();
        g.diff(&c1, &c2).unwrap();
        g.show(&c1, "a.rs").unwrap();
        g.dirty_sources().unwrap();
        g.branch();
        g.merge_base(&c1, &c2).unwrap();
        assert!(g.is_ancestor(&c1, &head).unwrap());
        g.default_branch();
        g.untracked_files().unwrap();
        let (snap, _) = g.snapshot_worktree(true).unwrap();
        assert!(snap.is_some(), "the edit must still reach the snapshot");
        let out = tempfile::tempdir().unwrap();
        g.export(&head, &out.path().join("tree")).unwrap();
        assert_eq!(fs::read_to_string(&marks).unwrap_or_default(), "", "a git method ran a program named in .git/config");
    }

    #[test]
    fn git_older_than_2_31_or_unreadable_is_refused() {
        for ok in ["git version 2.31.0", "git version 2.50.1 (Apple Git-155)\n", "git version 3.0.0", "git version 2.100.2"] {
            assert!(supports_env_config(ok), "{ok}");
        }
        for bad in ["git version 2.30.9", "git version 1.9.0", "", "not git", "git version x.y"] {
            assert!(!supports_env_config(bad), "{bad}");
        }
    }

    #[test]
    fn hardened_commands_empty_every_repository_filter_but_only_those() {
        let (d, _g, _, _) = repo();
        let _ = plant(d.path());
        // A filter from trusted global config must keep working: simulate with a global file.
        let global = d.path().join(".git").join("global.cfg");
        fs::write(&global, "[filter \"lfs\"]\n\tclean = git-lfs clean %f\n").unwrap();
        let get = |key: &str| String::from_utf8_lossy(&command(d.path()).unwrap().env("GIT_CONFIG_GLOBAL", &global).args(["config", "--get", key]).output().unwrap().stdout).trim().to_string();
        for key in ["filter.evil.clean", "filter.evil.smudge", "filter.evil.process", "filter.a.b=c.clean"] {
            assert_eq!(get(key), "", "{key} not emptied");
        }
        assert_eq!(get("filter.lfs.clean"), "git-lfs clean %f", "a filter from trusted global config must be left alone");
    }

    /// A superproject `d` with a submodule `sub` (own gitdir under `.git/modules/sub`) whose config
    /// plants programs that append to the returned marker file; `sub/.gitattributes` routes `*.txt`
    /// to the filter named `subevil`, which only the submodule's config defines. `sub/f.txt` is then
    /// edited so any status or diff of the superproject has a reason to look inside the submodule.
    fn submodule_repo() -> (tempfile::TempDir, tempfile::TempDir, Git, PathBuf) {
        let (d, g, _, _) = repo();
        let src = tempfile::tempdir().unwrap();
        sh(src.path(), &["init", "-q", "-b", "main"]);
        fs::write(src.path().join(".gitattributes"), "*.txt filter=subevil\n").unwrap();
        fs::write(src.path().join("f.txt"), "one\n").unwrap();
        sh(src.path(), &["add", "."]);
        sh(src.path(), &["commit", "-q", "-m", "s1"]);
        sh(d.path(), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &src.path().display().to_string(), "sub"]);
        sh(d.path(), &["commit", "-q", "-m", "add sub"]);
        let marks = d.path().join(".git").join("marks");
        let mark = d.path().join(".git").join("mark.sh");
        fs::write(&mark, format!("#!/bin/sh\necho \"$1\" >> {}\ncat\n", marks.display())).unwrap();
        fs::set_permissions(&mark, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let m = mark.display();
        let sub_git = d.path().join(".git/modules/sub");
        for (k, v) in [("core.fsmonitor", format!("{m} sub-fsmonitor")), ("filter.subevil.clean", format!("{m} sub-clean")), ("filter.subevil.smudge", format!("{m} sub-smudge"))] {
            sh(&sub_git, &["config", "--local", k, &v]);
        }
        fs::write(d.path().join("sub/f.txt"), "two\n").unwrap();
        (d, src, g, marks)
    }

    #[test]
    fn submodule_config_programs_never_run_from_any_git_method() {
        let (d, _src, g, marks) = submodule_repo();
        let _ = Command::new("git").arg("-C").arg(d.path()).args(["status", "--porcelain"]).output();
        let live = fs::read_to_string(&marks).unwrap_or_default();
        assert!(live.contains("sub-fsmonitor") && live.contains("sub-clean"), "control did not fire: {live:?}");
        fs::remove_file(&marks).unwrap();
        fs::write(d.path().join("a.rs"), "fn a() { 9; }\n").unwrap();
        g.dirty_sources().unwrap();
        g.untracked_files().unwrap();
        let snap = g.snapshot_worktree(true).unwrap().0.expect("the edit must reach the snapshot");
        assert_eq!(g.show(&snap, "a.rs").unwrap(), "fn a() { 9; }\n");
        assert_eq!(sh(d.path(), &["rev-parse", &format!("{snap}:sub")]), sh(d.path(), &["rev-parse", "HEAD:sub"]), "the submodule's recorded commit is kept");
        g.diff("HEAD~1", "HEAD").unwrap();
        let out = tempfile::tempdir().unwrap();
        g.export("HEAD", &out.path().join("t")).unwrap();
        assert_eq!(fs::read_to_string(&marks).unwrap_or_default(), "", "a git method ran a program named in a submodule's config");
    }

    #[test]
    fn config_enumeration_distinguishes_no_matches_from_failure() {
        // Exit 1 is git's "no matches": a valid empty answer.
        assert_eq!(parse_filter_names(Some(1), b"", b"").unwrap(), Vec::<String>::new());
        // Any other exit, or death by signal, fails closed with git's own message.
        assert!(parse_filter_names(Some(128), b"", b"fatal: bad config line 1").unwrap_err().contains("bad config line 1"));
        assert!(parse_filter_names(Some(3), b"", b"").is_err() && parse_filter_names(None, b"", b"").is_err());
        // Listing: scope is honored; names may hold dots and `=`; duplicates collapse.
        let listing = b"local\tfilter.a.b=c.clean\nlocal\tfilter.a.b=c.smudge\nworktree\tfilter.z.process\nglobal\tfilter.lfs.clean\ncommand\tfilter.cmd.clean\nnoscope\n";
        assert_eq!(parse_filter_names(Some(0), listing, b"").unwrap(), ["a.b=c", "cmd", "z"]);
    }

    #[test]
    fn a_repository_with_no_filters_or_remotes_is_not_a_failure_but_a_broken_config_or_directory_is_refused() {
        let (d, g, _, _) = repo();
        assert!(command(d.path()).is_ok(), "no filter defined (git exits 1) must still run");
        // A config git cannot parse: git config exits non-zero, so nothing may run unhardened.
        fs::write(d.path().join(".git/config"), "[core\nbare = false\n[filter \"evil\"\n").unwrap();
        let err = command(d.path()).unwrap_err();
        assert!(err.contains("cannot list the repository's git filters") && err.contains("git was not run"), "{err}");
        assert!(g.resolve("HEAD").is_err());
        assert!(g.snapshot_worktree(true).is_err() && g.dirty_sources().is_err() && g.untracked_files().is_err());
        assert!(g.is_ancestor("a", "b").is_err() && g.export("HEAD", &d.path().join("out")).is_err());
        // A directory that does not exist is refused too.
        assert!(command(&d.path().join("missing")).is_err());
    }

    #[test]
    fn every_transport_is_refused_whatever_the_repository_config_allows() {
        let (d, _g, _, _) = repo();
        sh(d.path(), &["config", "--local", "remote.up.stream.url", "ext::sh -c evil"]);
        sh(d.path(), &["config", "--local", "protocol.ext.allow", "always"]);
        sh(d.path(), &["config", "--local", "protocol.file.allow", "always"]);
        let get = |key: &str| String::from_utf8_lossy(&command(d.path()).unwrap().args(["config", "--get", key]).output().unwrap().stdout).trim().to_string();
        for p in ["ext", "file", "ssh", "git", "http", "https"] {
            assert_eq!(get(&format!("protocol.{p}.allow")), "never", "{p}");
        }
        assert_eq!((get("protocol.allow"), get("submodule.recurse")), ("never".into(), "false".into()));
        let env: Vec<_> = command(d.path()).unwrap().get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        assert!(env.contains(&("GIT_NO_LAZY_FETCH".into(), Some("1".into()))) && env.contains(&("GIT_PROTOCOL_FROM_USER".into(), Some("0".into()))));
    }

    /// An origin that serves any object, and a partial clone of it (no blobs, no checkout) whose remote
    /// `origin` uses a planted `uploadpack` that records itself, then runs the real one. Any lazy fetch
    /// of a missing blob goes through that program. Returns (origin, clone, marker file).
    fn promisor_clone(c1_c2_origin: &Path, dest: &Path) -> PathBuf {
        let marks = dest.join(".git").join("promisor-marks");
        let prog = dest.join(".git").join("uploadpack.sh");
        sh(dest.parent().unwrap(), &["clone", "-q", "--no-checkout", "--filter=blob:none", &format!("file://{}", c1_c2_origin.display()), &dest.display().to_string()]);
        fs::write(&prog, format!("#!/bin/sh\necho uploadpack >> {}\nexec git-upload-pack \"$@\"\n", marks.display())).unwrap();
        fs::set_permissions(&prog, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        sh(dest, &["config", "--local", "remote.origin.uploadpack", &prog.display().to_string()]);
        marks
    }

    #[test]
    fn a_missing_object_in_a_partial_clone_is_never_fetched_through_the_repositorys_remote_config() {
        let (origin, _g, c1, c2) = repo();
        sh(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        sh(origin.path(), &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let work = tempfile::tempdir().unwrap();
        // Control: plain git fetches the missing blob and so runs the planted program.
        let control = work.path().join("control");
        let marks = promisor_clone(origin.path(), &control);
        assert_eq!(sh(&control, &["config", "remote.origin.promisor"]), "true", "the clone must be a promisor clone");
        let shown = Command::new("git").arg("-C").arg(&control).args(["show", &format!("{c1}:a.rs")]).output().unwrap();
        assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
        assert!(fs::read_to_string(&marks).unwrap_or_default().contains("uploadpack"), "control did not fire: the probe is blind");
        // Hardened: every method that can read a missing blob fails or answers "absent" without fetching.
        let victim = work.path().join("victim");
        let marks = promisor_clone(origin.path(), &victim);
        let g = Git::open(&victim).unwrap();
        assert!(g.show(&c1, "a.rs").is_none(), "the blob is missing and must stay missing");
        assert!(g.diff(&c1, &c2).is_err());
        assert!(g.export(&c2, &work.path().join("tree")).is_err());
        fs::write(victim.join("a.rs"), "fn a() { 5; }\n").unwrap();
        let _ = g.snapshot_worktree(true);
        let _ = g.dirty_sources();
        let _ = g.untracked_files();
        assert_eq!(fs::read_to_string(&marks).unwrap_or_default(), "", "a git method fetched through the repository's remote program");
        let has_blob = Command::new("git").arg("-C").arg(&victim).args(["cat-file", "-e", &format!("{c1}:a.rs")]).env("GIT_NO_LAZY_FETCH", "1").output().unwrap();
        assert!(!has_blob.status.success(), "the blob was fetched anyway");
    }

    #[test]
    fn the_transport_block_alone_stops_a_lazy_fetch_on_a_git_without_no_lazy_fetch() {
        let (origin, _g, c1, _) = repo();
        sh(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        sh(origin.path(), &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let work = tempfile::tempdir().unwrap();
        let v = work.path().join("v");
        let marks = promisor_clone(origin.path(), &v);
        // What git before 2.44 does: it ignores GIT_NO_LAZY_FETCH, so only the refused transport remains.
        let out = command(&v).unwrap().env_remove("GIT_NO_LAZY_FETCH").args(["show", &format!("{c1}:a.rs")]).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("transport 'file' not allowed"), "{err}");
        assert_eq!(fs::read_to_string(&marks).unwrap_or_default(), "", "the remote's program ran");
    }

    #[test]
    fn gitlink_paths_lists_only_submodule_entries() {
        let staged = b"100644 aaaa 0\ta.rs\x00160000 bbbb 0\tsub dir/with space\x00160000 cccc 0\tx\x00100755 dddd 0\t160000 trap\x00";
        assert_eq!(gitlink_paths(staged), ["sub dir/with space", "x"]);
        assert!(gitlink_paths(b"").is_empty());
    }
}
