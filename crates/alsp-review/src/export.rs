//! Read-only export of a commit tree via `git archive | tar -x` without a shell.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Fills a fresh private directory with the head commit's files; returns notes about the export.
pub(crate) type Exporter<'a> = &'a (dyn Fn(&Path) -> Result<Vec<String>, String> + Sync);

/// Exports `head` with `git archive | tar -x` (no shell) into `dest`, so agents never touch the reviewed
/// checkout. Returns notes about what the export cannot carry (submodules, `export-ignore`).
pub(crate) fn export_head(
    repo: &Path,
    head: &str,
    dest: &Path,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    if head.is_empty() || head.starts_with('-') {
        return Err(format!("refusing head {head:?}"));
    }
    let mut git = alsp_evidence::git_command(repo)?;
    git.args(["archive", "--format=tar", head]);
    let mut tar = Command::new("tar");
    tar.args(["-x", "-f", "-", "-C"]).arg(dest);
    export_pipeline(git, tar, timeout)?;
    let mut notes = vec![];
    if dest.join(".gitmodules").exists() {
        notes.push("submodules are not in the export".to_string());
    }
    if std::fs::read_to_string(dest.join(".gitattributes"))
        .is_ok_and(|a| a.contains("export-ignore"))
    {
        notes.push(
            "export-ignore rules in .gitattributes may have dropped files from the export"
                .to_string(),
        );
    }
    Ok(notes)
}

