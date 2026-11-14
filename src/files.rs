use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::bail;
use crate::error::{Ctx, Error, Result, io_message, path_error};

/// Go's filepath.Ext: the suffix from the final dot in the last element.
pub fn go_ext(path: &OsStr) -> &[u8] {
    let bytes = path.as_bytes();
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b'/' => break,
            b'.' => return &bytes[i..],
            _ => {}
        }
    }
    b""
}

/// Go's filepath.Base.
pub fn go_base(path: &OsStr) -> OsString {
    let mut bytes = path.as_bytes();
    if bytes.is_empty() {
        return OsString::from(".");
    }
    while bytes.len() > 1 && bytes.ends_with(b"/") {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes == b"/" {
        return OsString::from("/");
    }
    let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    OsString::from_vec(bytes[start..].to_vec())
}

/// Go's filepath.Dir.
pub fn go_dir(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let end = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    clean(&bytes[..end])
}

/// Lexical cleanup like Go's filepath.Clean, without touching the filesystem.
pub fn clean(path: &[u8]) -> PathBuf {
    if path.is_empty() {
        return PathBuf::from(".");
    }
    let rooted = path[0] == b'/';
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if parts.last().is_some_and(|last| *last != b"..") {
                    parts.pop();
                } else if !rooted {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }
    let mut out = Vec::new();
    if rooted {
        out.push(b'/');
    }
    out.extend(parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    PathBuf::from(OsString::from_vec(out))
}

/// Go's filepath.Abs: an absolute, lexically cleaned path. Symlinks stay in place.
pub fn abs_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(clean(path.as_os_str().as_bytes()));
    }
    let cwd = std::env::current_dir().map_err(|e| Error::new(format!("getwd: {}", io_message(&e))))?;
    Ok(clean(cwd.join(path).as_os_str().as_bytes()))
}

fn random_suffix() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        bytes = nanos.to_le_bytes();
    }
    u32::from_le_bytes(bytes).to_string()
}

/// Creates a private directory with a random name, like os.MkdirTemp.
pub fn mkdir_temp(dir: &Path, prefix: &str) -> io::Result<PathBuf> {
    for _ in 0..10000 {
        let path = dir.join(format!("{prefix}{}", random_suffix()));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many temporary name collisions"))
}

/// Creates an empty private file `stage-*.pdf` and returns its path.
pub fn stage_path(dir: &Path) -> Result<PathBuf> {
    for _ in 0..10000 {
        let path = dir.join(format!("stage-{}.pdf", random_suffix()));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(path_error("open", &path, &e)),
        }
    }
    bail!("too many temporary name collisions in {}", dir.display())
}

/// The private staging directory next to the output. Python and Java tools cannot
/// open paths that are not valid UTF-8, so they reach such a directory through a
/// symlink in the system temporary directory.
pub struct Staging {
    pub path: PathBuf,
    work: PathBuf,
    alias: Option<PathBuf>,
}

impl Staging {
    pub fn create(output: &Path) -> Result<Staging> {
        let parent = go_dir(output);
        let work = mkdir_temp(&parent, ".pdf-squeezer-").map_err(|e| path_error("mkdirtemp", &parent, &e))?;
        if std::str::from_utf8(work.as_os_str().as_bytes()).is_ok() {
            return Ok(Staging { path: work.clone(), work, alias: None });
        }
        let temp = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()).unwrap_or_else(|| "/tmp".into());
        for base in [PathBuf::from(temp), PathBuf::from("/tmp")] {
            let Ok(alias) = mkdir_temp(&base, "pdf-squeezer-") else { continue };
            let link = alias.join("work");
            if std::str::from_utf8(link.as_os_str().as_bytes()).is_err() || std::os::unix::fs::symlink(&work, &link).is_err() {
                let _ = fs::remove_dir_all(&alias);
                continue;
            }
            return Ok(Staging { path: link, work, alias: Some(alias) });
        }
        // Native processing still works; only the external tools will refuse the path.
        Ok(Staging { path: work.clone(), work, alias: None })
    }

    pub fn remove(&self) {
        if let Some(alias) = &self.alias {
            // Removing the alias deletes the link, not its target.
            let _ = fs::remove_dir_all(alias);
        }
        let _ = fs::remove_dir_all(&self.work);
    }
}

