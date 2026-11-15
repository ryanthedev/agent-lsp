//! A repository's own `.git/config` can name programs git will run (fsmonitor hook, external diff,
//! textconv, clean/smudge/process filters, a gpg program). A repository received as an archive carries
//! one. These tests plant such a config whose programs append a line to a marker file, then show that
//! the commands the docs call safe on untrusted code (`sym`/`refs` with `--no-build`, `pack` without
//! `--with`, the ad-hoc `review` snapshot) never trigger any of them. Each trigger is first proved live
//! by running plain, unhardened git, so an empty marker file means "hardened", not "probe never fires".
//! Two more places a repository can hide a program are covered the same way: a submodule's own config
//! and filter (git looks inside submodules under their config), and a partial clone whose remote config
//! names a program that git runs when it fetches a missing object.
#![allow(non_snake_case)]

use alsp_evidence::testkit::scip_for;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Ignore the developer's global and system git config.
fn hermetic(c: &mut Command) -> &mut Command {
    c.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").env("XDG_CONFIG_HOME", "/nonexistent")
}
fn git_env(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    hermetic(&mut c).output().unwrap()
}
fn git(dir: &Path, args: &[&str]) -> String {
    let o = git_env(dir, args, &[]);
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().into()
}
fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn commit(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// What the planted programs recorded, as a set of trigger names.
fn fired(marks: &Path) -> BTreeSet<String> {
    fs::read_to_string(marks).unwrap_or_default().lines().map(str::to_string).collect()
}

/// Plants the hostile config in `repo/.git/config` (and an included file). Every program appends its
/// trigger name to `<scratch>/marks`. `.gitattributes` must already route `*.txt` to the `evil`
/// filter and diff driver and `*.md` to the `inc` filter and `*.dat` to the `proc` (process) filter.
fn plant(repo: &Path, scratch: &Path) -> PathBuf {
    let marks = scratch.join("marks");
    let (mark, gpg) = (scratch.join("mark.sh"), scratch.join("gpg.sh"));
    script(&mark, &format!("echo \"$1\" >> {}\ncat", marks.display()));
    script(&gpg, &format!("echo gpg >> {}\nexit 1", marks.display()));
    let m = mark.display();
    let included = scratch.join("included.cfg");
    fs::write(&included, format!("[filter \"inc\"]\n\tclean = {m} inc-clean\n\tsmudge = {m} inc-smudge\n")).unwrap();
    for (k, v) in [
        ("core.fsmonitor", format!("{m} fsmonitor")),
        ("core.hooksPath", scratch.join("nohooks").display().to_string()),
        ("core.pager", format!("{m} pager")),
        ("core.sshCommand", format!("{m} ssh")),
        ("diff.external", format!("{m} extdiff")),
        ("diff.evil.textconv", format!("{m} textconv")),
        ("filter.evil.clean", format!("{m} clean")),
        ("filter.evil.smudge", format!("{m} smudge")),
        ("filter.proc.process", format!("{m} process")),
        ("commit.gpgsign", "true".into()),
        ("gpg.program", gpg.display().to_string()),
        ("include.path", included.display().to_string()),
    ] {
        git(repo, &["config", "--local", k, &v]);
    }
    marks
}

/// Plain git, no hardening: each trigger must fire here (`gpg` is planted too, but `commit-tree` signs only
/// when given `-S`, which alsp never passes, so it has no live control), or the probes below prove nothing.
fn prove_triggers_fire(repo: &Path, marks: &Path, old: &str, new: &str) {
    let _ = fs::remove_file(marks);
    let _ = git_env(repo, &["status", "--porcelain"], &[]); // fsmonitor
    let idx = repo.join(".git/probe-index");
    fs::copy(repo.join(".git/index"), &idx).unwrap();
    let _ = git_env(repo, &["add", "--update"], &[("GIT_INDEX_FILE", &idx)]); // clean filters
    let _ = fs::remove_file(&idx);
    let _ = git_env(repo, &["diff", old, new], &[]); // diff.external
    let _ = git_env(repo, &["diff", "--no-ext-diff", "--textconv", old, new], &[]); // textconv
    let got = fired(marks);
    for t in ["fsmonitor", "clean", "process", "inc-clean", "extdiff", "textconv"] {
        assert!(got.contains(t), "trigger {t} did not fire under plain git, so the probe is blind: {got:?}");
    }
    let _ = fs::remove_file(marks);
}

const LIB1: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    1;\n}\n";
const LIB2: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    2;\n}\n";
const ATTRS: &str = "*.txt filter=evil diff=evil\n*.md filter=inc\n*.dat filter=proc\n";

