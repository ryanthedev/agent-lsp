//! Content hash of a tree: Rust sources, `Cargo.toml` files and `Cargo.lock`, nothing else.

use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

fn counts(name: &str) -> bool {
    name.ends_with(".rs") || name == "Cargo.toml" || name == "Cargo.lock"
}

/// A directory the walk must not enter: hidden directories (`.git`, `.alsp`, ...) and cargo build
/// output (a `target` directory carrying cargo's `CACHEDIR.TAG`).
fn skipped(dir: &Path, name: &str) -> bool {
    name.starts_with('.') || (name == "target" && dir.join("CACHEDIR.TAG").exists())
}

/// Files that count, as paths relative to `root`, sorted. Symlinks are not followed.
pub fn tracked_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                if !skipped(&entry.path(), &name) {
                    stack.push(entry.path());
                }
            } else if ty.is_file() && counts(&name) {
                out.push(entry.path().strip_prefix(root).expect("entry is under root").to_path_buf());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 16 hex characters of SHA-256 over `salt` and every counted file's relative path and bytes.
/// `salt` carries what else changes the index (tool version, schema version).
pub fn tree_hash(root: &Path, salt: &str) -> io::Result<String> {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    for rel in tracked_files(root)? {
        let bytes = std::fs::read(root.join(&rel))?;
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(format!("{:x}", h.finalize())[..16].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn only_sources_manifests_and_lockfile_count() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "a");
        write(d.path(), "Cargo.lock", "b");
        write(d.path(), "src/lib.rs", "c");
        write(d.path(), "README.md", "ignored");
        write(d.path(), ".git/x.rs", "ignored");
        write(d.path(), "target/CACHEDIR.TAG", "");
        write(d.path(), "target/debug/build.rs", "ignored");
        write(d.path(), "crates/x/target/y.rs", "counts: no CACHEDIR.TAG here");
        let files: Vec<String> = tracked_files(d.path()).unwrap().iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(files, vec!["Cargo.lock", "Cargo.toml", "crates/x/target/y.rs", "src/lib.rs"]);
    }

    #[test]
    fn hash_changes_with_content_path_and_salt_but_not_with_ignored_files() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "src/lib.rs", "fn a(){}");
        let h1 = tree_hash(d.path(), "s").unwrap();
        assert_eq!(h1.len(), 16);
        assert_eq!(h1, tree_hash(d.path(), "s").unwrap());
        write(d.path(), "notes.txt", "irrelevant");
        assert_eq!(h1, tree_hash(d.path(), "s").unwrap());
        assert_ne!(h1, tree_hash(d.path(), "other").unwrap());
        write(d.path(), "src/lib.rs", "fn b(){}");
        let h2 = tree_hash(d.path(), "s").unwrap();
        assert_ne!(h1, h2);
        fs::rename(d.path().join("src/lib.rs"), d.path().join("src/main.rs")).unwrap();
        assert_ne!(h2, tree_hash(d.path(), "s").unwrap());
    }

    #[test]
    fn unreadable_root_is_an_error() {
        assert!(tree_hash(Path::new("/definitely/not/here"), "").is_err());
    }
}
