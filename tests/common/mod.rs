#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use pdf_squeezer::error::Ctx;
use pdf_squeezer::filters;

pub fn integration() -> bool {
    std::env::var("PDF_SQUEEZER_INTEGRATION").as_deref() == Ok("1")
}

/// Whether a tool is installed. Integration runs require every tool.
pub fn have(name: &str) -> bool {
    if pdf_squeezer::process::look_path(OsStr::new(name)).is_some() {
        return true;
    }
    assert!(!integration(), "{name} is required for integration tests");
    eprintln!("skipping: {name} is not installed");
    false
}

pub fn python() -> OsString {
    pdf_squeezer::tools::python_command()
}

pub fn have_python_tools() -> bool {
    let ok = Command::new(python())
        .args(["-c", "import pikepdf, pymupdf, PIL, fontTools"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok || !integration(), "optional Python tools are required");
    ok
}

pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub fn obj(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// A stream whose data ends right before the endstream keyword.
pub fn content(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"endstream");
    out
}

/// A stream with an end-of-line marker before endstream.
pub fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

pub fn build_pdf(padding: usize, objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    // Serialization whitespace can shrink without changing page content.
    pdf.extend(std::iter::repeat_n(b' ', padding));
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    pdf
}

pub const PAGE_TEXT: &str = "Squeeze this PDF.";

pub fn test_pdf(padding: usize) -> Vec<u8> {
    let text = format!("BT /F1 12 Tf 20 100 Td ({PAGE_TEXT}) Tj ET\n");
    build_pdf(
        padding,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"),
            obj("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"),
            content("", text.as_bytes()),
        ],
    )
}

/// Replaces the page text with another of the same length.
pub fn test_pdf_with_text(padding: usize, text: &str) -> Vec<u8> {
    assert_eq!(text.len(), PAGE_TEXT.len());
    let data = test_pdf(padding);
    let at = data.windows(PAGE_TEXT.len()).position(|w| w == PAGE_TEXT.as_bytes()).unwrap();
    let mut out = data.clone();
    out[at..at + text.len()].copy_from_slice(text.as_bytes());
    out
}

/// Draws an image with an unknown color space. Ghostscript 10 skips it with a warning.
pub fn broken_image_pdf() -> Vec<u8> {
    let draw = "q 100 0 0 100 50 50 cm /Im1 Do Q\n";
    build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>"),
            content("", draw.as_bytes()),
            stream("/Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceFoo /BitsPerComponent 8", b"0123456789abcdef"),
        ],
    )
}

pub const XMP: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:creator><rdf:Seq><rdf:li>Jane Example</rdf:li></rdf:Seq></dc:creator></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;

/// Metadata in every place --privacy must clear: the Info dictionary, document
/// XMP, and page-level XMP, piece info and modification date.
pub fn metadata_pdf_with_type(type_entry: &str) -> Vec<u8> {
    let page = "BT /F1 12 Tf 20 100 Td (Squeeze this PDF.) Tj ET /Fm Do\n";
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R /Metadata 7 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> /XObject << /Fm 9 0 R >> >> /Contents 5 0 R /Metadata 7 0 R /LastModified (D:20260101120000Z) /PieceInfo << /ExampleApp << /LastModified (D:20260101120000Z) /Private (Trace) >> >> >>",
            ),
            obj("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"),
            content("", page.as_bytes()),
            obj(
                "<< /Title (Secret Report) /Author (Jane Example) /Creator (Example Writer) /Producer (Example Writer) /CreationDate (D:20260101120000Z) /ModDate (D:20260101120000Z) >>",
            ),
            stream(&format!("{type_entry} /Subtype /XML"), XMP.as_bytes()),
            obj("/Metadata"),
            content("/Subtype /Form /BBox [0 0 200 200] /Resources << >> /Metadata 7 0 R", b"q Q\n"),
        ],
    );
    // The trailer follows the cross-reference table, so adding /Info keeps the offsets.
    let at = pdf.windows(11).rposition(|w| w == b"/Root 1 0 R").unwrap();
    let mut out = pdf[..at + 11].to_vec();
    out.extend_from_slice(b" /Info 6 0 R");
    out.extend_from_slice(&pdf[at + 11..]);
    out
}

pub fn metadata_pdf() -> Vec<u8> {
    metadata_pdf_with_type("/Type /Metadata")
}

pub fn piece_info_pdf() -> Vec<u8> {
    let page = "/Fm Do\n";
    let mut objects = vec![
        obj(
            "<< /Type /Catalog /Pages 2 0 R /PieceInfo 6 0 R /TestPage 3 0 R /TestForm 5 0 R /PrivateAlias 12 0 R /Names << /EmbeddedFiles << /Names [(private.bin) 13 0 R] >> >> >>",
        ),
        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R /PieceInfo 7 0 R >>"),
        content("", page.as_bytes()),
        content("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /PieceInfo 8 0 R", b"q Q\n"),
    ];
    for i in 0..3 {
        objects.push(obj(&format!("<< /ExampleApp << /LastModified (D:20260101120000Z) /Private {} 0 R >> >>", 9 + i)));
    }
    for i in 0..3 {
        objects.push(stream("/Shared 12 0 R /Back 6 0 R", format!("private-{i}").as_bytes()));
    }
    objects.push(stream("/Type /EmbeddedFile", b"shared-private"));
    objects.push(obj("<< /Type /Filespec /F (private.bin) /EF << /F 12 0 R >> >>"));
    build_pdf(64000, &objects)
}

