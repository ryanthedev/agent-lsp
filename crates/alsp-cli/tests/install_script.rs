//! `scripts/install.sh` and the install docs. Every tool is a shell script on a private PATH, so
//! nothing real is installed; the script itself runs under /bin/sh with that PATH alone.
#![allow(non_snake_case)]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn script_path() -> PathBuf {
    root().join("scripts/install.sh")
}

fn fake(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A private PATH holding the named fakes (each exits 0), and an empty HOME.
struct Fx {
    tools: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fx {
    fn with(names: &[&str]) -> Fx {
        let fx = Fx { tools: tempfile::tempdir().unwrap(), home: tempfile::tempdir().unwrap() };
        for n in names {
            fake(fx.tools.path(), n, "exit 0");
        }
        fx
    }
    fn all() -> Fx {
        Fx::with(&["cargo", "rustup", "claude", "codex", "agy"])
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("/bin/sh")
            .arg(script_path())
            .args(args)
            .env_clear()
            .env("PATH", self.tools.path())
            .env("HOME", self.home.path())
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

#[test]
fn test_DW_5_1_dry_run_prints_every_command_for_every_agent() {
    let fx = Fx::all();
    let o = fx.run(&["--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let s = out(&o);
    let clone = fx.home.path().join(".local/share/agent-lsp-plugin");
    let clone = clone.display();
    for line in [
        "+ cargo install --git https://github.com/ryanthedev/agent-lsp alsp-cli --locked --force",
        "+ rustup component add rust-analyzer",
        "+ claude plugin marketplace add ryanthedev/agent-lsp",
        "+ claude plugin marketplace update agent-lsp",
        "+ claude plugin install alsp@agent-lsp",
        "+ claude plugin update alsp@agent-lsp",
        "+ codex plugin marketplace add ryanthedev/agent-lsp",
        "+ codex plugin marketplace upgrade agent-lsp",
        "+ codex plugin add alsp@agent-lsp",
        &format!("clone --depth 1 https://github.com/ryanthedev/agent-lsp {clone}"),
        &format!("+ agy plugin import {clone}"),
        "+ alsp doctor",
    ] {
        assert!(s.contains(line), "{line:?} missing from:\n{s}");
    }
    // the clone is restricted to https
    assert!(s.contains("-c protocol.allow=never -c protocol.https.allow=always clone"), "{s}");
}

#[test]
fn test_DW_5_1_dry_run_runs_nothing() {
    // Fakes that would leave a marker if executed.
    let fx = Fx::with(&[]);
    let marker = fx.home.path().join("ran");
    for n in ["cargo", "rustup", "claude", "codex", "agy", "alsp"] {
        fake(fx.tools.path(), n, &format!("echo {n} >> {}; exit 0", marker.display()));
    }
    let o = fx.run(&["--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!marker.exists(), "dry run executed: {}", fs::read_to_string(&marker).unwrap_or_default());
}

#[test]
fn test_DW_5_1_codex_absent_is_skipped_and_the_rest_still_run() {
    let fx = Fx::with(&["cargo", "rustup", "claude", "agy"]);
    let o = fx.run(&["--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let s = out(&o);
    assert!(s.contains("codex: not found, skipped"), "{s}");
    assert!(!s.contains("+ codex "), "{s}");
    assert!(s.contains("+ claude plugin install alsp@agent-lsp") && s.contains("+ agy plugin import"), "{s}");
}

#[test]
fn test_DW_5_1_no_agent_is_required() {
    let fx = Fx::with(&["cargo", "rustup"]);
    let o = fx.run(&["--dry-run"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let s = out(&o);
    for a in ["claude", "codex", "agy"] {
        assert!(s.contains(&format!("{a}: not found, skipped")), "{s}");
    }
    assert!(s.contains("no agent CLI found") && s.contains("+ alsp doctor"), "{s}");
}

#[test]
fn test_DW_5_1_agy_pulls_an_existing_managed_copy_and_repairs_a_stale_origin() {
    let fx = Fx::with(&["cargo", "rustup", "agy"]);
    let copy = fx.home.path().join(".local/share/agent-lsp-plugin");
    fs::create_dir_all(copy.join(".git")).unwrap();
    // A copy cloned from a local path: pulling it would never reach the public repo.
    fs::write(copy.join(".git/config"), "[remote \"origin\"]\n\turl = /Users/someone/repos/agent-lsp\n").unwrap();
    let o = fx.run(&["--dry-run"]);
    let s = out(&o);
    assert_eq!(o.status.code(), Some(0), "{s}{}", err(&o));
    let c = copy.display();
    assert!(s.contains(&format!("+ git -C {c} remote set-url origin https://github.com/ryanthedev/agent-lsp")), "{s}");
    assert!(s.contains("pull --ff-only") && !s.contains("clone"), "{s}");
    assert!(s.contains(&format!("+ agy plugin import {c}")), "{s}");

    // With the right origin there is no set-url.
    fs::write(copy.join(".git/config"), "[remote \"origin\"]\n\turl = https://github.com/ryanthedev/agent-lsp\n").unwrap();
    let s = out(&fx.run(&["--dry-run"]));
    assert!(s.contains("pull --ff-only") && !s.contains("set-url"), "{s}");
}

#[test]
fn test_DW_5_1_rust_analyzer_present_is_not_reinstalled() {
    let fx = Fx::with(&["cargo", "rustup"]);
    fake(fx.tools.path(), "rust-analyzer", "echo 'rust-analyzer 1.0'");
    let s = out(&fx.run(&["--dry-run"]));
    assert!(!s.contains("rustup component add"), "{s}");
}

#[test]
fn test_DW_5_1_a_rustup_shim_without_the_component_still_gets_it_installed() {
    let fx = Fx::with(&["cargo", "rustup"]);
    fake(fx.tools.path(), "rust-analyzer", "echo 'error: component not installed' >&2; exit 1");
    let s = out(&fx.run(&["--dry-run"]));
    assert!(s.contains("+ rustup component add rust-analyzer"), "{s}");
}

#[test]
fn test_rev_pins_the_cargo_install_and_rejects_option_like_values() {
    let fx = Fx::with(&["cargo", "rustup"]);
    let s = out(&fx.run(&["--dry-run", "--rev", "v0.2.0"]));
    assert!(s.contains("--git https://github.com/ryanthedev/agent-lsp --rev v0.2.0 alsp-cli --locked --force"), "{s}");
    for bad in ["--config=evil", "a b", "x;y", "$(id)", ""] {
        let o = fx.run(&["--dry-run", "--rev", bad]);
        assert_eq!(o.status.code(), Some(2), "{bad:?} accepted: {}", out(&o));
        assert!(!out(&o).contains("cargo install"), "{bad:?}");
    }
    assert_eq!(fx.run(&["--rev"]).status.code(), Some(2));
    assert_eq!(fx.run(&["--bogus"]).status.code(), Some(2));
}

#[test]
fn test_DW_5_2_missing_cargo_exits_nonzero_with_a_rust_hint() {
    let fx = Fx::with(&["claude", "codex", "agy"]);
    for args in [&["--dry-run"][..], &[][..]] {
        let o = fx.run(args);
        assert_eq!(o.status.code(), Some(1), "{}", out(&o));
        let e = err(&o);
        assert!(e.contains("cargo not found") && e.contains("https://rustup.rs"), "{e}");
        assert!(!out(&o).contains("plugin"), "must stop before touching any plugin: {}", out(&o));
    }
}

#[test]
fn test_a_failing_plugin_step_is_reported_and_the_other_agents_still_run() {
    // A real (non-dry) run against fakes: claude's marketplace update fails, codex and agy go on.
    let fx = Fx::all();
    let log = fx.home.path().join("log");
    for n in ["cargo", "rustup", "codex", "agy", "alsp", "git"] {
        fake(fx.tools.path(), n, &format!("echo \"{n} $*\" >> {}; exit 0", log.display()));
    }
    fake(fx.tools.path(), "claude", &format!("echo \"claude $*\" >> {}; case \"$*\" in *'marketplace update'*) exit 1;; esac", log.display()));
    let o = fx.run(&[]);
    assert_eq!(o.status.code(), Some(1), "{}{}", out(&o), err(&o));
    assert!(err(&o).contains("failed: claude plugin marketplace update agent-lsp"), "{}", err(&o));
    let log = fs::read_to_string(&log).unwrap();
    assert!(!log.contains("claude plugin install"), "claude should stop after its failed update:\n{log}");
    for expect in ["codex plugin add alsp@agent-lsp", "agy plugin import", "alsp doctor"] {
        assert!(log.contains(expect), "{expect:?} missing:\n{log}");
    }
}

#[test]
fn test_an_already_added_marketplace_and_installed_plugin_do_not_fail_the_run() {
    let fx = Fx::with(&["cargo", "rustup", "alsp"]);
    fake(fx.tools.path(), "claude", "case \"$*\" in *'marketplace add'*|*'plugin install'*) echo already >&2; exit 1;; esac");
    let o = fx.run(&[]);
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    assert!(err(&o).contains("ignored (probably already present): claude plugin install"), "{}", err(&o));
}

#[test]
fn test_a_failed_cargo_install_stops_the_script() {
    let fx = Fx::with(&["claude"]);
    fake(fx.tools.path(), "cargo", "exit 101");
    let o = fx.run(&[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(!out(&o).contains("claude"), "{}", out(&o));
}

#[test]
fn test_DW_5_3_shellcheck_passes() {
    if Command::new("shellcheck").arg("--version").output().is_err() {
        eprintln!("skipped: shellcheck is not installed");
        return;
    }
    let o = Command::new("shellcheck").arg(script_path()).output().unwrap();
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
}

#[test]
fn test_install_script_is_posix_sh_and_executable() {
    let p = script_path();
    assert!(fs::read_to_string(&p).unwrap().starts_with("#!/bin/sh\n"));
    assert!(fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0, "not executable");
}

fn readme_install_section() -> String {
    let readme = fs::read_to_string(root().join("README.md")).unwrap();
    let start = readme.find("\n## Install").expect("README has an Install section");
    let rest = &readme[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |i| i + 3);
    rest[..end].to_string()
}

#[test]
fn test_DW_5_5_readme_install_section_is_the_one_liner_plus_doctor() {
    let s = readme_install_section();
    assert!(s.contains("scripts/install.sh") && s.contains("alsp doctor"), "{s}");
    assert!(s.contains("curl -fsSL https://raw.githubusercontent.com/ryanthedev/agent-lsp/main/scripts/install.sh | sh"), "{s}");
    assert!(s.lines().count() < 25, "the install section should stay short:\n{s}");
}

#[test]
fn test_DW_5_5_plugin_doc_describes_update_as_rerunning_the_script() {
    let d = fs::read_to_string(root().join("docs/plugin.md")).unwrap();
    assert!(d.contains("scripts/install.sh") && d.to_lowercase().contains("re-run"), "{d}");
    assert!(!d.contains("It does not install the `alsp` binary, which must be on `PATH` first: `cargo install"), "stale install text");
    let i = fs::read_to_string(root().join("docs/install.md")).unwrap();
    for needle in ["--dry-run", "--rev", "claude plugin uninstall", "codex plugin remove", "agy plugin uninstall", "alsp doctor"] {
        assert!(i.contains(needle), "{needle:?} missing from docs/install.md");
    }
}

#[test]
fn test_DW_5_4_skill_names_test_impact_questions_and_the_new_commands() {
    let skill = fs::read_to_string(root().join("skills/alsp/SKILL.md")).unwrap();
    let (front, body) = skill.strip_prefix("---\n").unwrap().split_once("\n---\n").unwrap();
    let desc = front.lines().find_map(|l| l.strip_prefix("description: ")).unwrap();
    assert!(desc.len() <= 1024, "description is {} chars", desc.len());
    for needle in ["which tests", "what breaks"] {
        assert!(desc.to_lowercase().contains(needle), "{needle:?} missing from the description: {desc}");
    }
    // the command table routes the question to tests and refs
    let row = body.lines().find(|l| l.starts_with('|') && l.to_lowercase().contains("what breaks")).expect("a table row for what-breaks questions");
    assert!(row.contains("alsp tests") && row.contains("alsp refs"), "{row}");
    for needle in ["alsp doctor", "exit 3", "heuristic", "via"] {
        assert!(body.contains(needle), "{needle:?} missing from the skill body");
    }
}

#[test]
fn test_the_cheat_sheet_mentions_doctor() {
    let o = Command::new(env!("CARGO_BIN_EXE_alsp")).output().unwrap();
    let s = out(&o);
    assert!(s.contains("alsp doctor") && s.lines().count() < 40, "{s}");
}
