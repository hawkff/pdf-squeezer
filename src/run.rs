//! The batch: preflight every destination, prepare files in parallel, confirm
//! PDF/A in batches, and publish in input order.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use lopdf::Document;

use crate::bail;
use crate::document::{deduplicate_streams, document_dates, restore_document_dates, transform_document};
use crate::error::{Ctx, Error, Result, path_error};
use crate::extract::extract;
use crate::files::{Staging, available_output, check_pdf, go_base, go_dir, go_ext, open_input, publish_file, read_all, stage_path, write_stage};
use crate::images::optimize_images;
use crate::options::{Options, expand_inputs, list_contains, parse_options};
use crate::pdf::{self, WriteOptions, aes256, load_path, signed};
use crate::process::{Out, PrefixWriter, write_out};
use crate::tools::{declared_flavour, ghostscript, no_validator, pdfa_identification, python_pdf, qpdf, validate_pdfa};

pub const VALIDATION_BATCH: usize = 32;

/// The release name stamped at build time, or the package version.
pub fn version() -> &'static str {
    match option_env!("PDF_SQUEEZER_VERSION") {
        Some(stamp) if !stamp.is_empty() => stamp,
        _ => concat!("v", env!("CARGO_PKG_VERSION")),
    }
}

/// A compressed document that passed the structural check and waits for
/// publication, and for veraPDF when a PDF/A flavour has to be confirmed.
pub struct Staged {
    pub input: PathBuf,
    pub output: PathBuf,
    pub path: PathBuf,
    pub original: PathBuf,
    staging: Option<Staging>,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// The veraPDF flavour to confirm, "" for none.
    pub flavour: String,
    /// --pdfa produced it: a failed check is fatal.
    pub converted: bool,
}

impl Staged {
    pub fn cleanup(&self) {
        if let Some(staging) = &self.staging {
            staging.remove();
        }
    }

    pub fn for_check(path: PathBuf, input: PathBuf, original: PathBuf, flavour: &str, converted: bool) -> Staged {
        Staged { output: PathBuf::new(), path, input, original, staging: None, size: 0, modified: None, flavour: flavour.into(), converted }
    }
}

/// Runs veraPDF once per batch and flavour and keeps each verdict.
pub struct Verifier {
    stderr: Out,
    verdicts: HashMap<usize, Option<Error>>,
    /// Tool failures are not conformance verdicts.
    failed: HashMap<usize, Error>,
    warned: bool,
}

impl Verifier {
    pub fn new(stderr: Out) -> Self {
        Verifier { stderr, verdicts: HashMap::new(), failed: HashMap::new(), warned: false }
    }

    /// Whether veraPDF failed to judge a file, as opposed to rejecting it.
    pub fn tool_failed(&self, index: usize) -> bool {
        self.failed.contains_key(&index)
    }

