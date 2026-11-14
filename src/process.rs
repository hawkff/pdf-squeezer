use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Ctx, io_message};

/// A shared, locked stream such as the user's stderr or one file's prefixed view of it.
pub type Out = Arc<Mutex<dyn Write + Send>>;

pub fn write_out(out: &Out, data: &[u8]) {
    if let Ok(mut out) = out.lock() {
        let _ = out.write_all(data);
        let _ = out.flush();
    }
}

const DIAGNOSTIC_LIMIT: usize = 1 << 20;
const WAIT_DELAY: Duration = Duration::from_secs(5);

/// Bounds diagnostic output without blocking a child on an unread pipe.
/// The image-error marker is tracked even after the retained text is truncated.
#[derive(Default)]
pub struct BoundedBuffer {
    data: Vec<u8>,
    pub truncated: bool,
    pub image_error: bool,
    tail: Vec<u8>,
}

impl BoundedBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, p: &[u8]) {
        let mut combined = std::mem::take(&mut self.tail);
        combined.extend_from_slice(p);
        self.image_error = self.image_error || contains(&combined, b"recoverable image error");
        self.tail = combined[combined.len().saturating_sub(64)..].to_vec();
        let n = p.len().min(DIAGNOSTIC_LIMIT - self.data.len());
        self.data.extend_from_slice(&p[..n]);
        self.truncated = self.truncated || n != p.len();
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn set_bytes(&mut self, data: Vec<u8>) {
        self.data = data;
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

pub type Shared = Arc<Mutex<BoundedBuffer>>;

pub fn shared() -> Shared {
    Arc::new(Mutex::new(BoundedBuffer::new()))
}

/// Where a child's output goes.
pub enum Sink {
    Null,
    File(File),
    Buffer(Shared),
    /// Keep a bounded copy and stream everything to the user as well.
    Tee(Shared, Out),
}

pub struct Tool {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Option<Vec<u8>>,
    pub stdout: Sink,
    pub stderr: Sink,
    /// Send stderr through the stdout pipe, so both keep their relative order.
    pub combined: bool,
}

impl Tool {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Tool { program: program.into(), args: Vec::new(), env: Vec::new(), stdin: None, stdout: Sink::Null, stderr: Sink::Null, combined: false }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env.push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Points the tool's scratch files at the private staging directory, which is
    /// removed even when a canceled tool is killed before its own cleanup.
    pub fn private_temp(self, dir: &Path) -> Self {
        self.env("TMPDIR", dir).env("TEMP", dir)
    }
}

#[derive(Debug)]
pub enum Failure {
    Start(PathBuf, io::Error),
    Status(ExitStatus),
    /// The tool succeeded, but a helper kept its output open past the grace period.
    WaitDelay,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Start(program, e) => write!(f, "fork/exec {}: {}", program.display(), io_message(e)),
            Failure::Status(status) => f.write_str(&status_text(status)),
            Failure::WaitDelay => f.write_str("exec: WaitDelay expired before I/O complete"),
        }
    }
}

pub fn status_text(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    match status.signal() {
        Some(signal) => format!("signal: {}", signal_name(signal)),
        None => "unknown exit status".into(),
    }
}

fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "hangup",
        libc::SIGINT => "interrupt",
        libc::SIGQUIT => "quit",
        libc::SIGILL => "illegal instruction",
        libc::SIGTRAP => "trace/breakpoint trap",
        libc::SIGABRT => "aborted",
        libc::SIGBUS => "bus error",
        libc::SIGFPE => "floating point exception",
        libc::SIGKILL => "killed",
        libc::SIGSEGV => "segmentation fault",
        libc::SIGPIPE => "broken pipe",
        libc::SIGALRM => "alarm clock",
        libc::SIGTERM => "terminated",
        _ => return format!("signal {signal}"),
    };
    name.into()
}