struct Env {
    repo: tempfile::TempDir,
    fx: tempfile::TempDir,
    cache: tempfile::TempDir,
    scratch: tempfile::TempDir,
    ra: PathBuf,
}

impl Env {
    /// Two commits (c1 -> c2 edits `helper`, `notes.txt`), indexes cached for both, then the hostile
    /// config planted and the work tree dirtied (tracked `.txt`/`.md` edits plus untracked files).
    fn new() -> (Env, PathBuf) {
        let (repo, fx) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ra = fx.path().join("ra.sh");
        script(&ra, "if [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\nname=$(basename \"$2\")\nif [ -f \"$ALSP_TEST_FX/$name.scip\" ]; then cp \"$ALSP_TEST_FX/$name.scip\" \"$4\"; else cp \"$ALSP_TEST_FX/default.scip\" \"$4\"; fi");
        fs::write(fx.path().join("default.scip"), scip_for(&[("src/lib.rs", LIB2)])).unwrap();
        let p = repo.path();
        git(p, &["init", "-q", "-b", "main"]);
        write(p, ".gitattributes", ATTRS);
        write(p, "Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n");
        write(p, "src/lib.rs", LIB1);
        write(p, "notes.txt", "one\n");
        write(p, "README.md", "readme\n");
        write(p, "notes.dat", "d1\n");
        commit(p, "c1");
        write(p, "src/lib.rs", LIB2);
        write(p, "notes.txt", "two\n");
        commit(p, "c2");
        let e = Env { repo, fx, cache: tempfile::tempdir().unwrap(), scratch: tempfile::tempdir().unwrap(), ra };
        let p = e.repo.path();
        // Warm the cache while the config is still benign: `--no-build` serves only cached indexes.
        assert_eq!(e.alsp(&["sym", "api"]).status.code(), Some(0));
        assert_eq!(e.alsp(&["pack", "--base", "HEAD~1"]).status.code(), Some(0));
        let marks = plant(p, e.scratch.path());
        write(p, "notes.txt", "three, edited after the commit\n");
        write(p, "README.md", "readme edited\n");
        write(p, "notes.dat", "d2\n");
        write(p, "new.txt", "untracked\n");
        write(p, "new.md", "untracked\n");
        (e, marks)
    }

    fn alsp(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.args(args).arg("--repo").arg(self.repo.path()).arg("--cache-dir").arg(self.cache.path());
        c.env("ALSP_RUST_ANALYZER", &self.ra).env("ALSP_TEST_FX", self.fx.path());
        hermetic(&mut c).output().unwrap()
    }
}

