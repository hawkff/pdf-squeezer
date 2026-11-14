//! External tools: qpdf, the bundled Python helpers, veraPDF and Ghostscript.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use lopdf::{Document, Object};
use serde_json::{Value, json};

use crate::bail;
use crate::error::{Ctx, Error, Result, path_error};
use crate::files::{abs_path, go_dir, mkdir_temp};
use crate::filters;
use crate::options::{Options, list_contains};
use crate::pdf::{catalog, get, load_path};
use crate::process::{self, BoundedBuffer, Failure, Out, Sink, Tool, look_path, shared, write_out};

const PDF_TOOLS: &[u8] = include_bytes!("../tools/pdf_tools.py");
const PDFA_TOOLS: &[u8] = include_bytes!("../tools/pdfa.py");
const SRGB_PROFILE: &[u8] = include_bytes!("../tools/sRGB2014.icc");

pub fn python_command() -> OsString {
    match std::env::var_os("PDF_SQUEEZER_PYTHON") {
        Some(p) if !p.is_empty() => p,
        _ => OsString::from("python3"),
    }
}

/// Prints a tool's diagnostics with secrets redacted, then reports the failure.
/// Cancellation wins over the tool's own error.
pub fn tool_error(ctx: &Ctx, name: &str, failure: Option<Failure>, output: &BoundedBuffer, stderr: &Out, secrets: &[&str]) -> Result<()> {
    let mut text = output.text();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[redacted]");
        if output.truncated {
            for n in (1..secret.len().min(text.len() + 1)).rev() {
                if secret.is_char_boundary(n) && text.ends_with(&secret[..n]) {
                    text.truncate(text.len() - n);
                    text.push_str("[redacted]");
                    break;
                }
            }
        }
    }
    if !text.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        write_out(stderr, text.as_bytes());
    }
    ctx.check()?;
    match failure {
        Some(f) => bail!("{name}: {f}"),
        None => Ok(()),
    }
}

