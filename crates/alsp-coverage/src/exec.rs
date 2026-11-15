//! Running an external tool with a timeout and bounded memory. Shared by the coverage and
//! mutant-listing steps, which both shell out to cargo subcommands.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    /// Only the last `tail_bytes` of stderr (cargo is chatty; the end says why it stopped).
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Keep the last `n` bytes of `s`, on a char boundary, marking the cut.
pub fn tail(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = s.len() - n;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("...{}", &s[cut..])
}

/// Calls `notify` the first time `marker` shows up in the stream it watches.
pub struct Watch {
    pub marker: &'static str,
    pub notify: Box<dyn Fn() + Send>,
}

fn drain(mut r: impl Read + Send + 'static, keep: Option<usize>, watch: Option<Watch>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut all, mut chunk) = (Vec::new(), [0u8; 8192]);
        let mut pending = watch;
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&chunk[..n]);
            // `all` still holds the end of the previous chunk, so a marker split across two reads is found.
            if pending.as_ref().is_some_and(|w| String::from_utf8_lossy(&all).contains(w.marker)) {
                (pending.take().expect("checked above").notify)();
            }
            if let Some(k) = keep {
                if all.len() > 2 * k {
                    all.drain(..all.len() - k);
                }
            }
        }
        all
    })
}

/// Run `cmd` (stdin closed), draining both pipes on threads so a full pipe never stalls it. Stdout
/// is kept whole; stderr is bounded to its last `tail_bytes`. `Err` is a one-line reason: the
/// program could not be started, or it ran past `timeout` and was killed.
pub fn run(cmd: &mut Command, timeout: Duration, tail_bytes: usize) -> Result<Output, String> {
    run_watching(cmd, timeout, tail_bytes, None)
}

/// [`run`], and `watch` fires once as soon as its marker appears on stderr, while the program is
/// still running (cargo says "Blocking waiting for file lock" there before it blocks).
pub fn run_watching(cmd: &mut Command, timeout: Duration, tail_bytes: usize, watch: Option<Watch>) -> Result<Output, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {name}: {e}"))?;
    let out = drain(child.stdout.take().expect("stdout is piped"), None, None);
    let err = drain(child.stderr.take().expect("stderr is piped"), Some(tail_bytes), watch);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("waiting for {name} failed: {e}")),
        }
    };
    let text = |h: std::thread::JoinHandle<Vec<u8>>| String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned();
    Ok(Output { code: status.code(), stdout: text(out), stderr: tail(&text(err), tail_bytes) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    fn captures_stdout_whole_and_stderr_tail_and_exit_code() {
        let o = run(&mut sh("echo out; echo err >&2; exit 3"), Duration::from_secs(10), 100).unwrap();
        assert_eq!((o.code, o.stdout.as_str(), o.stderr.as_str(), o.success()), (Some(3), "out\n", "err", false));
        let o = run(&mut sh("exit 0"), Duration::from_secs(10), 100).unwrap();
        assert!(o.success());
    }

    #[test]
    fn large_stderr_is_bounded_to_its_tail() {
        let o = run(&mut sh("i=0; while [ $i -lt 3000 ]; do echo line-$i >&2; i=$((i+1)); done"), Duration::from_secs(20), 200).unwrap();
        assert!(o.stderr.len() <= 210 && o.stderr.ends_with("line-2999"), "{}", o.stderr);
    }

    #[test]
    fn timeout_and_missing_program_are_errors_not_panics() {
        let e = run(&mut sh("sleep 5"), Duration::from_millis(100), 10).err().unwrap();
        assert!(e.contains("timed out"), "{e}");
        let e = run(&mut Command::new("/no/such/program"), Duration::from_secs(1), 10).err().unwrap();
        assert!(e.contains("cannot run"), "{e}");
    }

    #[test]
    fn watch_fires_once_while_the_program_still_runs_even_when_the_marker_is_split_across_reads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let seen = Arc::new(AtomicUsize::new(0));
        let at_fire = Arc::new(AtomicUsize::new(usize::MAX));
        let (s2, a2) = (seen.clone(), at_fire.clone());
        let started = Instant::now();
        let w = Watch {
            marker: "file lock",
            notify: Box::new(move || {
                s2.fetch_add(1, Ordering::SeqCst);
                a2.store(started.elapsed().as_millis() as usize, Ordering::SeqCst);
            }),
        };
        // The marker arrives in two writes 200 ms apart, then the program lingers 600 ms and repeats it.
        let script = "printf 'Blocking waiting for fi' >&2; sleep 0.2; printf 'le lock\\n' >&2; sleep 0.6; echo 'file lock again' >&2";
        let o = run_watching(&mut sh(script), Duration::from_secs(10), 200, Some(w)).unwrap();
        assert!(o.success());
        assert_eq!(seen.load(Ordering::SeqCst), 1, "once, not per occurrence");
        assert!(at_fire.load(Ordering::SeqCst) < 700, "fired before the program ended: {}ms", at_fire.load(Ordering::SeqCst));
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("  short  ", 100), "short");
        let long = format!("{}é{}", "a".repeat(700), "z".repeat(599));
        let t = tail(&long, 600);
        assert!(t.starts_with("...") && t.ends_with('z') && t.len() < 700);
    }
}
