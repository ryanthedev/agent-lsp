//! Review settings: the optional, hand-written `<worktree>/.alsp/enabled`.
//!
//! Format: `key = value` lines, blank lines and whole-line `#` comments. Keys: `agent` (list from
//! claude, codex, agy; the agents `alsp review` runs by default), plus `with` and `timeout`, which are
//! validated for compatibility with older settings files but not used. An empty
//! file means defaults. The file is untrusted input (a repository can ship one), so every key and
//! value is checked against an allowlist, and a file tracked by git is never read.

use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const WITH_VALUES: [&str; 2] = ["coverage", "gaps"];
const AGENT_VALUES: [&str; 3] = ["claude", "codex", "agy"];
const MAX_TIMEOUT_SECS: u64 = 3600;
const MAX_FILE: u64 = 4096;
const TRACKED_MSG: &str = "`.alsp/enabled` is tracked by git; refusing repository-provided review settings";

/// `None` = not set.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings { pub with: Option<Vec<String>>, pub agent: Option<Vec<String>>, pub timeout: Option<u64> }

/// Quote untrusted text for an error message: escaped, short.
fn shown(s: &str) -> String { format!("{:?}", s.chars().take(40).collect::<String>()) }

fn list(key: &str, raw: &[String], allowed: &[&str]) -> Result<Option<Vec<String>>, String> {
    let mut out: Vec<String> = Vec::new();
    for v in raw.iter().map(|v| v.trim()) {
        if !allowed.contains(&v) { return Err(format!("{key}: unknown value {} (allowed: {})", shown(v), allowed.join(", "))); }
        if !out.iter().any(|o| o == v) { out.push(v.to_string()); }
    }
    Ok((!out.is_empty()).then_some(out))
}

impl Settings {
    /// Parse and validate the content of an `enabled` file.
    pub fn parse(text: &str) -> Result<Settings, String> {
        let mut s = Settings::default();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let at = |m: String| format!("line {}: {m}", i + 1);
            let Some((key, value)) = line.split_once('=') else { return Err(at(format!("expected `key = value`, got {}", shown(line)))) };
            let (key, value) = (key.trim(), value.trim());
            let items = || value.split(',').map(str::to_string).collect::<Vec<_>>();
            let dup = |set: bool| if set { Err(at(format!("{key} is set twice"))) } else { Ok(()) };
            match key {
                "with" => { dup(s.with.is_some())?; s.with = Some(items()); }
                "agent" => { dup(s.agent.is_some())?; s.agent = Some(items()); }
                "timeout" => {
                    dup(s.timeout.is_some())?;
                    s.timeout = Some(value.parse::<u64>().map_err(|_| at(format!("timeout: {} is not a whole number of seconds", shown(value))))?);
                }
                _ => return Err(at(format!("unknown key {} (allowed: with, agent, timeout)", shown(key)))),
            }
            if value.is_empty() { return Err(at(format!("{key} has no value"))); }
        }
        if let Some(t) = s.timeout { if !(1..=MAX_TIMEOUT_SECS).contains(&t) { return Err(format!("timeout: {t} is not between 1 and {MAX_TIMEOUT_SECS} seconds")); } }
        Ok(Settings {
            with: list("with", s.with.as_deref().unwrap_or_default(), &WITH_VALUES)?,
            agent: list("agent", s.agent.as_deref().unwrap_or_default(), &AGENT_VALUES)?,
            timeout: s.timeout,
        })
    }
}

fn refuse(path: &Path, what: &str) -> String { format!("refusing {}: not a plain {what} (symlink or other)", path.display()) }