/// Opens an input without blocking on a FIFO swapped in for the checked path.
pub fn open_input(path: &Path) -> Result<(File, fs::Metadata)> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).map_err(|e| path_error("open", path, &e))?;
    let info = file.metadata().map_err(|e| path_error("stat", path, &e))?;
    if !info.is_file() {
        bail!("input must be a regular PDF file");
    }
    Ok((file, info))
}

pub fn check_pdf(file: &File) -> Result<()> {
    let mut header = [0u8; 5];
    match file.read_exact_at(&mut header, 0) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => bail!("read PDF header: EOF"),
        Err(e) => bail!("read PDF header: {}", io_message(&e)),
    }
    if &header != b"%PDF-" {
        bail!("missing %PDF- header");
    }
    Ok(())
}

pub fn read_all(ctx: &Ctx, file: &mut File) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(file.metadata().map_or(0, |m| m.len() as usize));
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        ctx.check()?;
        let n = match file.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&chunk[..n]);
    }
}

pub fn copy_ctx(ctx: &Ctx, source: &mut dyn Read, dest: &mut dyn Write) -> Result<u64> {
    let mut chunk = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        ctx.check()?;
        let n = match source.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(total);
        }
        dest.write_all(&chunk[..n])?;
        total += n as u64;
    }
}

/// Writes data into an existing stage file.
pub fn write_stage(ctx: &Ctx, path: &Path, data: &[u8]) -> Result<()> {
    ctx.check()?;
    let mut file = OpenOptions::new().write(true).truncate(true).open(path).map_err(|e| path_error("open", path, &e))?;
    for chunk in data.chunks(1 << 20) {
        ctx.check()?;
        file.write_all(chunk)?;
    }
    Ok(())
}

/// Creates path exclusively and copies source into it, deleting it on failure.
// shortcut: hard-link publication falls back to this copy, which a crash can leave partial; add a no-replace rename where the platform has one.
pub fn write_new(ctx: &Ctx, path: &Path, source: &mut dyn Read) -> Result<u64> {
    ctx.check()?;
    let mut out =
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| path_error("open", path, &e).context("create output"))?;
    let result = (|| {
        let written = copy_ctx(ctx, source, &mut out)?;
        ctx.check()?;
        out.sync_all()?;
        Ok(written)
    })();
    if result.is_err() {
        drop(out);
        let _ = fs::remove_file(path);
    }
    result
}

/// Hard-links a finished stage to path: the name appears atomically and the link
/// fails if path exists. Filesystems without hard links get an exclusive copy.
pub fn publish_file(ctx: &Ctx, staged: &Path, path: &Path) -> Result<u64> {
    publish_with(ctx, staged, path, |from, to| fs::hard_link(from, to))
}

pub fn publish_with(ctx: &Ctx, staged: &Path, path: &Path, link: impl Fn(&Path, &Path) -> io::Result<()>) -> Result<u64> {
    // Tools may create stages with any mode. Outputs stay private, with their data on disk before the name.
    fs::set_permissions(staged, fs::Permissions::from_mode(0o600)).map_err(|e| path_error("chmod", staged, &e))?;
    let mut source = OpenOptions::new().read(true).write(true).open(staged).map_err(|e| path_error("open", staged, &e))?;
    let size = source.metadata()?.len();
    source.sync_all()?;
    ctx.check()?;
    match link(staged, path) {
        Ok(()) => Ok(size),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(Error::new(format!("create output: link {} {}: {}", staged.display(), path.display(), io_message(&e))))
        }
        Err(_) => write_new(ctx, path, &mut source),
    }
}

