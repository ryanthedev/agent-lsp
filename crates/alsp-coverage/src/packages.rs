//! Which Cargo package owns a source file, and where its workspace is (asked of `cargo metadata`,
//! so workspace layouts, renamed packages and nested manifests are all the cargo's problem).

use crate::exec;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Package {
    pub name: String,
    /// Directory to run cargo in so that `-p name` resolves.
    pub workspace_root: PathBuf,
}

/// Resolves files to packages, asking cargo once per manifest directory.
pub struct Resolver<'a> {
    program: &'a Path,
    env: &'a [(String, String)],
    tree: &'a Path,
    memo: HashMap<PathBuf, Result<Option<Package>, String>>,
}

impl<'a> Resolver<'a> {
    pub fn new(program: &'a Path, env: &'a [(String, String)], tree: &'a Path) -> Self {
        Resolver { program, env, tree, memo: HashMap::new() }
    }

    /// The package whose manifest is the nearest `Cargo.toml` above `file` (relative to the tree).
    pub fn package_of(&mut self, file: &str) -> Result<Package, String> {
        let mut dir = self.tree.join(file);
        while dir.pop() {
            if !dir.starts_with(self.tree) {
                break;
            }
            if dir.join("Cargo.toml").is_file() {
                if let Some(p) = self.at(&dir)? {
                    return Ok(p);
                }
            }
            if dir == self.tree {
                break;
            }
        }
        Err(format!("no Cargo package owns {file}"))
    }

    /// The package declared by `dir/Cargo.toml` (`None` for a virtual workspace manifest).
    fn at(&mut self, dir: &Path) -> Result<Option<Package>, String> {
        if let Some(r) = self.memo.get(dir) {
            return r.clone();
        }
        let r = self.ask(dir);
        self.memo.insert(dir.to_path_buf(), r.clone());
        r
    }

    fn ask(&self, dir: &Path) -> Result<Option<Package>, String> {
        let mut cmd = Command::new(self.program);
        cmd.args(["metadata", "--no-deps", "--format-version", "1", "--manifest-path"]).arg(dir.join("Cargo.toml")).current_dir(dir).envs(self.env.iter().map(|(k, v)| (k, v)));
        let out = exec::run(&mut cmd, Duration::from_secs(120), 600)?;
        if !out.success() {
            return Err(format!("cargo metadata failed in {}: {}", dir.display(), out.stderr));
        }
        parse_metadata(&out.stdout, dir)
    }
}

/// `cargo metadata --no-deps` JSON -> the package whose manifest is `dir/Cargo.toml`.
pub fn parse_metadata(json: &str, dir: &Path) -> Result<Option<Package>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("cargo metadata output is not JSON: {e}"))?;
    let root = v.get("workspace_root").and_then(|w| w.as_str()).ok_or("cargo metadata has no workspace_root")?;
    let manifest = std::fs::canonicalize(dir.join("Cargo.toml")).unwrap_or_else(|_| dir.join("Cargo.toml"));
    let pkgs = v.get("packages").and_then(|p| p.as_array()).ok_or("cargo metadata has no packages")?;
    for p in pkgs {
        let (Some(name), Some(path)) = (p.get("name").and_then(|n| n.as_str()), p.get("manifest_path").and_then(|n| n.as_str())) else { continue };
        if std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)) == manifest {
            return Ok(Some(Package { name: name.to_string(), workspace_root: PathBuf::from(root) }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_picks_the_package_with_this_manifest() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("crates/a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("Cargo.toml"), "").unwrap();
        let json = format!(
            r#"{{"workspace_root":"{}","packages":[{{"name":"other","manifest_path":"/x/Cargo.toml"}},{{"name":"a","manifest_path":"{}"}}]}}"#,
            d.path().display(),
            a.join("Cargo.toml").display()
        );
        let p = parse_metadata(&json, &a).unwrap().unwrap();
        assert_eq!((p.name.as_str(), p.workspace_root.as_path()), ("a", d.path()));
        assert_eq!(parse_metadata(&json, d.path()).unwrap(), None, "a virtual workspace owns nothing");
        assert!(parse_metadata("nope", &a).unwrap_err().contains("not JSON"));
        assert!(parse_metadata("{}", &a).unwrap_err().contains("workspace_root"));
        assert!(parse_metadata(r#"{"workspace_root":"/w"}"#, &a).unwrap_err().contains("no packages"));
    }
}
