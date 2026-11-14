use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Other,
    Canceled,
    Deadline,
    Help,
}

/// An error message chain. Wrapping keeps the kind, so cancellation survives context.
#[derive(Debug, Clone)]
pub struct Error {
    kind: Kind,
    msg: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error { kind: Kind::Other, msg: msg.into() }
    }

    pub fn canceled() -> Self {
        Error { kind: Kind::Canceled, msg: "context canceled".into() }
    }

    pub fn deadline() -> Self {
        Error { kind: Kind::Deadline, msg: "context deadline exceeded".into() }
    }

    pub fn help() -> Self {
        Error { kind: Kind::Help, msg: "flag: help requested".into() }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn is_canceled(&self) -> bool {
        self.kind == Kind::Canceled
    }

    pub fn is_help(&self) -> bool {
        self.kind == Kind::Help
    }

    pub fn context(self, prefix: impl fmt::Display) -> Self {
        Error { kind: self.kind, msg: format!("{prefix}: {}", self.msg) }
    }

    /// Joins messages with newlines; the result is canceled if any part is.
    pub fn join(errors: Vec<Error>) -> Option<Error> {
        if errors.is_empty() {
            return None;
        }
        let kind = if errors.iter().any(Error::is_canceled) { Kind::Canceled } else { errors[0].kind };
        let msg = errors.iter().map(|e| e.msg.as_str()).collect::<Vec<_>>().join("\n");
        Some(Error { kind, msg })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::new(io_message(&e))
    }
}

impl From<lopdf::Error> for Error {
    fn from(e: lopdf::Error) -> Self {
        Error::new(e.to_string())
    }
}

/// Formats an I/O error like the C library does, without Rust's "(os error N)" suffix.
pub fn io_message(e: &io::Error) -> String {
    let text = e.to_string();
    let text = match text.rfind(" (os error ") {
        Some(at) if text.ends_with(')') => text[..at].to_string(),
        _ => text,
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if e.raw_os_error().is_some() => first.to_lowercase().chain(chars).collect(),
        _ => text,
    }
}

/// An error that names the failed operation and path.
pub fn path_error(op: &str, path: &Path, e: &io::Error) -> Error {
    Error::new(format!("{op} {}: {}", path.display(), io_message(e)))
}

#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::new(format!($($arg)*)))
    };
}

/// Cancellation shared by a run, its files, and their tools. A child can be
/// canceled alone or by a deadline; a canceled parent cancels every child.
#[derive(Clone)]
pub struct Ctx(Arc<Inner>);

struct Inner {
    flag: Arc<AtomicBool>,
    parent: Option<Ctx>,
    deadline: Option<Instant>,
}

impl Ctx {
    pub fn new(flag: Arc<AtomicBool>) -> Ctx {
        Ctx(Arc::new(Inner { flag, parent: None, deadline: None }))
    }

    pub fn background() -> Ctx {
        Ctx::new(Arc::new(AtomicBool::new(false)))
    }

    pub fn child(&self) -> Ctx {
        Ctx(Arc::new(Inner { flag: Arc::new(AtomicBool::new(false)), parent: Some(self.clone()), deadline: None }))
    }

    pub fn with_timeout(&self, timeout: Duration) -> Ctx {
        Ctx(Arc::new(Inner { flag: Arc::new(AtomicBool::new(false)), parent: Some(self.clone()), deadline: Some(Instant::now() + timeout) }))
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
    }

    pub fn err(&self) -> Option<Error> {
        let mut current = Some(self);
        while let Some(ctx) = current {
            if ctx.0.flag.load(Ordering::SeqCst) {
                return Some(Error::canceled());
            }
            if ctx.0.deadline.is_some_and(|d| Instant::now() >= d) {
                return Some(Error::deadline());
            }
            current = ctx.0.parent.as_ref();
        }
        None
    }

    pub fn check(&self) -> Result<()> {
        match self.err() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn done(&self) -> bool {
        self.err().is_some()
    }
}