    pub fn validate(&mut self, ctx: &Ctx, batch: &[Staged]) {
        self.verdicts.clear();
        self.failed.clear();
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, s) in batch.iter().enumerate() {
            if s.flavour.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|(f, _)| *f == s.flavour) {
                Some((_, members)) => members.push(i),
                None => groups.push((s.flavour.clone(), vec![i])),
            }
        }
        for (flavour, group) in groups {
            let paths: Vec<PathBuf> = group.iter().map(|&i| batch[i].path.clone()).collect();
            let result = validate_pdfa(ctx, &flavour, &paths, &self.stderr);
            match result {
                Err(e) if group.len() > 1 && ctx.err().is_none() && !no_validator(&e) => {
                    // Retry file-specific report failures, but not a missing validator.
                    for &i in &group {
                        let path = batch[i].path.clone();
                        match validate_pdfa(ctx, &flavour, std::slice::from_ref(&path), &self.stderr) {
                            Err(e) => {
                                self.failed.insert(i, e);
                            }
                            Ok(mut single) => {
                                self.verdicts.insert(i, single.remove(&path).flatten());
                            }
                        }
                    }
                }
                Err(e) => {
                    for &i in &group {
                        self.failed.insert(i, e.clone());
                    }
                }
                Ok(mut results) => {
                    for &i in &group {
                        self.verdicts.insert(i, results.remove(&batch[i].path).flatten());
                    }
                }
            }
        }
    }

    /// Decides whether a staged document may be published. Converted documents
    /// fail closed. Documents that only declared PDF/A on input are published with
    /// a warning when veraPDF is not installed or when the input did not conform
    /// either. A validator that runs but cannot return a verdict fails closed.
    pub fn check(&mut self, ctx: &Ctx, index: usize, s: &Staged) -> Result<()> {
        if s.flavour.is_empty() {
            return Ok(());
        }
        let label = format!("PDF/A-{}", s.flavour.to_uppercase());
        if let Some(err) = self.failed.get(&index) {
            if s.converted || !no_validator(err) {
                return Err(err.clone());
            }
            if !self.warned {
                write_out(&self.stderr, format!("warning: {err}; declared PDF/A conformance is not verified\n").as_bytes());
                self.warned = true;
            }
            return Ok(());
        }
        let Some(Some(verdict)) = self.verdicts.get(&index).cloned() else { return Ok(()) };
        if s.converted {
            return Err(verdict);
        }
        match validate_pdfa(ctx, &s.flavour, std::slice::from_ref(&s.original), &self.stderr) {
            // A rejected output is published only with proof that the input did not conform.
            Err(e) => Err(Error::join(vec![verdict, e]).unwrap()),
            Ok(inputs) => {
                if inputs.get(&s.original).is_some_and(Option::is_none) {
                    bail!("compression broke the input's {label} conformance; use --pdfa to repair it or report this:\n{verdict}");
                }
                write_out(
                    &self.stderr,
                    format!("warning: {} declares {label} but does not conform to it; the output is not verified\n", s.input.display()).as_bytes(),
                );
                Ok(())
            }
        }
    }
}

fn publish(ctx: &Ctx, s: &Staged, opts: &Options) -> Result<(u64, u64)> {
    let result = (|| {
        let written = publish_file(ctx, &s.path, &s.output)?;
        if opts.timestamps == "preserve"
            && let Some(modified) = s.modified
        {
            let times = fs::FileTimes::new().set_accessed(modified).set_modified(modified);
            let set = OpenOptions::new().read(true).open(&s.output).and_then(|f| f.set_times(times));
            if let Err(e) = set {
                let _ = fs::remove_file(&s.output);
                return Err(path_error("chtimes", &s.output, &e));
            }
        }
        Ok((s.size, written))
    })();
    s.cleanup();
    result
}

fn report(stdout: &Out, output: &Path, before: u64, after: u64, opts: &Options) {
    let line = if before == after && !opts.required_output() {
        let hint = if opts.engine == "ghostscript" {
            "try --quality screen or a lower --dpi"
        } else if !opts.images {
            "try --images or --engine ghostscript"
        } else {
            "try --engine ghostscript"
        };
        format!("{}: no reduction; copied the original ({before} bytes); {hint}\n", output.display())
    } else if after > before {
        format!("{}: {before} -> {after} bytes ({:.1}% larger, {})\n", output.display(), 100.0 * (after as f64 / before as f64 - 1.0), opts.engine)
    } else {
        format!("{}: {before} -> {after} bytes ({:.1}% smaller, {})\n", output.display(), 100.0 * (1.0 - after as f64 / before as f64), opts.engine)
    };
    write_out(stdout, line.as_bytes());
}