/// Whether anything under `<root>/.alsp` (any letter case) is tracked by git (in the index or in HEAD). A committed file is
/// repository-provided, so it must never supply settings or review records. Not a git repository: nothing is
/// tracked. Any other git failure is an error (fail closed).
pub fn enabled_is_tracked(root: &Path) -> Result<bool, String> {
    let run = |args: &[&str]| -> Result<std::process::Output, String> {
        alsp_evidence::git_command(root)?.args(args)
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").env_remove("GIT_COMMON_DIR")
            .output().map_err(|e| format!("cannot run git to check whether .alsp/enabled is tracked: {e}"))
    };
    // `:(icase)` also catches `.ALSP/Enabled` and friends committed on a case-insensitive filesystem;
    // any tracked path under any spelling of `.alsp` is repository-provided state.
    // A directory git calls "not a git repository" (also when listing its config fails) has nothing tracked.
    let idx = match run(&["ls-files", "--", ":(icase).alsp"]) {
        Err(e) if e.contains("not a git repository") => return Ok(false),
        r => r?,
    };
    if !idx.status.success() {
        if String::from_utf8_lossy(&idx.stderr).contains("not a git repository") { return Ok(false); }
        return Err(format!("cannot check whether .alsp/enabled is tracked: {}", String::from_utf8_lossy(&idx.stderr).trim()));
    }
    if !idx.stdout.is_empty() { return Ok(true); }
    // Removed from the index but still committed. Only a genuinely unborn HEAD (a branch with no
    // commit yet) means "not in HEAD"; any other failure of git is an error, never "untracked".
    if let Ok(sym) = run(&["symbolic-ref", "-q", "HEAD"]) {
        let name = String::from_utf8_lossy(&sym.stdout).trim().to_string();
        if sym.status.success() && !name.is_empty() && run(&["show-ref", "--verify", "-q", &name])?.status.code() == Some(1) { return Ok(false); }
    }
    let tree = run(&["ls-tree", "--full-tree", "--name-only", "HEAD"])?;
    if !tree.status.success() { return Err(format!("cannot check whether .alsp/enabled is committed: {}", String::from_utf8_lossy(&tree.stderr).trim())); }
    // ls-tree has no `:(icase)`: a top-level entry named .alsp in any letter case means committed content under it.
    Ok(tree.stdout.split(|&b| b == b'\n').any(|n| n.eq_ignore_ascii_case(b".alsp")))
}