/// Picks the destination for one input, numbering it when the policy allows.
pub fn available_output(path: &Path, policy: &str, reserved: &HashSet<PathBuf>) -> Result<PathBuf> {
    let base = abs_path(path)?;
    let ext = go_ext(base.as_os_str()).to_vec();
    let stem = &base.as_os_str().as_bytes()[..base.as_os_str().len() - ext.len()];
    for n in 0..10000 {
        let candidate = if n == 0 {
            base.clone()
        } else {
            let mut name = stem.to_vec();
            name.extend_from_slice(format!(".{n}").as_bytes());
            name.extend_from_slice(&ext);
            PathBuf::from(OsString::from_vec(name))
        };
        match fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !reserved.contains(&candidate) {
                    return Ok(candidate);
                }
            }
            Err(e) => return Err(path_error("lstat", &candidate, &e)),
            Ok(_) => {}
        }
        if policy != "number" {
            bail!("output already exists or has a batch name collision: {}", candidate.display());
        }
    }
    bail!("too many numbered output collisions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_path_helpers() {
        assert_eq!(go_ext(OsStr::new("a/b.PDF")), b".PDF");
        assert_eq!(go_ext(OsStr::new("a.d/b")), b"");
        assert_eq!(go_base(OsStr::new("a/b/")), OsString::from("b"));
        assert_eq!(clean(b"/a/./b/../c//d"), PathBuf::from("/a/c/d"));
        assert_eq!(clean(b"../x"), PathBuf::from("../x"));
        assert_eq!(clean(b"/.."), PathBuf::from("/"));
        assert_eq!(go_dir(Path::new("/a/b.pdf")), PathBuf::from("/a"));
        assert_eq!(go_dir(Path::new("b.pdf")), PathBuf::from("."));
    }

    #[test]
    fn write_new_failure_and_exclusive_creation() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("read failed"));
                }
                self.0 = true;
                buf[..7].copy_from_slice(b"partial");
                Ok(7)
            }
        }
        let dir = std::env::temp_dir().join(format!("pdf-squeezer-files-{}", random_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.pdf");
        let ctx = Ctx::background();
        let err = write_new(&ctx, &path, &mut Failing(false)).unwrap_err();
        assert!(err.to_string().contains("read failed"));
        assert!(!path.exists());
        fs::write(&path, b"keep").unwrap();
        assert!(write_new(&ctx, &path, &mut &b"replace"[..]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        let canceled = Ctx::background();
        canceled.cancel();
        let other = dir.join("other.pdf");
        assert!(write_new(&canceled, &other, &mut &b"data"[..]).unwrap_err().is_canceled());
        assert!(!other.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publish_links_or_copies_privately() {
        for linked in [true, false] {
            let dir = std::env::temp_dir().join(format!("pdf-squeezer-publish-{}", random_suffix()));
            fs::create_dir_all(&dir).unwrap();
            let (staged, output, other) = (dir.join("stage.pdf"), dir.join("out.pdf"), dir.join("other.pdf"));
            fs::write(&staged, b"staged output").unwrap();
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o400)).unwrap();
            let ctx = Ctx::background();
            let written = if linked {
                publish_file(&ctx, &staged, &output).unwrap()
            } else {
                publish_with(&ctx, &staged, &output, |_, _| Err(io::Error::from(io::ErrorKind::Unsupported))).unwrap()
            };
            assert_eq!(written, 13);
            use std::os::unix::fs::MetadataExt;
            let (a, b) = (fs::metadata(&staged).unwrap(), fs::metadata(&output).unwrap());
            assert_eq!(a.ino() == b.ino(), linked);
            assert_eq!(fs::read(&output).unwrap(), b"staged output");
            assert_eq!(b.permissions().mode() & 0o777, 0o600);
            fs::write(&other, b"replacement").unwrap();
            assert!(publish_file(&ctx, &other, &output).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"staged output");
            fs::remove_dir_all(&dir).unwrap();
        }
    }
}