pub fn qpdf(ctx: &Ctx, args: &[OsString], password: &[u8], stderr: &Out) -> Result<()> {
    let Some(path) = look_path("qpdf".as_ref()) else {
        bail!("qpdf is required for inline images and encrypted input; install it and add it to PATH");
    };
    // qpdf's @- reads one argument per line. Passwords never enter argv or logs.
    let (mut private, mut public, mut secrets): (Vec<Vec<u8>>, Vec<OsString>, Vec<String>) = (Vec::new(), Vec::new(), Vec::new());
    if !password.is_empty() {
        private.push([b"--password=".as_slice(), password].concat());
        secrets.push(String::from_utf8_lossy(password).into_owned());
    }
    for arg in args {
        let bytes = arg.as_bytes();
        if let Some(secret) = bytes.strip_prefix(b"--encryption-file-password=") {
            if secret.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                bail!("invalid encryption password");
            }
            private.push(bytes.to_vec());
            secrets.push(String::from_utf8_lossy(secret).into_owned());
        } else {
            public.push(arg.clone());
        }
    }
    if !private.is_empty() {
        public.insert(0, OsString::from("@-"));
    }
    let messages = shared();
    let mut stdin = private.join(&b'\n');
    stdin.push(b'\n');
    let tool = Tool { stdin: Some(stdin), stdout: Sink::Buffer(messages.clone()), combined: true, ..Tool::new(path).args(public) };
    let result = process::run(ctx, tool);
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    // A qpdf warning is a failure: requested transformations must not silently lose data.
    tool_error(ctx, "qpdf", result.err(), &messages.lock().unwrap(), stderr, &secrets)
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn python_pdf(ctx: &Ctx, operation: &str, input: &Path, output: &Path, opts: &Options, stderr: &Out) -> Result<()> {
    let Some(python) = look_path(&python_command()) else {
        bail!("Python 3 is required for this operation; set PDF_SQUEEZER_PYTHON to a Python environment with the optional PDF tools");
    };
    let parent = go_dir(output);
    let dir = mkdir_temp(&parent, "pdf-tools-").map_err(|e| path_error("mkdirtemp", &parent, &e))?;
    let result = (|| {
        for (name, data) in [("pdf_tools.py", PDF_TOOLS), ("pdfa.py", PDFA_TOOLS), ("sRGB2014.icc", SRGB_PROFILE)] {
            let path = dir.join(name);
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| path_error("open", &path, &e))?;
            std::io::Write::write_all(&mut file, data)?;
        }
        let font_files: BTreeMap<String, String> = opts
            .font_files
            .iter()
            .map(|setting| {
                let text = setting.to_string_lossy();
                let (name, file) = text.split_once('=').unwrap_or((&text, ""));
                (name.to_string(), file.to_string())
            })
            .collect();
        let native = opts.engine == "native";
        let mut request = json!({
            "dpi": opts.dpi, "gray_dpi": opts.gray_dpi, "mono_dpi": opts.mono_dpi, "threshold": opts.dpi_threshold,
            "quality": opts.image_quality, "clip": opts.clip, "extended": opts.extended, "lossless": opts.lossless,
            "reduce_bits": opts.reduce_bits, "force": opts.force, "mono_codecs": opts.mono_codecs,
            "codecs": opts.image_codecs, "memory_mib": opts.image_memory,
            "gray": opts.gray && native, "geometry": native,
            "flatten": opts.flatten, "subset_fonts": opts.subset_fonts, "merge_fonts": opts.merge_fonts,
            "bitmap": opts.bitmap, "mrc": opts.mrc, "render_dpi": opts.render_dpi, "background_dpi": opts.background_dpi,
            "verbose": opts.verbose, "strip": opts.strip, "font_files": font_files,
            "output_intent": opts.output_intent.to_string_lossy(),
            "srgb_icc": lossy(&dir.join("sRGB2014.icc")), "pdfa": opts.pdfa, "object_streams": !opts.classic_xref,
        });
        if operation == "text" {
            request["password"] = Value::String(opts.password.clone());
        }
        let messages = shared();
        let tool = Tool {
            stdin: Some(serde_json::to_vec(&request).map_err(|e| Error::new(e.to_string()))?),
            stdout: Sink::Buffer(messages.clone()),
            combined: true,
            ..Tool::new(python).arg("-I").arg(dir.join("pdf_tools.py")).arg(operation).arg(input).arg(output).private_temp(&dir)
        };
        let result = process::run(ctx, tool);
        tool_error(ctx, "PDF tools", result.err(), &messages.lock().unwrap(), stderr, &[&opts.password])
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}

/// The PDF/A part and conformance a document declares in its XMP, as veraPDF
/// flavour names: "2b", "4", "4f". Undeclared documents give "".
pub fn pdfa_identification(ctx: &Ctx, doc: &Document) -> Result<String> {
    let Some(root) = catalog(doc) else { return Ok(String::new()) };
    let Some(Object::Stream(metadata)) = get(doc, root, b"Metadata") else { return Ok(String::new()) };
    let pipeline = filters::pipeline(doc, &metadata.dict).map_err(Error::new)?;
    let xmp = filters::decode(ctx, &metadata.content, &pipeline, 64 << 20).map_err(|e| Error::new(e.to_string()))?;
    let part = xmp_property(&xmp, "part");
    let conformance = xmp_property(&xmp, "conformance").to_lowercase();
    if part.is_empty() {
        return Ok(String::new());
    }
    if part == "4" && list_contains(",e,f", &conformance) {
        return Ok(part + &conformance);
    }
    if list_contains("1,2,3", &part) && list_contains("a,b,u", &conformance) {
        return Ok(part + &conformance);
    }
    bail!("unsupported PDF/A identification part {part:?} conformance {conformance:?}")
}

const PDFAID: &str = "http://www.aiim.org/pdfa/ns/id/";

/// Finds a PDF/A identification property by namespace and local name, written as
/// an attribute or as an element, with any prefix.
/// The scan reads tokens in order and stops at the first match or the first error,
/// so damage or unbound prefixes after the identification do not hide it.
pub fn xmp_property(xmp: &[u8], name: &str) -> String {
    use quick_xml::XmlVersion;
    use quick_xml::events::Event;
    use quick_xml::name::{Namespace, ResolveResult};
    let Ok(text) = std::str::from_utf8(xmp) else { return String::new() };
    let mut reader = quick_xml::NsReader::from_str(text);
    let pdfaid = |ns: &ResolveResult| matches!(ns, ResolveResult::Bound(Namespace(uri)) if *uri == PDFAID);
    loop {
        let (element, event) = match reader.read_resolved_event() {
            Ok((ns, event)) => (pdfaid(&ns), event),
            Err(_) => return String::new(),
        };
        let (start, empty) = match event {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::Eof => return String::new(),
            _ => continue,
        };
        for attr in start.attributes() {
            let Ok(attr) = attr else { return String::new() };
            let (ns, local) = reader.resolver().resolve_attribute(attr.key);
            if pdfaid(&ns) && local.as_ref() == name {
                return attr.normalized_value(XmlVersion::Implicit1_0).map(|v| v.trim().to_string()).unwrap_or_default();
            }
        }
        if element && start.local_name().as_ref() == name {
            if empty {
                return String::new();
            }
            let (mut value, mut depth) = (String::new(), 0usize);
            loop {
                match reader.read_event() {
                    Ok(Event::Start(_)) => depth += 1,
                    Ok(Event::End(_)) if depth == 0 => return value.trim().to_string(),
                    Ok(Event::End(_)) => depth -= 1,
                    Ok(Event::Text(text)) => value.push_str(&text.xml10_content()),
                    Ok(Event::CData(data)) => value.push_str(&data),
                    Ok(Event::GeneralRef(reference)) => match reference.resolve_char_ref() {
                        Ok(Some(c)) => value.push(c),
                        _ => value.push_str(quick_xml::escape::resolve_predefined_entity(&reference).unwrap_or_default()),
                    },
                    Ok(Event::Eof) | Err(_) => return String::new(),
                    Ok(_) => {}
                }
            }
        }
    }
}

pub fn declared_flavour(ctx: &Ctx, path: &Path) -> Result<String> {
    let doc = load_path(ctx, path, "")?;
    pdfa_identification(ctx, &doc)
}

pub const NO_VALIDATOR: &str = "PDF/A validation requires veraPDF on PATH";

pub fn no_validator(e: &Error) -> bool {
    e.to_string() == NO_VALIDATOR
}

/// veraPDF clause families, longest prefix first. Hints name the option that resolves them.
const PDFA_CATEGORIES: &[(&str, &str, &str)] = &[
    ("6.2.10", "fonts", "embed the font in the source document or pass --font-file NAME=PATH"),
    ("6.2.11", "fonts", "embed the font in the source document or pass --font-file NAME=PATH"),
    ("6.2.3", "color", "pass --output-intent with a valid ICC output or display profile"),
    ("6.2.4", "color", "pass --output-intent with an ICC profile for the color spaces the document uses"),
    ("6.2.9", "color", "pass --output-intent with an ICC profile for the color spaces the document uses"),
    ("6.2", "graphics", "the converter should repair this; report it with the object numbers"),
    ("6.3", "annotations", "use --flatten annotations, or --strip annotations, hidden, or multimedia"),
    ("6.4", "forms", "use --strip xfa or --strip actions"),
    ("6.5", "actions", "use --strip actions"),
    ("6.6", "actions or metadata", "use --strip actions; metadata should be repaired by the converter, report it"),
    ("6.7", "metadata", "the converter should repair this; report it with the object numbers"),
    ("6.8", "attachments", "use --strip attachments, or --pdfa 3b for arbitrary attachments"),
    ("6.9", "attachments", "use --strip attachments, or embed only PDF/A attachments"),
    ("6.1", "structure", "the converter should repair this; report it with the object numbers"),
    ("6", "structure", "the converter should repair this; report it with the object numbers"),
];

/// Keeps the innermost object number and the trailing label, e.g. "object 14 Helvetica".
fn check_objects(context: &str) -> String {
    let (mut object, mut label) = (String::new(), String::new());
    let bytes = context.as_bytes();
    let mut i = 0;
    while let Some(open) = bytes[i..].iter().position(|&b| b == b'(').map(|p| p + i) {
        let rest = &bytes[open + 1..];
        let Some(close) = rest.iter().position(|&b| b == b')' || b == b'(') else { break };
        if rest[close] == b'(' {
            i = open + 1 + close;
            continue;
        }
        let inner = &context[open + 1..open + 1 + close];
        let parts: Vec<&str> = inner.split(' ').collect();
        let numbered = parts.len() == 4
            && parts[0].bytes().all(|b| b.is_ascii_digit())
            && !parts[0].is_empty()
            && parts[1].bytes().all(|b| b.is_ascii_digit())
            && !parts[1].is_empty()
            && parts[2] == "obj"
            && !parts[3].is_empty()
            && parts[3].bytes().all(|b| b.is_ascii_alphabetic());
        let end = open + 1 + close + 1;
        if numbered {
            object = format!("object {}", parts[0]);
        } else if end == bytes.len() && !inner.is_empty() {
            label = inner.to_string();
        }
        i = end;
    }
    format!("{object} {label}").trim().to_string()
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    node.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text_of(node: roxmltree::Node) -> String {
    node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect()
}

/// Runs veraPDF once over every path with one flavour and returns a verdict per
/// path: None for compliant files, diagnostics grouped by what resolves them
/// otherwise. An error means veraPDF could not run or its report was unreadable.
pub fn validate_pdfa(ctx: &Ctx, flavour: &str, paths: &[PathBuf], stderr: &Out) -> Result<HashMap<PathBuf, Option<Error>>> {
    let Some(binary) = look_path("verapdf".as_ref()) else { return Err(Error::new(NO_VALIDATOR)) };
    let absolute: Vec<PathBuf> = paths.iter().map(|p| abs_path(p)).collect::<Result<_>>()?;
    let (report, messages) = (shared(), shared());
    let tool = Tool {
        stdout: Sink::Buffer(report.clone()),
        stderr: Sink::Buffer(messages.clone()),
        ..Tool::new(binary).args(["--format", "xml", "--flavour", flavour, "--maxfailuresdisplayed", "3"]).args(&absolute)
    };
    // veraPDF exits nonzero for noncompliant files; the report decides.
    let run = process::run(ctx, tool).err();
    let mut messages = messages.lock().unwrap();
    // JVM banners such as "Picked up JAVA_TOOL_OPTIONS" are not diagnostics.
    let kept: Vec<u8> =
        messages.bytes().split_inclusive(|&b| b == b'\n').filter(|line| !line.starts_with(b"Picked up ")).flatten().copied().collect();
    messages.set_bytes(kept);
    let report = report.lock().unwrap();
    if report.bytes().is_empty() {
        tool_error(ctx, "veraPDF", run, &messages, stderr, &[])?;
        bail!("veraPDF produced no report");
    }
    tool_error(ctx, "veraPDF", None, &messages, stderr, &[])?;
    if report.truncated {
        bail!("veraPDF report exceeded its size limit");
    }
    let text = String::from_utf8_lossy(report.bytes()).into_owned();
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let tree = roxmltree::Document::parse_with_options(&text, options).map_err(|e| Error::new(format!("veraPDF report: {e}")))?;
    let want = format!("PDF/A-{}", flavour.to_uppercase());
    let mut results: HashMap<PathBuf, Option<Error>> = HashMap::new();
    for jobs in child(tree.root_element(), "jobs") {
        for job in child(jobs, "job") {
            let name: String = child(job, "item").flat_map(|item| child(item, "name")).map(text_of).next().unwrap_or_default();
            let Some(validation) = child(job, "validationReport").next() else { continue };
            let profile = validation.attribute("profileName").unwrap_or("");
            let compliant = validation.attribute("isCompliant") == Some("true");
            if compliant && profile.to_uppercase().contains(&want) {
                results.insert(PathBuf::from(&name), None);
                continue;
            }
            let mut lines = vec![format!("veraPDF did not confirm {want} compliance; output was not published")];
            for rule in child(validation, "details").flat_map(|d| child(d, "rule")) {
                if rule.attribute("status") != Some("failed") {
                    continue;
                }
                let clause = rule.attribute("clause").unwrap_or("");
                let test = rule.attribute("testNumber").unwrap_or("");
                let (mut category, mut hint) = ("structure", PDFA_CATEGORIES[PDFA_CATEGORIES.len() - 1].2);
                for (prefix, c, h) in PDFA_CATEGORIES {
                    if clause == *prefix || clause.starts_with(&format!("{prefix}.")) {
                        (category, hint) = (c, h);
                        break;
                    }
                }
                let objects: Vec<String> = child(rule, "check")
                    .map(|check| check_objects(&child(check, "context").map(text_of).next().unwrap_or_default()))
                    .filter(|s| !s.is_empty())
                    .collect();
                let raw: String = child(rule, "description").map(text_of).next().unwrap_or_default();
                let mut description = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                if description.len() > 160 {
                    let mut cut = 157;
                    while !description.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    description.truncate(cut);
                    description.push_str("...");
                }
                let mut line = format!("  {category} {clause}-{test}: {description}");
                if !objects.is_empty() {
                    line.push_str(&format!(" [{}]", objects.join("; ")));
                }
                lines.push(line);
                lines.push(format!("    {hint}"));
            }
            results.insert(PathBuf::from(&name), Some(Error::new(lines.join("\n"))));
        }
    }
    let mut verdicts = HashMap::new();
    for (path, abs) in paths.iter().zip(&absolute) {
        let verdict = match results.get(abs) {
            Some(v) => v.clone(),
            None => Some(Error::new("veraPDF did not report on this file")),
        };
        verdicts.insert(path.clone(), verdict);
    }
    Ok(verdicts)
}

pub fn ghostscript(ctx: &Ctx, input: &Path, output: File, opts: &Options, stderr: &Out) -> Result<()> {
    let Some(executable) = look_path("gs".as_ref()) else {
        bail!("Ghostscript not found in PATH; install it or use --engine native");
    };
    // With the native engine, grayscale and DPI belong to the native and Python passes.
    let colors =
        if opts.gray && opts.engine == "ghostscript" { "-sColorConversionStrategy=Gray" } else { "-sColorConversionStrategy=LeaveColorUnchanged" };
    let mut args: Vec<String> = ["-dSAFER", "-dBATCH", "-dNOPAUSE", "-dPDFSTOPONERROR", "-sDEVICE=pdfwrite"].map(String::from).to_vec();
    if opts.engine == "ghostscript" {
        args.push(format!("-dPDFSETTINGS=/{}", opts.quality));
    }
    args.push(colors.into());
    if opts.cff_fonts {
        // Font conversion must not also downsample or recompress page images.
        args.extend(["-dCompressFonts=true", "-dSubsetFonts=true", "-dEmbedAllFonts=true", "-dConvertFontsToCFF=true"].map(String::from));
        if opts.engine != "ghostscript" {
            // pdfwrite auto-filters images to DCT by default; keep lossless data lossless.
            args.extend(
                [
                    "-dDownsampleColorImages=false",
                    "-dDownsampleGrayImages=false",
                    "-dDownsampleMonoImages=false",
                    "-dAutoFilterColorImages=false",
                    "-dAutoFilterGrayImages=false",
                    "-dColorImageFilter=/FlateEncode",
                    "-dGrayImageFilter=/FlateEncode",
                    "-dPassThroughJPEGImages=true",
                    "-dPassThroughJPXImages=true",
                ]
                .map(String::from),
            );
        }
    }
    if opts.engine == "ghostscript" {
        let gray = if opts.gray_dpi > 0 { opts.gray_dpi } else { opts.dpi.max(opts.gray_dpi) };
        for (kind, dpi) in [("Color", opts.dpi), ("Gray", gray), ("Mono", opts.mono_dpi)] {
            if dpi > 0 {
                let threshold = format!("{}", opts.dpi_threshold);
                args.push(format!("-dDownsample{kind}Images=true"));
                args.push(format!("-d{kind}ImageResolution={dpi}"));
                args.push(format!("-d{kind}ImageDownsampleThreshold={threshold}"));
            }
        }
    }
    args.extend(["-sOutputFile=-", "-sstdout=%stderr"].map(String::from));
    if opts.cff_fonts {
        args.extend(["-c", "<< /NeverEmbed [] >> setdistillerparams"].map(String::from));
    }
    let messages = shared();
    let stderr_sink = if opts.verbose {
        write_out(stderr, b"running Ghostscript\n");
        Sink::Tee(messages.clone(), stderr.clone())
    } else {
        Sink::Buffer(messages.clone())
    };
    // Ghostscript spools piped PDF input to a temporary copy; a staged path is read in place.
    let tool = Tool {
        stdout: Sink::File(output),
        stderr: stderr_sink,
        ..Tool::new(executable).args(&args).arg("-f").arg(input).private_temp(&go_dir(input)).env("GS_OPTIONS", "")
    };
    if let Err(failure) = process::run(ctx, tool) {
        ctx.check()?;
        let messages = messages.lock().unwrap();
        if !opts.verbose {
            write_out(stderr, messages.bytes());
        }
        return Err(Error::new(failure.to_string()));
    }
    if messages.lock().unwrap().image_error {
        bail!("Ghostscript skipped an image it could not decode; try --engine native");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verapdf_context_objects() {
        let context = "root/document[0]/pages[0](3 0 obj PDPage)/contentStream[0](4 0 obj PDContentStream)/operators[3]/font[0](Helvetica)";
        assert_eq!(check_objects(context), "object 4 Helvetica");
        assert_eq!(check_objects("root/document[0](1 0 obj PDDocument)"), "object 1");
        assert_eq!(check_objects("root"), "");
    }

    #[test]
    fn xmp_identification_forms() {
        let attr = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/" pdfaid:part="2" pdfaid:conformance="B"/></rdf:RDF></x:xmpmeta>"#;
        assert_eq!(xmp_property(attr, "part"), "2");
        assert_eq!(xmp_property(attr, "conformance"), "B");
        // An unbound prefix after the identification must not hide it.
        let unbound = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/" pdfaid:part="1" pdfaid:conformance="B"/><rdf:Description><xmp:CreateDate>2020</xmp:CreateDate></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        assert_eq!(xmp_property(unbound, "part"), "1");
        assert_eq!(xmp_property(unbound, "conformance"), "B");
        let element = br#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:id="http://www.aiim.org/pdfa/ns/id/"><id:part> 4 </id:part></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;
        assert_eq!(xmp_property(element, "part"), "4");
        let cdata = br#"<r xmlns:id="http://www.aiim.org/pdfa/ns/id/"><id:part><![CDATA[2]]></id:part><id:conformance>&#66;</id:conformance></r>"#;
        assert_eq!(xmp_property(cdata, "part"), "2");
        assert_eq!(xmp_property(cdata, "conformance"), "B");
        assert_eq!(xmp_property(b"not xml", "part"), "");
    }
}