/// Settings from `<worktree>/.alsp/enabled`. A missing file means defaults; a tracked `.alsp`,
/// a symlinked `.alsp` or `enabled`, a non-regular or oversized file, or invalid content is an error.
pub fn read_enabled(repo: &Path) -> Result<Settings, String> {
    let dir = repo.join(".alsp");
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(refuse(&dir, "directory")),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    }
    let path = dir.join("enabled");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(refuse(&path, "file")),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }
    if enabled_is_tracked(repo)? { return Err(TRACKED_MSG.to_string()); }
    let mut bytes = Vec::new();
    fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path)
        .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE { return Err(format!("{} is larger than {MAX_FILE} bytes", path.display())); }
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    Settings::parse(&text).map_err(|e| format!("invalid {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn s(with: Option<&[&str]>, agent: Option<&[&str]>, timeout: Option<u64>) -> Settings {
        let v = |x: Option<&[&str]>| x.map(|x| x.iter().map(|s| s.to_string()).collect());
        Settings { with: v(with), agent: v(agent), timeout }
    }
    fn git(dir: &Path, args: &[&str]) {
        let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    /// A repo with one commit and no `.alsp`.
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q", "-b", "main"]);
        fs::write(d.path().join("a.txt"), "x").unwrap();
        git(d.path(), &["add", "."]); git(d.path(), &["commit", "-q", "-m", "c0"]);
        d
    }
    /// Write an untracked `.alsp/enabled` holding `agent = claude`.
    fn enabled(d: &Path) { fs::create_dir_all(d.join(".alsp")).unwrap(); fs::write(d.join(".alsp/enabled"), "agent = claude\n").unwrap(); }
    /// Commit `path` (any case) force-added, with core.ignorecase=true like macOS.
    fn track(d: &Path, path: &str) {
        git(d, &["config", "core.ignorecase", "true"]);
        let p = d.join(path); fs::create_dir_all(p.parent().unwrap()).unwrap(); fs::write(&p, "agent = claude\n").unwrap();
        git(d, &["add", "-f", path]); git(d, &["commit", "-q", "-m", "track"]);
    }
    fn tracked_err(d: &Path) { assert!(read_enabled(d).unwrap_err().contains("tracked by git"), "{:?}", read_enabled(d)); }

    #[test]
    fn parses_comments_blank_lines_and_spacing() {
        let got = Settings::parse("# note\n\n with=coverage , gaps\nagent = claude,codex,agy\r\ntimeout =  90\n").unwrap();
        assert_eq!(got, s(Some(&["coverage", "gaps"]), Some(&["claude", "codex", "agy"]), Some(90)));
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
        assert_eq!(Settings::parse("# only a comment\n").unwrap(), Settings::default());
    }

    #[test]
    fn rejects_unknown_keys_values_and_bad_timeouts() {
        for bad in ["bogus = 1", "with = coverage,nope", "with = coverage;rm", "agent = gpt", "agent = claude,", "agent =", "with", "= x",
            "timeout = 0", "timeout = 3601", "timeout = -1", "timeout = 1.5", "timeout = abc", "timeout = 99999999999999999999999",
            "with = gaps\nwith = coverage", "WITH = gaps", "with = Coverage"] {
            assert!(Settings::parse(bad).is_err(), "accepted {bad:?}");
        }
        assert!(Settings::parse("timeout = 3600").is_ok() && Settings::parse("timeout = 1").is_ok());
    }

    #[test]
    fn duplicate_values_collapse() {
        assert_eq!(Settings::parse("with = gaps,gaps").unwrap(), s(Some(&["gaps"]), None, None));
    }

    #[test]
    fn error_text_is_escaped_and_short() {
        let e = Settings::parse(&format!("a\x1b[31m{} = 1", "z".repeat(500))).unwrap_err();
        assert!(!e.contains('\x1b') && e.len() < 200, "{e}");
    }

    #[test]
    fn read_enabled_reads_an_untracked_file_and_defaults_when_absent() {
        let d = repo(); enabled(d.path());
        assert_eq!(read_enabled(d.path()).unwrap().agent, Some(vec!["claude".into()]));
        fs::remove_file(d.path().join(".alsp/enabled")).unwrap();
        assert_eq!(read_enabled(d.path()).unwrap(), Settings::default());
        fs::remove_dir(d.path().join(".alsp")).unwrap();
        assert_eq!(read_enabled(d.path()).unwrap(), Settings::default());
    }

    #[test]
    fn unborn_head_with_an_untracked_enabled_file_is_not_tracked() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        fs::create_dir(d.path().join(".alsp")).unwrap(); fs::write(d.path().join(".alsp/enabled"), "timeout = 5\n").unwrap();
        assert!(!enabled_is_tracked(d.path()).unwrap());
        assert_eq!(read_enabled(d.path()).unwrap().timeout, Some(5));
    }

    #[test]
    fn not_a_git_repository_means_nothing_is_tracked() {
        let d = tempfile::tempdir().unwrap();
        assert!(!enabled_is_tracked(d.path()).unwrap());
    }

    #[test]
    fn read_enabled_refuses_symlinks_oversize_and_non_utf8_files() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join(".alsp")).unwrap();
        let target = d.path().join("target"); fs::write(&target, "with = gaps\n").unwrap();
        std::os::unix::fs::symlink(&target, d.path().join(".alsp/enabled")).unwrap();
        assert!(read_enabled(d.path()).unwrap_err().contains("refusing"));
        fs::remove_file(d.path().join(".alsp/enabled")).unwrap();
        fs::write(d.path().join(".alsp/enabled"), "#".repeat(5000)).unwrap();
        assert!(read_enabled(d.path()).unwrap_err().contains("larger"));
        fs::write(d.path().join(".alsp/enabled"), [0xff, 0xfe]).unwrap();
        assert!(read_enabled(d.path()).unwrap_err().contains("UTF-8"));
        fs::write(d.path().join(".alsp/enabled"), "agent = nobody\n").unwrap();
        assert!(read_enabled(d.path()).unwrap_err().contains("invalid"));
    }

    #[test]
    fn read_enabled_refuses_a_symlinked_alsp_folder() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real"); fs::create_dir(&real).unwrap(); fs::write(real.join("enabled"), "agent = claude\n").unwrap();
        std::os::unix::fs::symlink(&real, d.path().join(".alsp")).unwrap();
        assert!(read_enabled(d.path()).unwrap_err().contains("refusing"));
    }

    #[test]
    fn a_tracked_enabled_file_is_never_read() {
        let d = repo(); enabled(d.path());
        git(d.path(), &["add", "-f", ".alsp/enabled"]); git(d.path(), &["commit", "-q", "-m", "track enabled"]);
        tracked_err(d.path());
    }

    #[test]
    fn staged_only_and_head_only_tracked_enabled_files_are_never_read() {
        let d = repo(); enabled(d.path());
        git(d.path(), &["add", "-f", ".alsp/enabled"]); // index only, not committed
        tracked_err(d.path());
        git(d.path(), &["commit", "-q", "-m", "track enabled"]);
        git(d.path(), &["rm", "-q", "--cached", ".alsp/enabled"]); // in HEAD, gone from the index
        tracked_err(d.path());
    }

    #[test]
    fn case_variant_tracked_alsp_files_are_never_read() {
        for path in [".ALSP/Enabled", ".Alsp/enabled", ".alsp/other-file", ".alsp/ENABLED"] {
            let d = repo(); track(d.path(), path);
            // Mimic a case-insensitive checkout: the settings file reads as .alsp/enabled.
            enabled(d.path());
            assert!(read_enabled(d.path()).is_err(), "{path}");
        }
    }

    #[test]
    fn case_variant_head_only_tracked_is_never_read() {
        let d = repo(); track(d.path(), ".ALSP/Enabled");
        git(d.path(), &["rm", "-q", "--cached", ".ALSP/Enabled"]); // in HEAD, gone from the index
        enabled(d.path());
        assert!(read_enabled(d.path()).is_err());
    }
}
