//! Parsing `git diff -U0` output. Pure text in, structure out.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
}

impl Hunk {
    /// Head-side lines the hunk added or changed (empty for a pure deletion).
    pub fn new_lines(&self) -> std::ops::Range<u32> {
        self.new_start..self.new_start + self.new_len
    }
    /// Base-side lines the hunk removed or changed (empty for a pure addition).
    pub fn old_lines(&self) -> std::ops::Range<u32> {
        self.old_start..self.old_start + self.old_len
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FileDiff {
    /// Path in the base tree; `None` for an added file.
    pub old_path: Option<String>,
    /// Path in the head tree; `None` for a deleted file.
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn is_rust(&self) -> bool {
        [&self.old_path, &self.new_path].iter().any(|p| p.as_deref().is_some_and(|p| p.ends_with(".rs")))
    }
}

/// Undo git's C-style quoting (`"a\tb"`, octal escapes). Unquoted input is returned as is.
fn unquote(s: &str) -> String {
    let s = s.trim_end_matches(['\t', '\r']);
    let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { return s.to_string() };
    let mut bytes = Vec::new();
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'"') => bytes.push(b'"'),
            Some(b'\\') => bytes.push(b'\\'),
            Some(d @ b'0'..=b'3') => {
                let mut v = d - b'0';
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + (n - b'0');
                        it.next();
                    }
                }
                bytes.push(v);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `a/path` or `b/path` (possibly quoted) -> `path`; `/dev/null` -> None.
fn side_path(raw: &str) -> Option<String> {
    let p = unquote(raw);
    if p == "/dev/null" {
        return None;
    }
    Some(p.split_once('/').map_or(p.clone(), |(_, rest)| rest.to_string()))
}

/// `@@ -a[,b] +c[,d] @@ ...`
fn parse_hunk(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((s, l)) => Some((s.parse().ok()?, l.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let ((old_start, old_len), (new_start, new_len)) = (range(old)?, range(new)?);
    Some(Hunk { old_start, old_len, new_start, new_len })
}

/// Parse unified diff text (`git diff -U0 -M`). Binary files and mode-only changes yield entries
/// without hunks. Unrecognised lines are ignored.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunks = false;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff::default());
            in_hunks = false;
            continue;
        }
        let Some(cur) = files.last_mut() else { continue };
        if in_hunks {
            if let Some(h) = parse_hunk(line) {
                cur.hunks.push(h);
                continue;
            }
            // Body lines (`+`, `-`, `\ No newline`) are skipped; a new header ends the hunks.
            if line.starts_with(['+', '-', '\\', ' ']) {
                continue;
            }
        }
        if let Some(p) = line.strip_prefix("rename from ") {
            cur.old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            cur.new_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("--- ") {
            cur.old_path = side_path(p);
        } else if let Some(p) = line.strip_prefix("+++ ") {
            cur.new_path = side_path(p);
        } else if let Some(h) = parse_hunk(line) {
            cur.hunks.push(h);
            in_hunks = true;
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(os: u32, ol: u32, ns: u32, nl: u32) -> Hunk {
        Hunk { old_start: os, old_len: ol, new_start: ns, new_len: nl }
    }

    #[test]
    fn modified_added_deleted_and_hunk_forms() {
        let d = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -3 +3 @@ fn x() {
-old
+new
@@ -10,2 +10,3 @@ fn y() {
-a
-b
+c
+d
+e
@@ -20,0 +22,2 @@
+only added
+lines
@@ -30,4 +33,0 @@
-gone
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,3 @@
+x
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
--- a/src/old.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-x
";
        let f = parse_diff(d);
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].old_path.as_deref(), f[0].new_path.as_deref()), (Some("src/a.rs"), Some("src/a.rs")));
        assert_eq!(f[0].hunks, vec![h(3, 1, 3, 1), h(10, 2, 10, 3), h(20, 0, 22, 2), h(30, 4, 33, 0)]);
        assert_eq!((f[1].old_path.as_deref(), f[1].new_path.as_deref()), (None, Some("src/new.rs")));
        assert_eq!(f[1].hunks, vec![h(0, 0, 1, 3)]);
        assert_eq!((f[2].old_path.as_deref(), f[2].new_path.as_deref()), (Some("src/old.rs"), None));
        assert_eq!(f[2].hunks, vec![h(1, 2, 0, 0)]);
        assert_eq!(f[0].hunks[0].new_lines(), 3..4);
        assert_eq!(f[0].hunks[3].new_lines(), 33..33);
        assert_eq!(f[0].hunks[1].old_lines(), 10..12);
    }

    #[test]
    fn rename_with_and_without_edits() {
        let d = "\
diff --git a/src/a.rs b/src/b.rs
similarity index 90%
rename from src/a.rs
rename to src/b.rs
--- a/src/a.rs
+++ b/src/b.rs
@@ -5 +5 @@
-x
+y
diff --git a/src/c.rs b/src/d.rs
similarity index 100%
rename from src/c.rs
rename to src/d.rs
";
        let f = parse_diff(d);
        assert_eq!((f[0].old_path.as_deref(), f[0].new_path.as_deref()), (Some("src/a.rs"), Some("src/b.rs")));
        assert_eq!(f[0].hunks.len(), 1);
        assert_eq!((f[1].old_path.as_deref(), f[1].new_path.as_deref()), (Some("src/c.rs"), Some("src/d.rs")));
        assert!(f[1].hunks.is_empty());
        assert!(f[1].is_rust());
    }

    #[test]
    fn quoted_paths_binary_files_and_noise() {
        let d = "\
garbage before the first header
diff --git \"a/sp\\303\\251cial \\\"q\\\".rs\" \"b/sp\\303\\251cial \\\"q\\\".rs\"
--- \"a/sp\\303\\251cial \\\"q\\\".rs\"
+++ \"b/sp\\303\\251cial \\\"q\\\".rs\"
@@ -1 +1 @@
-a
+b
\\ No newline at end of file
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
diff --git a/with space.rs b/with space.rs
--- a/with space.rs\t
+++ b/with space.rs\t
@@ -2,0 +3 @@
+z
";
        let f = parse_diff(d);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].new_path.as_deref(), Some("spécial \"q\".rs"));
        assert_eq!(f[0].hunks.len(), 1);
        assert!(!f[1].is_rust() && f[1].hunks.is_empty());
        assert_eq!(f[2].new_path.as_deref(), Some("with space.rs"));
        assert_eq!(f[2].hunks, vec![h(2, 0, 3, 1)]);
    }

    #[test]
    fn body_lines_that_look_like_headers_are_not_headers() {
        // A removed line whose text starts with "-- " shows up as "--- ..." inside a hunk.
        let d = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,0 @@
--- a comment line
-x
";
        let f = parse_diff(d);
        assert_eq!(f[0].old_path.as_deref(), Some("a.rs"));
        assert_eq!(f[0].hunks.len(), 1);
    }

    #[test]
    fn empty_and_malformed_input() {
        assert!(parse_diff("").is_empty());
        assert!(parse_hunk("@@ nonsense").is_none());
        assert!(parse_hunk("@@ -x +1 @@").is_none());
        assert_eq!(parse_hunk("@@ -7 +8,0 @@ ctx"), Some(h(7, 1, 8, 0)));
    }
}