/// Runs `producer | consumer` without a shell under one deadline. Both stderrs go to temp files, so a chatty
/// command can never block on a full pipe. On timeout or failure both processes are killed.
fn export_pipeline(
    mut producer: Command,
    mut consumer: Command,
    timeout: Duration,
) -> Result<(), String> {
    fn name(c: &Command) -> String {
        c.get_program().to_string_lossy().into_owned()
    }
    fn start_err(c: &Command, e: std::io::Error) -> String {
        let n = name(c);
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{n} is not installed")
        } else {
            format!("cannot start {n}: {e}")
        }
    }
    fn log() -> Result<std::fs::File, String> {
        tempfile::tempfile().map_err(|e| format!("cannot create a log file: {e}"))
    }
    fn read_log(mut f: std::fs::File) -> String {
        use std::io::Seek;
        let mut s = String::new();
        let _ = f.rewind();
        let _ = f.take(8192).read_to_string(&mut s);
        s.trim().to_string()
    }
    let (plog, clog) = (log()?, log()?);
    let mut p = producer
        .stdout(Stdio::piped())
        .stderr(plog.try_clone().map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| start_err(&producer, e))?;
    let out = p.stdout.take().ok_or("producer gave no stdout")?;
    let mut c = match consumer
        .stdin(Stdio::from(out))
        .stdout(Stdio::null())
        .stderr(clog.try_clone().map_err(|e| e.to_string())?)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = p.kill();
            let _ = p.wait();
            return Err(start_err(&consumer, e));
        }
    };
    let deadline = Instant::now() + timeout;
    let wait = |child: &mut std::process::Child| loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => return Err(format!("timed out after {}s", timeout.as_secs())),
            Err(e) => return Err(e.to_string()),
        }
    };
    let results = (wait(&mut c), wait(&mut p));
    let kill_both = |c: &mut std::process::Child, p: &mut std::process::Child| {
        let _ = c.kill();
        let _ = p.kill();
        let _ = c.wait();
        let _ = p.wait();
    };
    match results {
        (Ok(cs), Ok(ps)) if cs.success() && ps.success() => Ok(()),
        (Ok(cs), Ok(ps)) => {
            let failed: Vec<String> = [
                (!ps.success()).then(|| (name(&producer), plog)),
                (!cs.success()).then(|| (name(&consumer), clog)),
            ]
            .into_iter()
            .flatten()
            .map(|(n, f)| format!("{n} failed: {}", read_log(f)))
            .collect();
            Err(failed.join("; "))
        }
        (Err(e), _) | (_, Err(e)) => {
            kill_both(&mut c, &mut p);
            Err(e)
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(30);

    fn git_in(dir: &Path, args: &[&str]) {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    fn git_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git_in(d.path(), &["init", "-q"]);
        for (f, body) in files {
            std::fs::write(d.path().join(f), body).unwrap();
        }
        git_in(d.path(), &["add", "-A"]);
        git_in(d.path(), &["commit", "-q", "-m", "c"]);
        d
    }

    #[test]
    fn export_head_copies_the_committed_tree_without_dot_git_or_uncommitted_files() {
        let repo = git_repo(&[("a.txt", "hello\n")]);
        let head = {
            let o = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        std::fs::write(repo.path().join("a.txt"), "dirty\n").unwrap();
        std::fs::write(repo.path().join("untracked"), "u").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let notes = export_head(repo.path(), &head, dest.path(), T).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(std::fs::read_to_string(dest.path().join("a.txt")).unwrap(), "hello\n");
        assert!(!dest.path().join(".git").exists() && !dest.path().join("untracked").exists());
    }

    #[test]
    fn export_head_runs_no_program_named_in_the_repository_config() {
        use std::os::unix::fs::PermissionsExt;
        let repo = git_repo(&[(".gitattributes", "*.txt filter=evil\n"), ("a.txt", "hello\n")]);
        let (marks, prog) = (repo.path().join(".git/marks"), repo.path().join(".git/mark.sh"));
        std::fs::write(&prog, format!("#!/bin/sh\necho ran >> {}\ncat\n", marks.display()))
            .unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        for var in ["clean", "smudge", "process"] {
            let o = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["config", &format!("filter.evil.{var}"), &prog.display().to_string()])
                .output()
                .unwrap();
            assert!(o.status.success());
        }
        let dest = tempfile::tempdir().unwrap();
        export_head(repo.path(), "HEAD", dest.path(), T).unwrap();
        assert_eq!(std::fs::read_to_string(dest.path().join("a.txt")).unwrap(), "hello\n");
        assert!(!marks.exists(), "git archive ran a filter named in .git/config");
    }

    #[test]
    fn export_head_notes_submodules_and_export_ignore() {
        let repo = git_repo(&[
            (".gitmodules", "[submodule \"x\"]\n"),
            (".gitattributes", "secret.txt export-ignore\n"),
            ("secret.txt", "s"),
            ("keep.txt", "k"),
        ]);
        let dest = tempfile::tempdir().unwrap();
        let notes = export_head(repo.path(), "HEAD", dest.path(), T).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("submodules") && notes[1].contains("export-ignore"));
        assert!(!dest.path().join("secret.txt").exists() && dest.path().join("keep.txt").exists());
    }

    #[test]
    fn export_head_rejects_bad_heads_and_non_repos_with_a_reason() {
        let repo = git_repo(&[("a", "a")]);
        let dest = tempfile::tempdir().unwrap();
        assert!(export_head(repo.path(), "", dest.path(), T).unwrap_err().contains("refusing"));
        assert!(export_head(repo.path(), "--output=/tmp/x", dest.path(), T)
            .unwrap_err()
            .contains("refusing"));
        assert!(export_head(repo.path(), "no-such-rev", dest.path(), T)
            .unwrap_err()
            .contains("git failed"));
        let plain = tempfile::tempdir().unwrap();
        assert!(export_head(plain.path(), "HEAD", dest.path(), T)
            .unwrap_err()
            .contains("git failed"));
    }

    #[test]
    fn export_pipeline_times_out_and_kills_both_ends() {
        let mut slow = Command::new("sleep");
        slow.arg("30");
        let t = Instant::now();
        let e = export_pipeline(slow, Command::new("cat"), Duration::from_secs(1)).unwrap_err();
        assert_eq!(e, "timed out after 1s");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn export_pipeline_survives_large_stderr_and_reports_the_failure() {
        let mut noisy = Command::new("sh");
        noisy.args(["-c", "head -c 300000 /dev/zero | tr '\\0' e >&2; exit 3"]);
        let t = Instant::now();
        let e = export_pipeline(noisy, Command::new("cat"), Duration::from_secs(20)).unwrap_err();
        assert!(e.starts_with("sh failed: eee"), "{}", &e[..40.min(e.len())]);
        assert!(t.elapsed() < Duration::from_secs(15));
        assert!(
            export_pipeline(Command::new("true"), Command::new("cat"), Duration::from_secs(5))
                .is_ok()
        );
        assert_eq!(
            export_pipeline(
                Command::new("/no/such/prog"),
                Command::new("cat"),
                Duration::from_secs(5)
            )
            .unwrap_err(),
            "/no/such/prog is not installed"
        );
    }
}