/// Runs a tool in its own process group. Cancellation kills the whole group, so a
/// tool cannot leave helpers behind. Output pipes get a grace period after exit,
/// then close, so no output arrives after this returns.
pub fn run(ctx: &Ctx, tool: Tool) -> Result<(), Failure> {
    let Tool { program, args, env, stdin, stdout, stderr, combined } = tool;
    let start = |e| Failure::Start(program.clone(), e);
    let mut cmd = Command::new(&program);
    cmd.args(&args).process_group(0);
    for (key, value) in &env {
        cmd.env(key, value);
    }
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut pipes: Vec<(File, Sink)> = Vec::new();
    let mut stdout_sink = None;
    let mut stderr_sink = None;
    if combined {
        let (reader, writer) = io::pipe().map_err(start)?;
        cmd.stderr(writer.try_clone().map_err(start)?);
        cmd.stdout(writer);
        pipes.push((File::from(OwnedFd::from(reader)), stdout));
        drop(stderr);
    } else {
        match stdout {
            Sink::Null => {
                cmd.stdout(Stdio::null());
            }
            Sink::File(file) => {
                cmd.stdout(Stdio::from(file));
            }
            sink => {
                cmd.stdout(Stdio::piped());
                stdout_sink = Some(sink);
            }
        }
        match stderr {
            Sink::Null => {
                cmd.stderr(Stdio::null());
            }
            Sink::File(file) => {
                cmd.stderr(Stdio::from(file));
            }
            sink => {
                cmd.stderr(Stdio::piped());
                stderr_sink = Some(sink);
            }
        }
    }
    let mut child = cmd.spawn().map_err(start)?;
    // The command still holds the parent's copies of the pipe write ends.
    drop(cmd);
    if let (Some(sink), Some(pipe)) = (stdout_sink, child.stdout.take()) {
        pipes.push((File::from(OwnedFd::from(pipe)), sink));
    }
    if let (Some(sink), Some(pipe)) = (stderr_sink, child.stderr.take()) {
        pipes.push((File::from(OwnedFd::from(pipe)), sink));
    }
    let (done, finished) = mpsc::channel();
    let abandon = Arc::new(AtomicBool::new(false));
    let readers = pipes.len();
    for (mut pipe, sink) in pipes {
        let (done, abandon) = (done.clone(), abandon.clone());
        thread::spawn(move || {
            let mut chunk = vec![0u8; 32 << 10];
            let mut ready = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // Short waits let an abandoned reader drop its pipe.
            while !abandon.load(Ordering::Relaxed) {
                // SAFETY: poll reads and writes the one pollfd this thread owns.
                match unsafe { libc::poll(&mut ready, 1, 100) } {
                    0 => continue,
                    n if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                    _ => {}
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => deliver(&sink, &chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = done.send(());
        });
    }
    // Only the readers hold senders now, so a reader that dies cannot leave a wait hanging.
    drop(done);
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A tool that exits without reading its input is not an error here.
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill_group(child.id());
                let _ = child.wait();
                return Err(Failure::Start(program, e));
            }
        }
        if ctx.done() {
            kill_group(child.id());
            match child.wait() {
                Ok(status) => break status,
                Err(e) => return Err(Failure::Start(program, e)),
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    let deadline = Instant::now() + WAIT_DELAY;
    let mut open = readers;
    while open > 0 && finished.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok() {
        open -= 1;
    }
    if open > 0 {
        // As in Go, a delivery in progress finishes before the call returns.
        abandon.store(true, Ordering::Relaxed);
        while finished.recv().is_ok() {}
    }
    if !status.success() {
        return Err(Failure::Status(status));
    }
    if open > 0 { Err(Failure::WaitDelay) } else { Ok(()) }
}

fn deliver(sink: &Sink, data: &[u8]) {
    match sink {
        Sink::Buffer(buffer) => buffer.lock().unwrap().write(data),
        Sink::Tee(buffer, out) => {
            write_out(out, data);
            buffer.lock().unwrap().write(data);
        }
        Sink::Null | Sink::File(_) => {}
    }
}

fn kill_group(pid: u32) {
    // SAFETY: kill only sends a signal; a negative pid names the tool's own process group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// Finds an executable like a shell does. Names with a slash are used as given.
pub fn look_path(name: &OsStr) -> Option<PathBuf> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes.contains(&b'/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    let search = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&search) {
        let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
        let candidate = dir.join(name);
        if executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Passes on whole lines only, each starting with prefix, so lines from writers
/// that share a stream cannot mix. `close` writes an unfinished last line.
pub struct PrefixWriter {
    writer: Out,
    prefix: Vec<u8>,
    partial: Vec<u8>,
}

impl PrefixWriter {
    pub fn new(writer: Out, prefix: Vec<u8>) -> Self {
        PrefixWriter { writer, prefix, partial: Vec::new() }
    }

    pub fn close(&mut self) -> io::Result<()> {
        if self.partial.is_empty() {
            return Ok(());
        }
        self.write_all(b"\n")
    }
}

impl Write for PrefixWriter {
    fn write(&mut self, p: &[u8]) -> io::Result<usize> {
        self.partial.extend_from_slice(p);
        let mut out = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&self.prefix);
            out.extend_from_slice(&self.partial[..=i]);
            self.partial.drain(..=i);
        }
        if !out.is_empty() {
            let mut writer = self.writer.lock().map_err(|_| io::Error::other("poisoned writer"))?;
            writer.write_all(&out)?;
            writer.flush()?;
        }
        Ok(p.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, p: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(p);
            Ok(p.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn prefix_writer_passes_whole_lines() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out: Out = Arc::new(Mutex::new(Capture(seen.clone())));
        let mut w = PrefixWriter::new(out, b"a.pdf: ".to_vec());
        for (chunk, want) in [
            ("one\ntw", "a.pdf: one\n"),
            ("o\n", "a.pdf: one\na.pdf: two\n"),
            ("", "a.pdf: one\na.pdf: two\n"),
            ("three", "a.pdf: one\na.pdf: two\n"),
        ] {
            assert_eq!(w.write(chunk.as_bytes()).unwrap(), chunk.len());
            assert_eq!(String::from_utf8_lossy(&seen.lock().unwrap()), want);
        }
        w.close().unwrap();
        assert_eq!(String::from_utf8_lossy(&seen.lock().unwrap()), "a.pdf: one\na.pdf: two\na.pdf: three\n");
    }

    #[test]
    fn helpers_cannot_write_after_the_call() {
        let output = shared();
        let tool = Tool { stdout: Sink::Buffer(output.clone()), ..Tool::new("sh").args(["-c", "(sleep 6; echo late) & echo early"]) };
        let start = Instant::now();
        let failure = run(&Ctx::background(), tool).unwrap_err();
        assert!(matches!(failure, Failure::WaitDelay), "{failure}");
        assert!(start.elapsed() < Duration::from_secs(6));
        thread::sleep(Duration::from_secs(7).saturating_sub(start.elapsed()));
        assert_eq!(output.lock().unwrap().text(), "early\n");
    }

    /// Takes a while to accept each write and records when the last one finished.
    struct Slow(Arc<Mutex<Option<Instant>>>);

    impl Write for Slow {
        fn write(&mut self, p: &[u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_secs(7));
            *self.0.lock().unwrap() = Some(Instant::now());
            Ok(p.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn slow_output_finishes_before_the_call_returns() {
        let written = Arc::new(Mutex::new(None));
        let out: Out = Arc::new(Mutex::new(Slow(written.clone())));
        let tool = Tool { stdout: Sink::Tee(shared(), out), ..Tool::new("sh").args(["-c", "echo early"]) };
        let failure = run(&Ctx::background(), tool).unwrap_err();
        let returned = Instant::now();
        assert!(matches!(failure, Failure::WaitDelay), "{failure}");
        assert!(written.lock().unwrap().is_some_and(|at| at <= returned), "output was written after the call returned");
    }

    #[test]
    fn bounded_buffer_keeps_late_image_errors() {
        let mut b = BoundedBuffer::new();
        b.write(&vec![b'x'; 2 << 20]);
        assert_eq!(b.bytes().len(), 1 << 20);
        assert!(b.truncated);
        b.write(b"recoverable image ");
        b.write(b"error");
        assert!(b.image_error);
    }
}
