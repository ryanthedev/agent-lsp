//! End to end: which rust-analyzer a command ends up with, and what it says when none runs. The
//! fakes are shell scripts on a private PATH: a rustup shim that fails under the repository's pin,
//! a `rustup` that knows one other toolchain, and an indexer that copies a prebuilt SCIP file.
#![allow(non_snake_case)]
use alsp_evidence::testkit::scip_for;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const PIN: &str = "nightly-2026-07-22";

struct Fx {
    repo: tempfile::TempDir,
    tools: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Fx {
    /// A repository pinned to `PIN`, two commits apart, with fakes on `tools`. `fallback` says
    /// whether the stable toolchain has a rust-analyzer.
    fn new(fallback: bool) -> Fx {
        let (repo, tools, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (r, t) = (repo.path(), tools.path());
        git(r, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join("Cargo.toml"), "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(r.join("rust-toolchain.toml"), format!("[toolchain]\nchannel = \"{PIN}\"\n")).unwrap();
        fs::write(r.join("src/lib.rs"), "pub fn api() {\n    1;\n}\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "-q", "-m", "base"]);
        let head = "pub fn api() {\n    2;\n}\n";
        fs::write(r.join("src/lib.rs"), head).unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "-q", "-m", "head"]);
        fs::write(t.join("index.scip"), scip_for(&[("src/lib.rs", head)])).unwrap();
        script(t, "rust-analyzer", &format!("if [ \"$RUSTUP_TOOLCHAIN\" = {PIN} ]; then echo \"error: Unknown binary 'rust-analyzer' in official toolchain '$RUSTUP_TOOLCHAIN'\" >&2; exit 1; fi; echo shim"));
        let ra = format!("if [ \"$1\" = --version ]; then echo 'rust-analyzer stable-fake'; exit 0; fi\ncp \"$ALSP_RVIEW_SCIP\" \"$4\"");
        if fallback {
            script(t, "ra-stable-x", &ra);
        }
        script(t, "ra-broken", "exit 1");
        script(t, "rustup", &format!("case \"$1\" in toolchain) printf '%s\\n' 'stable-x' '{PIN}-x (default)';; which) [ -x \"{d}/ra-$3\" ] && echo \"{d}/ra-$3\" || {{ echo \"error: unknown binary 'rust-analyzer' in toolchain '$3'\" >&2; exit 1; }};; esac", d = t.display()));
        script(t, "cargo", "cat \"$ALSP_REVIEW_META\"");
        let canon = fs::canonicalize(r).unwrap();
        fs::write(t.join("metadata.json"), format!(r#"{{"workspace_root":"{0}","packages":[{{"name":"fx","manifest_path":"{0}/Cargo.toml"}}]}}"#, canon.display())).unwrap();
        Fx { repo, tools, cache }
    }

    fn review(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.args(["review", "--quick", "--base", "HEAD~1", "--repo"]).arg(self.repo.path());
        c.env("PATH", format!("{}:/usr/bin:/bin", self.tools.path().display()))
            .env_remove("ALSP_RUST_ANALYZER")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env("ALSP_CACHE_DIR", self.cache.path())
            .env("ALSP_CARGO", self.tools.path().join("cargo"))
            .env("ALSP_REVIEW_META", self.tools.path().join("metadata.json"))
            .env("ALSP_RVIEW_SCIP", self.tools.path().join("index.scip"));
        c
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn test_DW_2_1_a_shim_that_fails_under_the_pin_falls_back_and_the_index_line_says_so() {
    let fx = Fx::new(true);
    let o = fx.review().output().unwrap();
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.contains("Index:") && t.contains("1/1 roots indexed") && t.contains("fallback") && t.contains("stable-x") && t.contains(PIN), "{t}");
    assert!(t.contains("api"), "the fallback indexer's facts are served: {t}");
}

#[test]
fn test_DW_2_2_no_working_candidate_exits_3_with_a_fix_naming_the_repository_toolchain() {
    let fx = Fx::new(false);
    let o = fx.review().output().unwrap();
    let t = text(&o);
    assert_eq!(o.status.code(), Some(3), "{t}");
    assert!(t.contains(&format!("rustup component add rust-analyzer --toolchain {PIN}")), "{t}");
    assert!(t.contains("Unknown binary"), "the proxy's own words are shown: {t}");
}

#[test]
fn test_DW_2_3_a_broken_alsp_rust_analyzer_is_reported_and_not_replaced_by_the_fallback() {
    let fx = Fx::new(true);
    let broken = fx.tools.path().join("ra-broken");
    let o = fx.review().env("ALSP_RUST_ANALYZER", &broken).output().unwrap();
    let t = text(&o);
    assert_eq!(o.status.code(), Some(3), "{t}");
    assert!(t.contains(&format!("ALSP_RUST_ANALYZER={}", broken.display())) && !t.contains("fallback: rust-analyzer of toolchain"), "{t}");
}

#[test]
fn test_DW_2_5_a_planted_rust_analyzer_and_rustup_in_the_repository_never_run() {
    let fx = Fx::new(false);
    let r = fx.repo.path();
    let marker = fx.tools.path().join("marker");
    fs::create_dir_all(r.join("bin")).unwrap();
    for name in ["rust-analyzer", "rustup", "cargo"] {
        script(&r.join("bin"), name, &format!("echo \"{name} $0 $PWD $* PATH=$PATH\" >> '{}'; exit 1", marker.display()));
    }
    fs::write(r.join("rust-toolchain.toml"), format!("[toolchain]\nchannel = \"{PIN}\"\npath = \"{}/bin\"\n", r.display())).unwrap();
    // Listed on PATH both absolutely and as a relative entry; the process also starts inside the repository.
    let path = format!("{}/bin:bin:.:{}:/usr/bin:/bin", r.display(), fx.tools.path().display());
    let o = fx.review().env("PATH", path).current_dir(r).output().unwrap();
    assert!(!marker.exists(), "a program from the repository ran: {}", fs::read_to_string(&marker).unwrap_or_default());
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));
}

#[test]
fn test_DW_2_2_a_query_with_no_working_candidate_exits_3_and_shows_the_fix() {
    let fx = Fx::new(false);
    for no_build in [false, true] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.args(["refs", "api", "--repo"]).arg(fx.repo.path()).env("PATH", format!("{}:/usr/bin:/bin", fx.tools.path().display()));
        c.env_remove("ALSP_RUST_ANALYZER").env_remove("RUSTUP_TOOLCHAIN").env("ALSP_CACHE_DIR", fx.cache.path());
        if no_build {
            c.arg("--no-build");
        }
        let o = c.output().unwrap();
        assert_eq!(o.status.code(), Some(3), "no_build={no_build}: {}", text(&o));
        // `--no-build` never reaches the fix: it says to rerun without it, which does.
        assert!(no_build || text(&o).contains(&format!("rustup component add rust-analyzer --toolchain {PIN}")), "{}", text(&o));
    }
}