#[test]
fn test_DW_G_1_sym_refs_and_pack_run_none_of_the_repository_configs_programs() {
    let (e, marks) = Env::new();
    prove_triggers_fire(e.repo.path(), &marks, "HEAD~1", "HEAD");
    for args in [&["sym", "api", "--no-build"][..], &["refs", "helper", "--no-build"], &["pack", "--base", "HEAD~1", "--no-build"], &["pack", "--base", "HEAD~1"]] {
        let o = e.alsp(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", stderr(&o));
        assert_eq!(fired(&marks), BTreeSet::new(), "{args:?} ran a program named in .git/config");
    }
}

#[test]
fn test_DW_G_1_the_adhoc_review_snapshot_runs_none_of_the_repository_configs_programs() {
    // Only non-Rust files differ, so review starts no indexer, and --print starts no agent.
    let (repo, cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let p = repo.path();
    git(p, &["init", "-q", "-b", "main"]);
    write(p, ".gitattributes", ATTRS);
    write(p, "notes.txt", "one\n");
    write(p, "README.md", "readme\n");
    write(p, "notes.dat", "d1\n");
    commit(p, "c0");
    git(p, &["checkout", "-q", "-b", "feat"]);
    write(p, "notes.txt", "two\n");
    commit(p, "c1");
    let marks = plant(p, scratch.path());
    prove_triggers_fire(p, &marks, "HEAD~1", "HEAD");
    write(p, "notes.txt", "three\n");
    write(p, "README.md", "readme edited\n");
    write(p, "notes.dat", "d2\n");
    write(p, "new.txt", "untracked\n");
    write(p, "new.md", "untracked\n");
    git(p, &["add", "new.txt"]);
    let _ = fs::remove_file(&marks); // the staging above ran the planted clean filter on purpose
    for extra in [&[][..], &["--untracked"]] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.arg("review").arg("--repo").arg(p).arg("--print").args(extra).env("ALSP_CACHE_DIR", cache.path());
        let o = hermetic(&mut c).output().unwrap();
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        let prompt = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(prompt.contains("notes.txt") && prompt.contains("three"), "the snapshot must still carry the edit:\n{prompt}");
        assert_eq!(fired(&marks), BTreeSet::new(), "review {extra:?} ran a program named in .git/config");
    }
}

/// Adds a submodule `sub` whose own config (under `.git/modules/sub`) plants a clean filter and an fsmonitor,
/// both selected by the submodule's committed `.gitattributes`, and edits `sub/f.txt` without changing its
/// size so any status or diff of the superproject has to compare it. Returns the submodule's marker file.
fn plant_submodule(repo: &Path, scratch: &Path) -> PathBuf {
    let src = scratch.join("subsrc");
    fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    write(&src, ".gitattributes", "*.txt filter=subevil\n");
    write(&src, "f.txt", "one\n");
    commit(&src, "s1");
    git(repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &src.display().to_string(), "sub"]);
    let (marks, mark) = (scratch.join("submarks"), scratch.join("submark.sh"));
    script(&mark, &format!("echo \"$1\" >> {}\ncat", marks.display()));
    let m = mark.display();
    let gitdir = repo.join(".git/modules/sub");
    for (k, v) in [("core.fsmonitor", format!("{m} sub-fsmonitor")), ("filter.subevil.clean", format!("{m} sub-clean")), ("filter.subevil.smudge", format!("{m} sub-smudge"))] {
        git(&gitdir, &["config", "--local", k, &v]);
    }
    write(repo, "sub/f.txt", "two\n");
    marks
}

