//! Finding the Cargo roots of a tree and running `rust-analyzer scip` on one of them.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How to run the indexer. The environment of the calling process is passed through untouched
/// (build scripts that read their own variables), plus `extra_env`.
#[derive(Clone, Debug)]
pub struct Tool {
    /// The rust-analyzer binary; looked up on `PATH` when not a path.
    pub rust_analyzer: PathBuf,
    pub timeout: Duration,
    pub extra_env: Vec<(String, String)>,
}

impl Default for Tool {
    fn default() -> Self {
        Tool {
            rust_analyzer: std::env::var_os("ALSP_RUST_ANALYZER").map(PathBuf::from).unwrap_or_else(|| "rust-analyzer".into()),
            timeout: Duration::from_secs(20 * 60),
            extra_env: Vec::new(),
        }
    }
}

impl Tool {
    /// `rust-analyzer --version`, or the reason it cannot run. Part of the cache key.
    /// Run from `cwd`, which must be outside the repository: the installed `rust-analyzer` is
    /// usually a rustup shim, and rustup picks the binary from the directory it is started in, so
    /// a repository's `rust-toolchain.toml` must not decide what runs here.
    pub fn version(&self, cwd: &Path) -> Result<String, String> {
        let out = Command::new(&self.rust_analyzer)
            .arg("--version")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.rust_analyzer.display()))?;
        if !out.status.success() {
            return Err(format!("{} --version failed: {}", self.rust_analyzer.display(), tail(&String::from_utf8_lossy(&out.stderr))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// A Cargo root indexed on its own: a directory with a `Cargo.toml` no other root contains.
/// The tree root when it has a manifest (its workspace members come along); otherwise the
/// top-most directories that have one, so independent crates in one repository fail separately.
pub fn find_units(tree: &Path) -> std::io::Result<Vec<PathBuf>> {
    if tree.join("Cargo.toml").is_file() {
        return Ok(vec![PathBuf::new()]);
    }
    let mut units = Vec::new();
    let mut stack = vec![(PathBuf::new(), 0usize)];
    while let Some((rel, depth)) = stack.pop() {
        for entry in std::fs::read_dir(tree.join(&rel))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_dir() || name.starts_with('.') || name == "target" || depth >= 6 {
                continue;
            }
            let child = rel.join(&name);
            if tree.join(&child).join("Cargo.toml").is_file() {
                units.push(child);
            } else {
                stack.push((child, depth + 1));
            }
        }
    }
    units.sort();
    Ok(units)
}

/// Keep the last 600 bytes of a message: indexers are chatty, the end says why they stopped.
pub fn tail(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 600 {
        return s.to_string();
    }
    let mut cut = s.len() - 600;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("...{}", &s[cut..])
}

/// Run `rust-analyzer scip <unit_dir> --output <out>`. Success needs exit 0 and a non-empty file.
pub fn run_scip(tool: &Tool, unit_dir: &Path, out: &Path) -> Result<(), String> {
    let mut cmd = Command::new(&tool.rust_analyzer);
    cmd.arg("scip").arg(unit_dir).arg("--output").arg(out);
    cmd.envs(tool.extra_env.iter().map(|(k, v)| (k, v)));
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", tool.rust_analyzer.display()))?;
    // Drain stderr on a thread so a full pipe never stalls the indexer; keep only the tail.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > tool.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", tool.timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("waiting for the indexer failed: {e}")),
        }
    };
    let log = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("exit {}: {}", status.code().map_or("signal".into(), |c| c.to_string()), tail(&log)));
    }
    match std::fs::metadata(out) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => Err(format!("the indexer wrote no output: {}", tail(&log))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fake_tool(dir: &Path, script_body: &str) -> Tool {
        let p = dir.join("fake-ra");
        fs::write(&p, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        Tool { rust_analyzer: p, timeout: Duration::from_secs(10), extra_env: vec![] }
    }

    #[test]
    fn units_are_the_root_when_it_has_a_manifest() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Cargo.toml"), "").unwrap();
        fs::create_dir_all(d.path().join("crates/a")).unwrap();
        fs::write(d.path().join("crates/a/Cargo.toml"), "").unwrap();
        assert_eq!(find_units(d.path()).unwrap(), vec![PathBuf::new()]);
    }

    #[test]
    fn units_are_the_topmost_manifests_without_a_root_manifest() {
        let d = tempfile::tempdir().unwrap();
        for p in ["good", "bad", "group/deep", ".hidden/x", "target/t"] {
            fs::create_dir_all(d.path().join(p)).unwrap();
            fs::write(d.path().join(p).join("Cargo.toml"), "").unwrap();
        }
        fs::create_dir_all(d.path().join("good/nested")).unwrap();
        fs::write(d.path().join("good/nested/Cargo.toml"), "").unwrap();
        assert_eq!(find_units(d.path()).unwrap(), vec![PathBuf::from("bad"), PathBuf::from("good"), PathBuf::from("group/deep")]);
    }

    #[test]
    fn no_manifest_means_no_units() {
        let d = tempfile::tempdir().unwrap();
        assert!(find_units(d.path()).unwrap().is_empty());
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("  short  "), "short");
        let long = format!("{}é{}", "a".repeat(700), "z".repeat(599));
        let t = tail(&long);
        assert!(t.starts_with("...") && t.ends_with('z') && t.len() < 700);
    }

    #[test]
    fn run_scip_success_failure_empty_output_and_timeout() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("o.scip");
        let ok = fake_tool(d.path(), "while [ $# -gt 0 ]; do [ \"$1\" = --output ] && echo data > \"$2\"; shift; done");
        run_scip(&ok, d.path(), &out).unwrap();
        assert!(out.exists());
        fs::remove_file(&out).unwrap();

        let fail = fake_tool(d.path(), "echo 'manifest is broken' >&2; exit 3");
        let e = run_scip(&fail, d.path(), &out).unwrap_err();
        assert!(e.contains("exit 3") && e.contains("manifest is broken"), "{e}");

        let empty = fake_tool(d.path(), "exit 0");
        assert!(run_scip(&empty, d.path(), &out).unwrap_err().contains("no output"));

        let mut slow = fake_tool(d.path(), "sleep 5");
        slow.timeout = Duration::from_millis(100);
        assert!(run_scip(&slow, d.path(), &out).unwrap_err().contains("timed out"));

        let missing = Tool { rust_analyzer: "/no/such/ra".into(), ..Tool::default() };
        assert!(run_scip(&missing, d.path(), &out).unwrap_err().contains("cannot run"));
        assert!(missing.version(d.path()).unwrap_err().contains("cannot run"));
    }

    #[test]
    fn environment_is_passed_through_and_extra_env_added() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("o.scip");
        let mut t = fake_tool(d.path(), "while [ $# -gt 0 ]; do [ \"$1\" = --output ] && echo \"$ALSP_TEST_EXTRA\" > \"$2\"; shift; done");
        t.extra_env.push(("ALSP_TEST_EXTRA".into(), "hello".into()));
        run_scip(&t, d.path(), &out).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "hello");
    }

    #[test]
    fn version_reports_the_tool_output() {
        let d = tempfile::tempdir().unwrap();
        let t = fake_tool(d.path(), "echo 'rust-analyzer 9.9'");
        assert_eq!(t.version(d.path()).unwrap(), "rust-analyzer 9.9");
        let bad = fake_tool(d.path(), "exit 1");
        assert!(bad.version(d.path()).is_err());
    }
}

#[cfg(test)]
pub(crate) use tests::fake_tool;