pub fn image_pdf(bits: u8, color_space: &str, extra: &str, data: &[u8], more: &[Vec<u8>]) -> Vec<u8> {
    let draw = "q 64 0 0 64 0 0 cm /Im Do Q\n";
    let mut objects = vec![
        obj("<< /Type /Catalog /Pages 2 0 R >>"),
        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>"),
        content("", draw.as_bytes()),
        stream(&format!("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /{color_space} /BitsPerComponent {bits} {extra}"), data),
    ];
    objects.extend_from_slice(more);
    build_pdf(0, &objects)
}

/// Declares a PDF/A level in XMP without conforming to it.
pub fn pdfa_pdf(part: &str, conformance: &str) -> Vec<u8> {
    let mut xmp = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/" pdfaid:part="{part}""#
    );
    if !conformance.is_empty() {
        xmp.push_str(&format!(r#" pdfaid:conformance="{conformance}""#));
    }
    xmp.push_str("/></rdf:RDF></x:xmpmeta>");
    // Padding makes the rewrite smaller than the input; the DeviceRGB fill keeps the
    // document from conforming by accident.
    let fill = "0 0 1 rg 10 10 30 30 re f\n";
    build_pdf(
        64000,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 5 0 R >>"),
            stream("/Type /Metadata /Subtype /XML", xmp.as_bytes()),
            content("", fill.as_bytes()),
        ],
    )
}

pub struct Run {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pdf-squeezer"))
}

pub fn cli<S: AsRef<OsStr>>(args: &[S]) -> Run {
    cli_env(args, &[])
}

pub fn cli_env<S: AsRef<OsStr>>(args: &[S], env: &[(&str, &OsStr)]) -> Run {
    let mut cmd = Command::new(binary());
    cmd.args(args).stdin(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("run pdf-squeezer");
    Run {
        ok: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub fn write(path: &Path, data: &[u8]) {
    fs::write(path, data).unwrap();
}

pub fn read_file(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

pub fn stub(path: &Path, script: &str) {
    write(path, script.as_bytes());
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub fn read_doc(path: &Path, password: &str) -> Document {
    pdf_squeezer::pdf::load(&read_file(path), password).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn decoded(doc: &Document, s: &Stream) -> Vec<u8> {
    let pipeline = filters::pipeline(doc, &s.dict).unwrap();
    filters::decode(&Ctx::background(), &s.content, &pipeline, 1 << 30).unwrap()
}

pub fn try_decoded(doc: &Document, s: &Stream) -> Option<Vec<u8>> {
    let pipeline = filters::pipeline(doc, &s.dict).ok()?;
    filters::decode(&Ctx::background(), &s.content, &pipeline, 1 << 30).ok()
}

pub fn images(doc: &Document) -> Vec<(ObjectId, &Stream)> {
    doc.objects.iter().filter_map(|(id, o)| pdf_squeezer::images::image_stream(o).map(|s| (*id, s))).collect()
}

pub fn first_image(doc: &Document) -> &Stream {
    images(doc).first().map(|(_, s)| *s).expect("image missing")
}

pub fn name<'a>(d: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    pdf_squeezer::pdf::name_entry(d, key)
}

pub fn int(d: &Dictionary, key: &[u8]) -> Option<i64> {
    pdf_squeezer::pdf::int_entry(d, key)
}

pub fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    pdf_squeezer::pdf::deref(doc, obj)
}

pub fn dict<'a>(doc: &'a Document, obj: Option<&'a Object>) -> &'a Dictionary {
    match obj.and_then(|o| deref(doc, o)) {
        Some(Object::Dictionary(d)) => d,
        Some(Object::Stream(s)) => &s.dict,
        other => panic!("expected a dictionary, got {other:?}"),
    }
}

pub fn text(doc: &Document, obj: Option<&Object>) -> String {
    match obj.and_then(|o| deref(doc, o)) {
        Some(o @ Object::String(..)) => lopdf::decode_text_string(o).unwrap_or_default(),
        other => panic!("expected a string, got {other:?}"),
    }
}

pub fn catalog(doc: &Document) -> &Dictionary {
    pdf_squeezer::pdf::catalog(doc).expect("catalog")
}

/// The XObject resources of the document's only page.
pub fn page_forms(doc: &Document) -> &Dictionary {
    let pages = pdf_squeezer::pdf::page_ids(doc).unwrap();
    assert_eq!(pages.len(), 1);
    let page = dict(doc, doc.objects.get(&pages[0]));
    let resources = dict(doc, page.get(b"Resources").ok());
    dict(doc, resources.get(b"XObject").ok())
}

pub fn streams_containing(doc: &Document, needle: &[u8]) -> usize {
    doc.objects
        .values()
        .filter_map(|o| match o {
            Object::Stream(s) => try_decoded(doc, s),
            _ => None,
        })
        .filter(|data| data.windows(needle.len()).any(|w| w == needle))
        .count()
}

pub fn assert_no_temps(dir: &Path) {
    let left: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".pdf-squeezer-"))
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "staging directories remain: {left:?}");
}

pub fn assert_missing(path: &Path) {
    assert!(fs::symlink_metadata(path).is_err(), "expected no file at {}", path.display());
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

pub fn os(path: &Path) -> &OsStr {
    path.as_os_str()
}