#[test]
fn test_DW_G2_2_a_submodules_own_config_programs_never_run_from_sym_refs_pack_or_review() {
    let (e, marks) = Env::new();
    let (p, sub_scratch) = (e.repo.path(), tempfile::tempdir().unwrap());
    let submarks = plant_submodule(p, sub_scratch.path());
    // Control: plain git status looks inside the submodule and runs its fsmonitor and clean filter.
    let _ = git_env(p, &["status", "--porcelain"], &[]);
    let live = fired(&submarks);
    assert!(live.contains("sub-fsmonitor") && live.contains("sub-clean"), "control did not fire: {live:?}");
    for args in [&["sym", "api", "--no-build"][..], &["refs", "helper", "--no-build"], &["pack", "--base", "HEAD~1", "--no-build"], &["pack", "--base", "HEAD~1"]] {
        let _ = fs::remove_file(&submarks);
        let o = e.alsp(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", stderr(&o));
        assert_eq!(fired(&submarks), BTreeSet::new(), "{args:?} ran a program named in a submodule's config");
    }
    // The ad-hoc review snapshot takes the same work tree. Only non-Rust files differ from HEAD besides
    // sub/, so no indexer starts; `--print` starts no agent.
    let _ = fs::remove_file(&submarks);
    let _ = fs::remove_file(&marks);
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("review").arg("--repo").arg(p).arg("--base").arg("HEAD~1").arg("--print").env("ALSP_CACHE_DIR", e.cache.path()).env("ALSP_RUST_ANALYZER", &e.ra).env("ALSP_TEST_FX", e.fx.path());
    let o = hermetic(&mut c).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(fired(&submarks), BTreeSet::new(), "review ran a program named in a submodule's config");
    assert_eq!(fired(&marks), BTreeSet::new(), "review ran a program named in .git/config");
}

/// A partial clone of a local origin: no blobs, no checkout. Its remote's `uploadpack` is a planted program
/// that records itself and then runs the real one, so any fetch of a missing blob through the repository's
/// remote config leaves a mark. Returns the marker file.
fn partial_clone(origin: &Path, dest: &Path, scratch: &Path) -> PathBuf {
    git(scratch, &["clone", "-q", "--no-checkout", "--filter=blob:none", &format!("file://{}", origin.display()), &dest.display().to_string()]);
    let (marks, prog) = (scratch.join(format!("{}-marks", dest.file_name().unwrap().to_string_lossy())), scratch.join(format!("{}-up.sh", dest.file_name().unwrap().to_string_lossy())));
    script(&prog, &format!("echo uploadpack >> {}\nexec git-upload-pack \"$@\"", marks.display()));
    git(dest, &["config", "--local", "remote.origin.uploadpack", &prog.display().to_string()]);
    git(dest, &["config", "--local", "remote.origin.url", &format!("file://{}", origin.display())]);
    marks
}

#[test]
fn test_DW_G2_1_a_missing_blob_in_a_partial_clone_is_never_fetched_through_the_repositorys_remote_config() {
    let (origin, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let o = origin.path();
    git(o, &["init", "-q", "-b", "main"]);
    git(o, &["config", "uploadpack.allowFilter", "true"]);
    git(o, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    write(o, "Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n");
    write(o, "src/lib.rs", LIB1);
    commit(o, "c1");
    write(o, "src/lib.rs", LIB2);
    commit(o, "c2");
    // Control: plain git fetches the missing blob, which runs the planted program.
    let (ctl, victim) = (scratch.path().join("ctl"), scratch.path().join("victim"));
    let ctl_marks = partial_clone(o, &ctl, scratch.path());
    assert_eq!(git(&ctl, &["config", "remote.origin.promisor"]), "true");
    let shown = git_env(&ctl, &["show", "HEAD~1:src/lib.rs"], &[]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    assert!(fired(&ctl_marks).contains("uploadpack"), "control did not fire: the probe is blind");
    // Hardened: the same missing blobs, read by alsp.
    let (marks, cache, fx) = (partial_clone(o, &victim, scratch.path()), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(fx.path().join("default.scip"), scip_for(&[("src/lib.rs", LIB2)])).unwrap();
    let ra = fx.path().join("ra.sh");
    script(&ra, "if [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\ncp \"$ALSP_TEST_FX/default.scip\" \"$4\"");
    let alsp = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.args(args).arg("--repo").arg(&victim).arg("--cache-dir").arg(cache.path()).env("ALSP_RUST_ANALYZER", &ra).env("ALSP_TEST_FX", fx.path());
        hermetic(&mut c).output().unwrap()
    };
    for args in [&["sym", "api", "--no-build"][..], &["refs", "helper", "--no-build"], &["pack", "--base", "HEAD~1", "--no-build"], &["pack", "--base", "HEAD~1"]] {
        let out = alsp(args);
        assert_eq!(fired(&marks), BTreeSet::new(), "{args:?} fetched through the repository's remote program: {}", stderr(&out));
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("review").arg("--repo").arg(&victim).arg("--base").arg("HEAD~1").arg("--print").env("ALSP_CACHE_DIR", cache.path());
    let out = hermetic(&mut c).output().unwrap();
    assert_eq!(fired(&marks), BTreeSet::new(), "review fetched through the repository's remote program: {}", stderr(&out));
    let blob = git_env(&victim, &["cat-file", "-e", "HEAD~1:src/lib.rs"], &[("GIT_NO_LAZY_FETCH", Path::new("1"))]);
    assert!(!blob.status.success(), "the missing blob was fetched anyway");
}
