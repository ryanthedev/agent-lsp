//! `alsp outline`: instant `syn`-powered structural skeletons of local Rust files/directories
//! or dependency crates (`--dep <crate>`).

use alsp_query::outline::{outline_paths, resolve_dep_root};
use clap::Args as ClapArgs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(ClapArgs)]
pub struct Args {
    /// Files or directories to outline (default: `src/` if present, else `.`).
    pub paths: Vec<PathBuf>,
    /// Show only public items, public fields, trait impls, and public impl methods.
    #[arg(long = "pub")]
    pub pub_only: bool,
    /// Outline a dependency crate's source (located via `cargo metadata`).
    #[arg(long)]
    pub dep: Option<String>,
    /// Repository root (default: current directory).
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Print one-line JSON instead of compact text.
    #[arg(long)]
    pub json: bool,
}

fn cargo_bin() -> PathBuf {
    cargo_bin_from(|k| std::env::var_os(k))
}

fn cargo_bin_from(get: impl FnOnce(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    get("ALSP_CARGO")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn default_paths(root: &Path) -> Vec<PathBuf> {
    if root.join("src").symlink_metadata().is_ok_and(|m| m.is_dir()) {
        vec![PathBuf::from("src")]
    } else {
        vec![PathBuf::from(".")]
    }
}

pub fn run(args: Args) -> ExitCode {
    let cargo = cargo_bin();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with(args, &cargo, &mut stdout, &mut stderr)
}

fn run_with(args: Args, cargo: &Path, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let (root, paths) = match &args.dep {
        Some(dep_name) => match resolve_dep_root(&args.repo, dep_name, cargo) {
            Ok(dep_root) => {
                let paths = if args.paths.is_empty() {
                    default_paths(&dep_root)
                } else {
                    args.paths.clone()
                };
                (dep_root, paths)
            }
            Err(msg) => {
                let _ = writeln!(err, "alsp: {msg}");
                return ExitCode::from(2);
            }
        },
        None => {
            let paths = if args.paths.is_empty() {
                default_paths(&args.repo)
            } else {
                args.paths.clone()
            };
            (args.repo.clone(), paths)
        }
    };

    match outline_paths(&root, &paths, args.pub_only) {
        Ok(ans) => {
            let rendered = if args.json { ans.to_json() } else { ans.to_text() };
            let _ = out.write_all(rendered.as_bytes());
            ExitCode::SUCCESS
        }
        Err(msg) => {
            let _ = writeln!(err, "alsp: {msg}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: Args,
    }

    fn code(c: ExitCode) -> String {
        format!("{c:?}")
    }

    #[test]
    fn args_parse_flags_and_defaults() {
        let a = TestCli::try_parse_from(["alsp", "src/lib.rs", "src/main.rs", "--pub", "--json"]).unwrap().args;
        assert_eq!(a.paths, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]);
        assert!(a.pub_only && a.json && a.dep.is_none());
        assert_eq!(a.repo, PathBuf::from("."));

        let b = TestCli::try_parse_from(["alsp", "--dep", "serde", "--repo", "/tmp/x"]).unwrap().args;
        assert!(b.paths.is_empty());
        assert_eq!(b.dep.as_deref(), Some("serde"));
        assert_eq!(b.repo, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn test_DW_2_1_local_outline_needs_no_cargo_or_rust_analyzer() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(
            repo.path().join("src/lib.rs"),
            "pub struct Alpha { pub x: u32 }\nfn priv_fn() {}\npub fn pub_fn() -> u32 { 1 }\n",
        )
        .unwrap();

        let args = Args {
            paths: vec![],
            pub_only: false,
            dep: None,
            repo: repo.path().to_path_buf(),
            json: false,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        // Passing nonexistent cargo verifies local outline never spawns cargo (or rust-analyzer).
        let status = run_with(args, Path::new("/nonexistent/cargo"), &mut out, &mut err);
        assert_eq!(code(status), code(ExitCode::SUCCESS), "stderr: {}", String::from_utf8_lossy(&err));
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("outline src/lib.rs [resolved]\n"), "{text}");
        assert!(text.contains("pub struct Alpha L1"), "{text}");
        assert!(text.contains("fn priv_fn() L2"), "{text}");
        assert!(text.contains("pub fn pub_fn() -> u32 L3"), "{text}");
    }

    #[test]
    fn test_DW_2_3_dep_outline_uses_cargo_metadata_and_unknown_dep_exits_2() {
        let repo = tempfile::tempdir().unwrap();
        let dep = tempfile::tempdir().unwrap();
        fs::create_dir_all(dep.path().join("src")).unwrap();
        fs::write(
            dep.path().join("src/lib.rs"),
            "pub trait Store {\n    fn get(&self, k: &str) -> Option<u32>;\n}\nfn hidden() {}\n",
        )
        .unwrap();
        let dep_canon = fs::canonicalize(dep.path()).unwrap();

        let tools = tempfile::tempdir().unwrap();
        let meta_path = tools.path().join("meta.json");
        fs::write(
            &meta_path,
            serde_json::json!({
                "packages": [
                    { "name": "fake-dep", "manifest_path": format!("{}/Cargo.toml", dep_canon.display()) }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let stub_cargo = tools.path().join("cargo");
        fs::write(
            &stub_cargo,
            format!("#!/bin/sh\ncat \"{}\"\n", meta_path.display()),
        )
        .unwrap();
        fs::set_permissions(&stub_cargo, fs::Permissions::from_mode(0o755)).unwrap();

        // Known dep with --pub
        let args = Args {
            paths: vec![],
            pub_only: true,
            dep: Some("fake-dep".into()),
            repo: repo.path().to_path_buf(),
            json: false,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = run_with(args, &stub_cargo, &mut out, &mut err);
        assert_eq!(code(status), code(ExitCode::SUCCESS), "stderr: {}", String::from_utf8_lossy(&err));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("pub trait Store L1-3"), "{text}");
        assert!(text.contains("fn get(&self, k: &str) -> Option<u32> L2"), "{text}");
        assert!(!text.contains("hidden"), "--pub should hide private fn: {text}");

        // Unknown dep exits 2 with helpful message
        let bad = Args {
            paths: vec![],
            pub_only: false,
            dep: Some("no-such-crate".into()),
            repo: repo.path().to_path_buf(),
            json: false,
        };
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let status2 = run_with(bad, &stub_cargo, &mut out2, &mut err2);
        assert_eq!(code(status2), code(ExitCode::from(2)));
        let err_text = String::from_utf8(err2).unwrap();
        assert!(err_text.contains("not found") && err_text.contains("fake-dep"), "{err_text}");

        // --dep with explicit subpath + --json
        fs::write(dep.path().join("src/sub.rs"), "pub fn sub_only() {}\n").unwrap();
        let sub_args = Args {
            paths: vec![PathBuf::from("src/sub.rs")],
            pub_only: false,
            dep: Some("fake-dep".into()),
            repo: repo.path().to_path_buf(),
            json: true,
        };
        let mut out3 = Vec::new();
        let mut err3 = Vec::new();
        let status3 = run_with(sub_args, &stub_cargo, &mut out3, &mut err3);
        assert_eq!(code(status3), code(ExitCode::SUCCESS), "stderr: {}", String::from_utf8_lossy(&err3));
        let json_str = String::from_utf8(out3).unwrap();
        assert_eq!(json_str.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 1);
        assert_eq!(v["files"][0]["file"], "src/sub.rs");
        assert_eq!(v["files"][0]["items"][0]["name"], "sub_only");
    }

    #[test]
    fn test_DW_2_1_and_2_3_cargo_bin_reads_alsp_cargo_and_ignores_rust_analyzer() {
        let env_nonexistent = |k: &str| match k {
            "ALSP_CARGO" => Some(std::ffi::OsString::from("/nonexistent/cargo")),
            "ALSP_RUST_ANALYZER" => Some(std::ffi::OsString::from("/nonexistent/rust-analyzer")),
            _ => None,
        };
        assert_eq!(cargo_bin_from(env_nonexistent), PathBuf::from("/nonexistent/cargo"));
        assert_eq!(cargo_bin_from(|_| Some(std::ffi::OsString::new())), PathBuf::from("cargo"));
        assert_eq!(cargo_bin_from(|_| None), PathBuf::from("cargo"));

        // Local outline succeeds even when ALSP_CARGO=/nonexistent and ALSP_RUST_ANALYZER=/nonexistent.
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/lib.rs"), "pub fn ping() {}\n").unwrap();
        let cargo = cargo_bin_from(env_nonexistent);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = run_with(
            Args { paths: vec![], pub_only: false, dep: None, repo: repo.path().to_path_buf(), json: false },
            &cargo,
            &mut out,
            &mut err,
        );
        assert_eq!(code(status), code(ExitCode::SUCCESS));
        assert!(String::from_utf8(out).unwrap().contains("pub fn ping() L1"));

        // --dep with ALSP_CARGO=/nonexistent fails cleanly with exit 2 (proving ALSP_CARGO is used).
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        let status2 = run_with(
            Args { paths: vec![], pub_only: false, dep: Some("serde".into()), repo: repo.path().to_path_buf(), json: false },
            &cargo,
            &mut out2,
            &mut err2,
        );
        assert_eq!(code(status2), code(ExitCode::from(2)));
        assert!(String::from_utf8(err2).unwrap().contains("/nonexistent/cargo"));
    }
}