/// Go's Duration.String for a duration rounded to milliseconds.
pub fn duration_text(d: Duration) -> String {
    let ms = (d.as_micros() + 500) / 1000;
    if ms == 0 {
        return "0s".into();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut secs = s.to_string();
    if frac > 0 {
        secs.push_str(format!(".{frac:03}").trim_end_matches('0'));
    }
    if h > 0 {
        format!("{h}h{m}m{secs}s")
    } else if m > 0 {
        format!("{m}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

struct Slots {
    free: Mutex<usize>,
    ready: Condvar,
}

impl Slots {
    fn acquire(&self) {
        let mut free = self.free.lock().unwrap();
        while *free == 0 {
            free = self.ready.wait(free).unwrap();
        }
        *free -= 1;
    }

    fn release(&self) {
        *self.free.lock().unwrap() += 1;
        self.ready.notify_one();
    }
}

enum Outcome {
    Staged(Box<Staged>),
    Extracted,
}

/// Deeply nested objects recurse in the parser, so workers get a large stack.
const WORKER_STACK: usize = 256 << 20;

pub fn run(ctx: &Ctx, args: &[OsString], stdout: &Out, stderr: &Out) -> Result<()> {
    let parsed = parse_options(ctx, args, stderr)?;
    if parsed.version {
        write_out(stdout, format!("{}\n", version()).as_bytes());
        return Ok(());
    }
    ctx.check()?;
    let opts = parsed.opts;
    let inputs = parsed.inputs;
    if inputs.is_empty() {
        // --save-profile without a document.
        return Ok(());
    }
    let files = expand_inputs(ctx, &inputs, opts.recursive)?;
    if files.is_empty() {
        bail!("no input PDFs found");
    }
    if opts.outputs.len() > 1 {
        if opts.outputs.len() != files.len() || inputs.len() != files.len() {
            bail!("repeat --output exactly once per distinct input file");
        }
        for input in &inputs {
            let info = fs::metadata(input).map_err(|e| path_error("stat", Path::new(input), &e))?;
            if info.is_dir() {
                bail!("per-input outputs require files, not directory inputs");
            }
        }
    }
    let mut output_dir = false;
    if !opts.output.is_empty() {
        output_dir = fs::metadata(&opts.output).is_ok_and(|m| m.is_dir());
        if files.len() > 1 && !output_dir {
            bail!("batch --output must be an existing directory");
        }
    }
    if files.len() == 1 && opts.extract == "images" {
        output_dir = false;
    }
    // Resolve the complete batch before writing any output. Exclusive creation still
    // protects against another process creating a destination after this preflight.
    let mut outputs = Vec::with_capacity(files.len());
    let mut reserved = HashSet::new();
    for (i, file) in files.iter().enumerate() {
        let base_name = go_base(file.path.as_os_str());
        let ext_len = go_ext(&base_name).len();
        let mut name = base_name.as_bytes()[..base_name.len() - ext_len].to_vec();
        name.extend_from_slice(match opts.extract.as_str() {
            "images" => b".images".as_slice(),
            "text" => b".txt",
            _ => b".squeezed.pdf",
        });
        let name = PathBuf::from(OsString::from_vec(name));
        let mut output = PathBuf::from(&opts.output);
        if opts.outputs.len() > 1 {
            output = PathBuf::from(&opts.outputs[i]);
        }
        if output.as_os_str().is_empty() {
            output = go_dir(&file.path).join(&name);
        }
        if output_dir {
            output = PathBuf::from(&opts.output).join(go_dir(&file.relative)).join(&name);
        }
        let output = available_output(&output, &opts.collision, &reserved)?;
        reserved.insert(output.clone());
        outputs.push(output);
    }

    let mut failures: Vec<Error> = Vec::new();
    let mut pending: Vec<Staged> = Vec::new();
    let mut verifier = Verifier::new(stderr.clone());
    let flush = |pending: &mut Vec<Staged>, verifier: &mut Verifier, failures: &mut Vec<Error>| {
        for (i, s) in pending.drain(..).enumerate() {
            if ctx.err().is_some() {
                s.cleanup();
                continue;
            }
            if let Err(e) = verifier.check(ctx, i, &s) {
                failures.push(e.context(s.input.display()));
                s.cleanup();
                continue;
            }
            match publish(ctx, &s, &opts) {
                Ok((before, after)) => report(stdout, &s.output, before, after, &opts),
                Err(e) => failures.push(e.context(s.input.display())),
            }
        }
    };
    let slots = Slots { free: Mutex::new(opts.jobs as usize), ready: Condvar::new() };
    let process = |i: usize| -> Result<Outcome> {
        ctx.check()?;
        if output_dir && let Some(parent) = outputs[i].parent() {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| path_error("mkdir", parent, &e))?;
        }
        let mut prefixed = None;
        let out: Out = if opts.jobs > 1 && files.len() > 1 {
            // Parallel files share stderr, so every line names its file.
            let mut prefix = files[i].path.as_os_str().as_bytes().to_vec();
            prefix.extend_from_slice(b": ");
            let writer = Arc::new(Mutex::new(PrefixWriter::new(stderr.clone(), prefix)));
            prefixed = Some(writer.clone());
            writer
        } else {
            stderr.clone()
        };
        let result = if !opts.extract.is_empty() {
            extract(ctx, &files[i].path, &outputs[i], &opts, &out).map(|()| Outcome::Extracted)
        } else {
            prepare(ctx, &files[i].path, &outputs[i], &opts, &out).map(|s| Outcome::Staged(Box::new(s)))
        };
        if let Some(writer) = prefixed {
            let _ = writer.lock().unwrap().close();
        }
        result
    };
    thread::scope(|scope| {
        let mut receivers = Vec::with_capacity(files.len());
        let mut senders = Vec::with_capacity(files.len());
        for _ in 0..files.len() {
            let (tx, rx) = mpsc::channel::<Result<Outcome>>();
            senders.push(tx);
            receivers.push(rx);
        }
        let (slots, process) = (&slots, &process);
        // Up to --jobs files are prepared at once; results are handled in input order.
        // A slot frees only once its result is handled, so work never runs further ahead.
        scope.spawn(move || {
            for (i, tx) in senders.into_iter().enumerate() {
                slots.acquire();
                let spawned = thread::Builder::new().stack_size(WORKER_STACK).spawn_scoped(scope, move || {
                    let _ = tx.send(process(i));
                });
                // A worker that cannot start drops its sender, and its file fails.
                if let Err(e) = spawned {
                    eprintln!("pdf-squeezer: cannot start a worker: {e}");
                }
            }
        });
        for (i, rx) in receivers.into_iter().enumerate() {
            let outcome = rx.recv().unwrap_or_else(|_| Err(Error::new("worker stopped without a result")));
            match outcome {
                Err(e) => {
                    let canceled = ctx.err().is_some() && e.is_canceled();
                    if !canceled {
                        failures.push(e.context(files[i].path.display()));
                    }
                }
                Ok(Outcome::Extracted) => write_out(stdout, format!("{}\n", outputs[i].display()).as_bytes()),
                Ok(Outcome::Staged(s)) => {
                    // veraPDF starts a JVM per invocation, so files that need it wait for a batch.
                    // After cancellation, flush removes staged files instead of publishing them.
                    let needs_check = !s.flavour.is_empty();
                    pending.push(*s);
                    if !needs_check || pending.len() >= VALIDATION_BATCH {
                        verifier.validate(ctx, &pending);
                        flush(&mut pending, &mut verifier, &mut failures);
                    }
                }
            }
            slots.release();
        }
    });
    verifier.validate(ctx, &pending);
    flush(&mut pending, &mut verifier, &mut failures);
    if let Some(e) = ctx.err() {
        failures.push(e);
    }
    match Error::join(failures) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Compresses and transforms input into a staged file that passed the structural
/// check. The caller confirms PDF/A with veraPDF when needed and publishes.
pub fn prepare(ctx: &Ctx, input: &Path, output: &Path, opts: &Options, stderr: &Out) -> Result<Staged> {
    ctx.check()?;
    let (mut file, info) = open_input(input)?;
    check_pdf(&file).map_err(|e| e.context("input"))?;
    match fs::symlink_metadata(output) {
        Ok(_) => bail!("output already exists: {}", output.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(path_error("lstat", output, &e)),
    }
    let staging = Staging::create(output)?;
    let mut staged = Staged {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        path: PathBuf::new(),
        original: PathBuf::new(),
        staging: None,
        size: info.len(),
        modified: info.modified().ok(),
        flavour: String::new(),
        converted: false,
    };
    match prepare_in(ctx, &mut file, info.len(), &staging.path, opts, stderr, &mut staged) {
        Ok(()) => {
            staged.staging = Some(staging);
            Ok(staged)
        }
        Err(e) => {
            staging.remove();
            Err(e)
        }
    }
}

fn prepare_in(ctx: &Ctx, file: &mut fs::File, input_size: u64, work: &Path, opts: &Options, stderr: &Out, staged: &mut Staged) -> Result<()> {
    let logf = |message: String| {
        if opts.verbose {
            write_out(stderr, format!("{message}\n").as_bytes());
        }
    };
    let mut opts = opts.clone();
    logf(format!("input: {} ({input_size} bytes)", staged.input.display()));
    logf(format!("staging in {}", work.display()));
    let data = read_all(ctx, file)?;
    let loaded = pdf::load(&data, &opts.password).map_err(|e| e.context("input"))?;
    let original_dates = document_dates(&loaded);
    let encrypted = loaded.encryption_state.is_some();
    if encrypted && !opts.pdfa.is_empty() && !opts.decrypt {
        bail!("PDF/A cannot preserve encryption; explicitly use --decrypt");
    }
    let mut declared = pdfa_identification(ctx, &loaded).unwrap_or_else(|e| {
        write_out(stderr, format!("warning: {e}; declared conformance is not preserved\n").as_bytes());
        String::new()
    });
    if !declared.is_empty() && opts.pdfa.is_empty() {
        let label = format!("PDF/A-{}", declared.to_uppercase());
        let mut breaking = opts.breaks_pdfa(false);
        if breaking.is_empty() && declared.ends_with('a') && list_contains(&opts.strip, "tags") {
            breaking = "--strip tags".into();
        }
        if !breaking.is_empty() {
            write_out(
                stderr,
                format!("warning: the input declares {label} and {breaking} removes what it requires; the output will not conform\n").as_bytes(),
            );
            declared.clear();
        } else {
            logf(format!("input declares {label}; preserving its structure and verifying the output"));
        }
    }
    // PDF/A-1 is a PDF 1.4 profile: cross-reference and object streams are not allowed.
    opts.classic_xref = opts.pdfa.is_empty() && declared.starts_with('1');
    if encrypted && opts.privacy && !opts.decrypt && opts.encrypt_owner_file.is_empty() {
        bail!("--privacy on encrypted input requires --decrypt or new output encryption to replace its identifier");
    }
    if signed(&loaded) {
        write_out(stderr, b"warning: rewriting this signed PDF invalidates its digital signatures\n");
    }
    let start = Instant::now();
    // Every tool works on a private snapshot of the opened input, never its path.
    let original = stage_path(work)?;
    write_stage(ctx, &original, &data)?;
    drop(data);
    staged.original = original.clone();
    let mut doc: Option<Document> = Some(loaded);
    let mut current = original.clone();
    if encrypted {
        logf("decrypting working copy".into());
        current = stage_path(work)?;
        qpdf(ctx, &["--decrypt".into(), original.clone().into_os_string(), current.clone().into_os_string()], &opts.password_bytes, stderr)?;
        doc = None;
    }
    // A finished stage goes once the next one exists. The input snapshot stays for
    // encryption, the size fallback, and conformance checks.
    let advance = |current: &mut PathBuf, label: &str, process: &mut dyn FnMut(&Path, &Path) -> Result<()>| -> Result<()> {
        ctx.check()?;
        let next = stage_path(work)?;
        logf(label.to_string());
        process(current, &next).map_err(|e| e.context(label))?;
        if *current != original {
            let _ = fs::remove_file(&*current);
        }
        *current = next;
        Ok(())
    };
    let stage_opts = opts.clone();
    // PDF/A-4 constrains inline images (length key, filters); XObjects avoid that.
    if opts.inline || !opts.pdfa.is_empty() || list_contains(&opts.strip, "images") {
        advance(&mut current, "externalizing inline images", &mut |src, dst| {
            qpdf(ctx, &["--externalize-inline-images".into(), "--ii-min-bytes=0".into(), src.into(), dst.into()], b"", stderr)
        })?;
        doc = None;
    }
    if opts.engine == "ghostscript" || opts.cff_fonts {
        advance(&mut current, "rewriting with Ghostscript", &mut |src, dst| {
            let out = OpenOptions::new().write(true).truncate(true).open(dst).map_err(|e| path_error("open", dst, &e))?;
            ghostscript(ctx, src, out, &stage_opts, stderr)
        })?;
        doc = None;
    }
    let native = opts.engine == "native";
    if opts.advanced_images()
        || !opts.flatten.is_empty()
        || opts.merge_fonts
        || opts.subset_fonts
        || opts.bitmap
        || opts.mrc
        || opts.python_strip()
        || opts.gray && native
    {
        advance(&mut current, "processing advanced document features", &mut |src, dst| python_pdf(ctx, "transform", src, dst, &stage_opts, stderr))?;
        doc = None;
    }
    let mut doc = match doc {
        Some(doc) => doc,
        None => load_path(ctx, &current, "").map_err(|e| e.context("transformed input"))?,
    };
    // Optimize structure first to avoid re-encoding duplicate image objects.
    deduplicate_streams(ctx, &mut doc, false, opts.image_memory)?;
    if opts.images {
        let stats = optimize_images(ctx, &mut doc, &opts, &logf)?;
        logf(stats.to_string());
    }
    if opts.timestamps != "now" && !opts.privacy {
        restore_document_dates(&mut doc, &original_dates);
    }
    transform_document(ctx, &mut doc, &opts)?;
    deduplicate_streams(ctx, &mut doc, true, opts.image_memory)?;
    let next = stage_path(work)?;
    let layout = WriteOptions { object_streams: !opts.classic_xref, encryption: None, new_id: false };
    pdf::write(ctx, &doc, &next, &layout)?;
    drop(doc);
    if current != original {
        let _ = fs::remove_file(&current);
    }
    current = next;
    if opts.mono_codecs != "flate" {
        advance(&mut current, "encoding monochrome images", &mut |src, dst| python_pdf(ctx, "monochrome", src, dst, &stage_opts, stderr))?;
        advance(&mut current, "deduplicating final streams", &mut |src, dst| {
            let mut last = load_path(ctx, src, "")?;
            deduplicate_streams(ctx, &mut last, true, stage_opts.image_memory)?;
            let layout = WriteOptions { object_streams: !stage_opts.classic_xref, encryption: None, new_id: false };
            pdf::write(ctx, &last, dst, &layout)
        })?;
    }
    let (mut flavour, mut converted) = (declared, false);
    if !opts.pdfa.is_empty() {
        let label = format!("converting to PDF/A-{}", opts.pdfa.to_uppercase());
        advance(&mut current, &label, &mut |src, dst| python_pdf(ctx, "pdfa", src, dst, &stage_opts, stderr))?;
        flavour = declared_flavour(ctx, &current)?;
        if flavour.is_empty() {
            bail!("converted document does not declare PDF/A");
        }
        converted = true;
    }
    let mut output_password = String::new();
    if !opts.encrypt_owner_file.is_empty() {
        advance(&mut current, "encrypting output", &mut |src, dst| {
            let plain = load_path(ctx, src, "")?;
            let state = aes256(&stage_opts.encrypt_user, &stage_opts.encrypt_owner, &stage_opts.permissions)?;
            pdf::write(ctx, &plain, dst, &WriteOptions { object_streams: true, encryption: Some(state), new_id: false })
        })?;
        output_password = opts.encrypt_owner.clone();
    } else if encrypted && !opts.decrypt {
        let mut copy = OsString::from("--copy-encryption=");
        copy.push(&original);
        let password = OsString::from_vec([b"--encryption-file-password=".as_slice(), &opts.password_bytes].concat());
        advance(&mut current, "preserving encryption", &mut |src, dst| {
            qpdf(ctx, &[copy.clone(), password.clone(), src.into(), dst.into()], b"", stderr)
        })?;
        output_password = opts.password.clone();
    }
    let size = fs::metadata(&current).map_err(|e| path_error("stat", &current, &e))?.len();
    load_path(ctx, &current, &output_password).map_err(|e| e.context("output validation"))?;
    logf(format!("{}: {size} bytes in {}; validated", opts.engine, duration_text(start.elapsed())));
    if size >= input_size && !opts.required_output() {
        current = original.clone();
    }
    // Keep the original snapshot only when a failed conformance verdict may need it.
    for entry in fs::read_dir(work)? {
        let path = entry?.path();
        if path != current && !(path == original && !flavour.is_empty() && !converted) {
            let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
        }
    }
    staged.path = current;
    staged.flavour = flavour;
    staged.converted = converted;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_durations() {
        assert_eq!(duration_text(Duration::from_millis(0)), "0s");
        assert_eq!(duration_text(Duration::from_millis(567)), "567ms");
        assert_eq!(duration_text(Duration::from_millis(1500)), "1.5s");
        assert_eq!(duration_text(Duration::from_millis(39522)), "39.522s");
        assert_eq!(duration_text(Duration::from_millis(61000)), "1m1s");
        assert_eq!(duration_text(Duration::from_millis(3_723_004)), "1h2m3.004s");
    }
}
