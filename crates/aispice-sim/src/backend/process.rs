//! Running a simulator process safely.
//!
//! Every simulator runs from an argument vector (never a shell), with stdin
//! closed, a timeout, a cancellation token, and stdout and stderr captured up
//! to a size cap. On Unix the child leads its own process group so a timeout
//! or cancel kills everything it started: Wine's Windows process and the
//! virtual X server from `xvfb-run` are grandchildren that outlive a plain
//! kill of the direct child.

use super::SimError;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Captured stdout and stderr are each capped at this many bytes; the first
/// and last halves are kept, since errors come early and results late.
pub const OUTPUT_CAP: usize = 1 << 20;

/// After the child exits, how long to wait for its pipes to close. A daemon
/// it started (wineserver) may hold them open indefinitely.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// A Landlock domain to confine a child to before it execs: it may read and
/// execute `read_exec`, read `read`, write under `write`, and (always) make
/// no TCP bind or connection. Built in the parent and enforced in the
/// child's `pre_exec` hook by [`super::confine`] (Linux only).
#[derive(Debug, Clone, Default)]
pub struct Landlock {
    pub read_exec: Vec<PathBuf>,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

/// How a child is confined on Linux: resource limits, and an optional
/// Landlock domain. Ignored on other systems, where it is never set.
#[derive(Debug, Clone, Default)]
pub struct Confinement {
    pub landlock: Option<Landlock>,
    /// `RLIMIT_FSIZE`: the largest file the child may write.
    pub rlimit_fsize: Option<u64>,
    /// `RLIMIT_AS`: the child's address space.
    pub rlimit_as: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    /// Start from an empty environment instead of aispice's own, so the
    /// child sees only `env`.
    pub clear_env: bool,
    /// How to confine the child before it runs (Linux only).
    pub confine: Option<Confinement>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            clear_env: false,
            confine: None,
        }
    }

    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn env(mut self, k: impl Into<OsString>, v: impl Into<OsString>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// The command line for logs and error messages.
    pub fn display(&self) -> String {
        std::iter::once(self.program.display().to_string())
            .chain(self.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Exit code; `None` when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout then stderr, as one log.
    pub fn combined(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (_, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
        }
    }
}

/// Keeps the first and last `cap / 2` bytes of a stream.
#[derive(Debug, Default)]
struct Capped {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
    cap: usize,
}

impl Capped {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len();
        let half = self.cap / 2;
        if self.head.len() < half {
            let n = (half - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
        }
        self.tail.extend(bytes);
        while self.tail.len() > half {
            self.tail.pop_front();
        }
    }

    fn text(&self) -> String {
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        let kept = self.head.len() + self.tail.len();
        if self.total > kept {
            out.push_str(&format!(
                "\n[aispice: {} bytes of output omitted]\n",
                self.total - kept
            ));
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        out.push_str(&String::from_utf8_lossy(&tail));
        out
    }
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(mut r: R, into: Arc<Mutex<Capped>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => into.lock().expect("capture lock").push(&buf[..n]),
        }
    }
}

/// Run a process to completion, or until `timeout` or `cancel`.
pub async fn run(
    spec: &ProcessSpec,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ProcessOutput, SimError> {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if spec.clear_env {
        cmd.env_clear();
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    // Confine the child before it runs (resource limits, and a Landlock
    // domain when one is given). The ruleset is built here, in the parent;
    // the pre_exec hook only enforces it. On other systems a confinement is
    // never set.
    #[cfg(target_os = "linux")]
    if let Some(c) = &spec.confine {
        super::confine::apply(&mut cmd, c).map_err(|e| SimError::Spawn {
            program: spec.program.display().to_string(),
            message: format!("could not confine the process: {e}"),
        })?;
    }
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| SimError::Spawn {
        program: spec.program.display().to_string(),
        message: e.to_string(),
    })?;
    let pid = child.id();
    let out = Arc::new(Mutex::new(Capped::new(OUTPUT_CAP)));
    let err = Arc::new(Mutex::new(Capped::new(OUTPUT_CAP)));
    let mut readers = Vec::new();
    if let Some(s) = child.stdout.take() {
        readers.push(tokio::spawn(drain(s, out.clone())));
    }
    if let Some(s) = child.stderr.take() {
        readers.push(tokio::spawn(drain(s, err.clone())));
    }

    enum End {
        Exited(std::io::Result<std::process::ExitStatus>),
        Timeout,
        Cancelled,
    }
    let end = tokio::select! {
        s = child.wait() => End::Exited(s),
        _ = tokio::time::sleep(timeout) => End::Timeout,
        _ = cancel.cancelled() => End::Cancelled,
    };
    let status = match end {
        End::Exited(s) => {
            s.map_err(|e| SimError::Io(format!("waiting for {}: {e}", spec.display())))?
        }
        End::Timeout | End::Cancelled => {
            kill_tree(pid).await;
            let _ = child.kill().await;
            for r in &readers {
                r.abort();
            }
            return Err(if matches!(end, End::Timeout) {
                SimError::Timeout(timeout)
            } else {
                SimError::Cancelled
            });
        }
    };
    for r in readers {
        if tokio::time::timeout(PIPE_GRACE, r).await.is_err() {
            tracing::debug!("{}: output pipe still open after exit", spec.display());
        }
    }
    let stdout = out.lock().expect("capture lock").text();
    let stderr = err.lock().expect("capture lock").text();
    Ok(ProcessOutput {
        code: status.code(),
        stdout,
        stderr,
        elapsed: start.elapsed(),
    })
}

/// Kill the child's whole process group (Unix) or process tree (Windows).
async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    let mut killer = {
        let mut c = tokio::process::Command::new("kill");
        c.arg("-KILL").arg("--").arg(format!("-{pid}"));
        c
    };
    #[cfg(windows)]
    let mut killer = {
        let mut c = tokio::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID"]).arg(pid.to_string());
        c
    };
    #[cfg(any(unix, windows))]
    {
        killer
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = tokio::time::timeout(Duration::from_secs(5), killer.status()).await;
    }
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_keeps_head_and_tail() {
        let mut c = Capped::new(8);
        c.push(b"abcdefghijklmnop");
        let t = c.text();
        assert!(t.starts_with("abcd"));
        assert!(t.ends_with("mnop"));
        assert!(t.contains("8 bytes of output omitted"));
        let mut small = Capped::new(100);
        small.push(b"hello ");
        small.push(b"world");
        assert_eq!(small.text(), "hello world");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_captures_and_times_out() {
        let dir = std::env::temp_dir();
        let cancel = CancellationToken::new();
        let ok = run(
            &ProcessSpec::new("sh", &dir)
                .arg("-c")
                .arg("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(ok.code, Some(3));
        assert_eq!(ok.stdout.trim(), "out");
        assert_eq!(ok.stderr.trim(), "err");

        let slow = run(
            &ProcessSpec::new("sh", &dir)
                .arg("-c")
                .arg("sleep 30 & sleep 30"),
            Duration::from_millis(200),
            &cancel,
        )
        .await;
        assert!(matches!(slow, Err(SimError::Timeout(_))));

        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel();
        });
        let cancelled = run(
            &ProcessSpec::new("sleep", &dir).arg("30"),
            Duration::from_secs(30),
            &token,
        )
        .await;
        assert!(matches!(cancelled, Err(SimError::Cancelled)));

        let missing = run(
            &ProcessSpec::new("/nonexistent/aispice-sim-binary", &dir),
            Duration::from_secs(1),
            &cancel,
        )
        .await;
        assert!(matches!(missing, Err(SimError::Spawn { .. })));
    }
}
