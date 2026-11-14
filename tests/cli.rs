mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use common::*;
use lopdf::Object;

fn args(items: &[&dyn AsRef<OsStr>]) -> Vec<OsString> {
    items.iter().map(|a| a.as_ref().to_os_string()).collect()
}

fn plus(base: &[&str], rest: &[&dyn AsRef<OsStr>]) -> Vec<OsString> {
    let mut out: Vec<OsString> = base.iter().map(OsString::from).collect();
    out.extend(rest.iter().map(|a| a.as_ref().to_os_string()));
    out
}

const FLAG_SETS: &[&[&str]] =
    &[&["--privacy"], &["--strip", "metadata"], &["--metadata", "Title=Updated"], &["--timestamps", "now"], &["--timestamps", "modified"]];

#[test]
fn engines() {
    for engine in ["native", "ghostscript"] {
        if engine == "ghostscript" && !have("gs") {
            continue;
        }
        for verbose in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("@scan 100%.pdf");
            let original = test_pdf(64 * 1024);
            write(&input, &original);
            let mut list: Vec<OsString> = vec!["--engine".into(), engine.into()];
            if engine == "ghostscript" {
                list.extend(["--quality".into(), "screen".into()]);
            }
            if verbose {
                list.push("-V".into());
            }
            list.push(input.clone().into());
            // Inherited options must not disable rendering or override safety flags.
            let run = cli_env(&list, &[("GS_OPTIONS", OsStr::new("-dNODISPLAY"))]);
            assert!(run.ok, "{engine}: {}", run.stderr);
            let output = dir.path().join("@scan 100%.squeezed.pdf");
            let result = read_file(&output);
            assert!(result.len() < original.len(), "did not shrink: {} -> {}", original.len(), result.len());
            assert_eq!(read_file(&input), original, "input changed");
            let doc = pdf_squeezer::pdf::load(&result, "").unwrap();
            assert_eq!(pdf_squeezer::pdf::page_ids(&doc).unwrap().len(), 1);
            assert!(run.stdout.contains(&format!("smaller, {engine}")), "missing size summary: {}", run.stdout);
            if verbose {
                assert!(run.stderr.contains("input: ") && run.stderr.contains(&format!("{engine}: ")), "{}", run.stderr);
            } else {
                assert!(run.stderr.is_empty(), "unexpected stderr: {}", run.stderr);
            }
            assert_no_temps(dir.path());
            if have("pdftotext") {
                let text = Command::new("pdftotext").arg(&output).arg("-").output().unwrap();
                assert!(contains(&text.stdout, PAGE_TEXT.as_bytes()), "text changed: {:?}", String::from_utf8_lossy(&text.stdout));
            }
        }
    }
}

#[test]
fn no_reduction() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("tiny.pdf"), dir.path().join("copy.pdf"));
    let original = test_pdf(0);
    write(&input, &original);
    let run = cli(&args(&[&"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(read_file(&output), original);
    assert!(run.stdout.contains("no reduction"), "{}", run.stdout);
    assert_no_temps(dir.path());
}

/// Every serialized object and decoded stream, for plaintext searches.
fn everything(doc: &lopdf::Document) -> Vec<u8> {
    let mut out = Vec::new();
    for obj in doc.objects.values() {
        pdf_squeezer::pdf::canonical(obj, &mut out);
        if let Object::Dictionary(d) = obj {
            for (_, v) in d.iter() {
                if let Object::String(..) = v {
                    out.extend_from_slice(lopdf::decode_text_string(v).unwrap_or_default().as_bytes());
                }
            }
        }
        if let Object::Stream(s) = obj
            && let Some(data) = try_decoded(doc, s)
        {
            out.extend_from_slice(&data);
        }
    }
    for (_, v) in doc.trailer.iter() {
        pdf_squeezer::pdf::canonical(v, &mut out);
    }
    out
}

#[test]
fn privacy() {
    for engine in ["native", "ghostscript"] {
        if engine == "ghostscript" && !have("gs") {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &metadata_pdf());
        let run = cli(&args(&[&"--engine", &engine, &"--privacy", &"-o", &output, &input]));
        assert!(run.ok, "{}", run.stderr);
        assert!(!run.stdout.contains("copied the original"), "{}", run.stdout);
        let doc = read_doc(&output, "");
        let all = everything(&doc);
        for secret in ["Secret Report", "Jane Example", "Example Writer", "20260101", "xpacket", "Trace"] {
            assert!(!contains(&all, secret.as_bytes()), "{engine}: {secret:?} remains in the output");
            assert!(!contains(&read_file(&output), secret.as_bytes()), "{engine}: {secret:?} remains in the file bytes");
        }
        if let Some(info) = pdf_squeezer::pdf::info_id(&doc) {
            assert!(dict(&doc, doc.objects.get(&info)).is_empty(), "info dictionary remains");
        }
        for (id, obj) in &doc.objects {
            let Some(d) = pdf_squeezer::pdf::dict_of(obj) else { continue };
            assert_ne!(name(d, b"Type"), Some(b"Metadata".as_slice()), "object {id:?} is a metadata stream");
            for key in [b"Metadata".as_slice(), b"PieceInfo", b"LastModified"] {
                assert!(!d.has(key), "{engine}: object {id:?} keeps {}", String::from_utf8_lossy(key));
            }
        }
    }
}

#[test]
fn metadata_stream_type_removal() {
    for type_entry in ["/Type /Metadata", "/Type 8 0 R", ""] {
        for flags in FLAG_SETS {
            let dir = tempfile::tempdir().unwrap();
            let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
            write(&input, &metadata_pdf_with_type(type_entry));
            let run = cli(&plus(flags, &[&"-o", &output, &input]));
            assert!(run.ok, "{type_entry} {flags:?}: {}", run.stderr);
            let doc = read_doc(&output, "");
            for (id, obj) in &doc.objects {
                match obj {
                    Object::Dictionary(d) => assert!(!d.has(b"Metadata"), "{type_entry} {flags:?}: object {id:?} retains XMP reference"),
                    Object::Stream(s) => {
                        if let Some(data) = try_decoded(&doc, s) {
                            assert!(!contains(&data, b"xpacket"), "{type_entry} {flags:?}: object {id:?} retains XMP bytes");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn metadata_on_untyped_annotation() {
    const XMP_TEXT: &str = "<private>Identifying annotation metadata</private>";
    for subtype in ["/XML", "6 0 R", "/XMP"] {
        for flags in FLAG_SETS {
            let dir = tempfile::tempdir().unwrap();
            let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
            write(
                &input,
                &build_pdf(
                    0,
                    &[
                        obj("<< /Type /Catalog /Pages 2 0 R >>"),
                        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Annots [4 0 R] >>"),
                        obj("<< /Subtype /Text /Rect [0 0 10 10] /Contents (Note) /Metadata 5 0 R >>"),
                        stream(&format!("/Subtype {subtype}"), XMP_TEXT.as_bytes()),
                        obj("/XML"),
                    ],
                ),
            );
            let run = cli(&plus(flags, &[&"-o", &output, &input]));
            assert!(run.ok, "{}", run.stderr);
            let doc = read_doc(&output, "");
            let mut found = false;
            for obj in doc.objects.values() {
                match obj {
                    Object::Dictionary(d) if name(d, b"Subtype") == Some(b"Text") => {
                        found = true;
                        assert!(!d.has(b"Metadata"), "{subtype} {flags:?}: untyped annotation retains XMP reference");
                    }
                    Object::Stream(s) => assert!(!try_decoded(&doc, s).is_some_and(|d| contains(&d, XMP_TEXT.as_bytes()))),
                    _ => {}
                }
            }
            assert!(found, "annotation removed instead of its metadata");
        }
    }
}

#[test]
fn metadata_editing_preserves_unidentified_streams() {
    const PAYLOAD: &str = "unrelated application data";
    for flags in FLAG_SETS {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(
            &input,
            &build_pdf(
                0,
                &[
                    obj("<< /Type /Catalog /Pages 2 0 R >>"),
                    obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                    obj(
                        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 6 0 R >> >> /Annots [4 0 R] /Contents 7 0 R >>",
                    ),
                    obj("<< /Type /Annot /Subtype /Text /Rect [0 0 10 10] /Contents (Note) /Metadata 5 0 R >>"),
                    stream("", PAYLOAD.as_bytes()),
                    content("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /Metadata 5 0 R", b"q Q\n"),
                    content("", b"/Fm Do"),
                ],
            ),
        );
        let run = cli(&plus(flags, &[&"-o", &output, &input]));
        assert!(run.ok, "{}", run.stderr);
        let doc = read_doc(&output, "");
        let owners: Vec<&lopdf::Dictionary> = doc
            .objects
            .values()
            .filter_map(|o| match o {
                Object::Dictionary(d) if name(d, b"Subtype") == Some(b"Text") => Some(d),
                Object::Stream(s) if name(&s.dict, b"Subtype") == Some(b"Form") => Some(&s.dict),
                _ => None,
            })
            .collect();
        assert_eq!(owners.len(), 2, "expected annotation and form");
        for d in owners {
            let Some(Object::Stream(data)) = d.get(b"Metadata").ok().and_then(|m| deref(&doc, m)) else {
                panic!("{flags:?}: unidentified stream was removed");
            };
            assert_eq!(decoded(&doc, data), PAYLOAD.as_bytes());
        }
    }
}

fn qpdf_check(path: &Path) {
    if integration() {
        let out = Command::new("qpdf").arg("--check").arg(path).output().unwrap();
        assert!(out.status.success(), "qpdf check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn piece_info_preservation() {
    let sets: &[&[&str]] = &[
        &[],
        &["--compression", "light"],
        &["--compression", "balanced"],
        &["--compression", "medium"],
        &["--compression", "strong"],
        &["--compression", "heavy"],
        &["--mono-codecs", "ccitt"],
        &["--privacy"],
        &["--strip", "piece-info"],
    ];
    for flags in sets {
        if flags.len() == 2 && ["medium", "strong", "heavy", "ccitt"].contains(&flags[1]) && !integration() {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        let original = piece_info_pdf();
        write(&input, &original);
        let run = cli(&plus(flags, &[&"-o", &output, &input]));
        assert!(run.ok, "{flags:?}: {}", run.stderr);
        assert_ne!(read_file(&output), original, "size fallback hid the optimization result");
        qpdf_check(&output);
        let doc = read_doc(&output, "");
        let root = catalog(&doc);
        let page = dict(&doc, root.get(b"TestPage").ok());
        let form = dict(&doc, root.get(b"TestForm").ok());
        let stripped = flags.first().is_some_and(|f| *f == "--privacy" || *f == "--strip");
        for (i, owner) in [root, page, form].into_iter().enumerate() {
            if stripped {
                assert!(!owner.has(b"PieceInfo"), "{flags:?}: owner {i} retains PieceInfo");
                continue;
            }
            let piece = dict(&doc, owner.get(b"PieceInfo").ok());
            let app = dict(&doc, piece.get(b"ExampleApp").ok());
            let Some(Object::Stream(private)) = app.get(b"Private").ok().and_then(|p| deref(&doc, p)) else {
                panic!("{flags:?}: owner {i} lost its private stream");
            };
            assert_eq!(decoded(&doc, private), format!("private-{i}").as_bytes());
            assert_eq!(private.dict.get(b"Shared").ok(), root.get(b"PrivateAlias").ok(), "private stream graph lost its shared reference");
        }
        let Some(Object::Stream(shared)) = root.get(b"PrivateAlias").ok().and_then(|p| deref(&doc, p)) else { panic!("shared stream freed") };
        assert_eq!(decoded(&doc, shared), b"shared-private");
        if stripped {
            assert_eq!(streams_containing(&doc, b"private-"), 0, "stripped private stream remains reachable");
        }
    }
}

#[test]
fn piece_info_encryption() {
    const MARKER: &str = "0123456789abcdef0123456789abcdef";
    const PAYLOAD: &str = "synthetic private child stream";
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("encrypted.pdf"));
    let (owner, user) = (dir.path().join("owner.txt"), dir.path().join("user.txt"));
    write(&owner, b"example-owner\n");
    write(&user, b"example-reader\n");
    write(
        &input,
        &build_pdf(
            64000,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R >>"),
                content("", b"/Fm Do\n"),
                content("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /PieceInfo 6 0 R /LastModified 9 0 R", b"q Q\n"),
                obj(&format!("<< /App << /LastModified 9 0 R /Private ({MARKER}) /Payload 7 0 R >> >>")),
                obj("<< /Stream 8 0 R >>"),
                stream("", PAYLOAD.as_bytes()),
                obj("(D:20260101120000Z)"),
            ],
        ),
    );
    let run = cli(&args(&[&"--encrypt-user-file", &user, &"--encrypt-owner-file", &owner, &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    for plaintext in [MARKER, PAYLOAD] {
        assert!(!contains(&read_file(&output), plaintext.as_bytes()), "encrypted output exposes {plaintext:?}");
    }
    let mut doc = read_doc(&output, "example-owner");
    let state = doc.encryption_state.as_ref().expect("expected encryption");
    assert_eq!((state.version(), state.revision()), (5, 6), "expected AES-256");
    if integration() {
        let decrypted = dir.path().join("decrypted.pdf");
        let out =
            Command::new("qpdf").arg(format!("--password-file={}", owner.display())).arg("--decrypt").arg(&output).arg(&decrypted).output().unwrap();
        assert!(out.status.success(), "independent decryption: {}", String::from_utf8_lossy(&out.stderr));
        doc = read_doc(&decrypted, "");
    }
    let forms = page_forms(&doc);
    let form = dict(&doc, forms.get(b"Fm").ok());
    let piece = dict(&doc, form.get(b"PieceInfo").ok());
    let app = dict(&doc, piece.get(b"App").ok());
    assert_eq!(text(&doc, app.get(b"Private").ok()), MARKER);
    assert_eq!(text(&doc, app.get(b"LastModified").ok()), "D:20260101120000Z");
    let private = dict(&doc, app.get(b"Payload").ok());
    let Some(Object::Stream(child)) = private.get(b"Stream").ok().and_then(|s| deref(&doc, s)) else { panic!("child stream missing") };
    assert_eq!(decoded(&doc, child), PAYLOAD.as_bytes());
}

#[test]
fn piece_info_distinct_form_bindings() {
    let mut objects = vec![
        obj("<< /Type /Catalog /Pages 2 0 R >>"),
        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>"),
        content("", b"/A Do /B Do\n"),
    ];
    for value in ["private-A", "private-B"] {
        objects.push(content(&format!("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /LastModified (D:20260101120000Z) /PieceInfo << /App << /LastModified (D:20260101120000Z) /Private ({value}) >> >> /pdfSqueezerPieceInfo 99 /pdfSqueezerPieceInfo_ 88"), b"q Q\n"));
    }
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &build_pdf(64000, &objects));
    let run = cli(&args(&[&"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let forms = page_forms(&doc);
    assert_ne!(forms.get(b"A").ok(), forms.get(b"B").ok(), "distinct private data merged into one resource binding");
    for key in ["A", "B"] {
        let form = dict(&doc, forms.get(key.as_bytes()).ok());
        let piece = dict(&doc, form.get(b"PieceInfo").ok());
        let app = dict(&doc, piece.get(b"App").ok());
        assert_eq!(text(&doc, app.get(b"Private").ok()), format!("private-{key}"));
        assert_eq!(int(form, b"pdfSqueezerPieceInfo"), Some(99));
        assert_eq!(int(form, b"pdfSqueezerPieceInfo_"), Some(88));
        assert!(!form.has(b"pdfSqueezerPieceInfo__"));
    }
}

#[test]
fn piece_info_strip_shared_lazy_graph() {
    if !have("qpdf") {
        return;
    }
    const PAYLOAD: &str = "shared lazy dictionary child";
    const MARKER: &str = "0123456789abcdef0123456789abcdef";
    for flags in [&["--strip", "piece-info"][..], &["--privacy"]] {
        for rewrite in ["native", "encrypt", "monochrome"] {
            if rewrite == "monochrome" && !integration() {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let (plain, input, output) = (dir.path().join("plain.pdf"), dir.path().join("in.pdf"), dir.path().join("out.pdf"));
            write(
                &plain,
                &build_pdf(
                    0,
                    &[
                        obj("<< /Type /Catalog /Pages 2 0 R /PieceInfo 6 0 R >>"),
                        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R >>"),
                        content("", b"/Fm Do\n"),
                        content("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /SharedPrivate 7 0 R", b"q Q\n"),
                        obj("<< /App << /LastModified (D:20260101120000Z) /Private 7 0 R >> >>"),
                        obj(&format!("<< /Value ({MARKER}) /Payload 8 0 R >>")),
                        stream("", PAYLOAD.as_bytes()),
                    ],
                ),
            );
            let out = Command::new("qpdf").arg("--object-streams=generate").arg(&plain).arg(&input).output().unwrap();
            assert!(out.status.success(), "prepare object streams: {}", String::from_utf8_lossy(&out.stderr));
            let mut list: Vec<OsString> = flags.iter().map(OsString::from).collect();
            let mut password = "";
            if rewrite == "encrypt" {
                password = "example-owner";
                let owner = dir.path().join("owner.txt");
                write(&owner, b"example-owner\n");
                list.extend(["--encrypt-owner-file".into(), owner.into()]);
            }
            if rewrite == "monochrome" {
                list.extend(["--mono-codecs".into(), "ccitt".into()]);
            }
            list.extend(["-o".into(), output.clone().into(), input.clone().into()]);
            let run = cli(&list);
            assert!(run.ok, "{flags:?} {rewrite}: {}", run.stderr);
            if rewrite == "encrypt" {
                for plaintext in [MARKER, PAYLOAD] {
                    assert!(!contains(&read_file(&output), plaintext.as_bytes()));
                }
            }
            let doc = read_doc(&output, password);
            if rewrite == "encrypt" {
                assert!(doc.encryption_state.is_some(), "shared graph output is not encrypted");
            }
            assert!(!catalog(&doc).has(b"PieceInfo"), "PieceInfo was not stripped");
            let forms = page_forms(&doc);
            let form = dict(&doc, forms.get(b"Fm").ok());
            let private = dict(&doc, form.get(b"SharedPrivate").ok());
            assert_eq!(text(&doc, private.get(b"Value").ok()), MARKER);
            let Some(Object::Stream(child)) = private.get(b"Payload").ok().and_then(|p| deref(&doc, p)) else { panic!("shared private child lost") };
            assert_eq!(decoded(&doc, child), PAYLOAD.as_bytes());
        }
    }
}

#[test]
fn long_indirect_chain_rewrite() {
    const COUNT: usize = 256;
    const MARKER: &str = "synthetic long-chain private value";
    const PAYLOAD: &str = "synthetic long-chain child stream";
    let mut objects = vec![
        obj("<< /Type /Catalog /Pages 2 0 R >>"),
        obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R >>"),
        content("", b"/Fm Do\n"),
        content("/Subtype /Form /BBox [0 0 64 64] /Resources << >> /Chain 6 0 R", b"q Q\n"),
    ];
    for n in 0..COUNT {
        let next = if n == COUNT - 1 { format!("/Value ({MARKER}) /Payload {} 0 R", COUNT + 6) } else { format!("/Next {} 0 R", n + 7) };
        objects.push(obj(&format!("<< /Index {n} {next} >>")));
    }
    objects.push(stream("", PAYLOAD.as_bytes()));
    for rewrite in ["native", "encrypt", "monochrome"] {
        if rewrite == "monochrome" && !integration() {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &build_pdf(64000, &objects));
        let mut list: Vec<OsString> = vec!["--force-recompression".into()];
        let mut password = "";
        if rewrite == "encrypt" {
            password = "example-owner";
            let owner = dir.path().join("owner.txt");
            write(&owner, b"example-owner\n");
            list.extend(["--encrypt-owner-file".into(), owner.into()]);
        } else if rewrite == "monochrome" {
            list.extend(["--mono-codecs".into(), "ccitt".into()]);
        }
        list.extend(["-o".into(), output.clone().into(), input.clone().into()]);
        let run = cli(&list);
        assert!(run.ok, "{rewrite}: {}", run.stderr);
        let doc = read_doc(&output, password);
        if rewrite == "encrypt" {
            assert!(doc.encryption_state.is_some());
            assert!(!contains(&read_file(&output), MARKER.as_bytes()) && !contains(&read_file(&output), PAYLOAD.as_bytes()));
        }
        let forms = page_forms(&doc);
        let form = dict(&doc, forms.get(b"Fm").ok());
        let mut node = form.get(b"Chain").ok();
        let mut tail = None;
        for n in 0..COUNT {
            let d = dict(&doc, node);
            assert_eq!(int(d, b"Index"), Some(n as i64), "chain node {n} changed");
            node = d.get(b"Next").ok();
            tail = Some(d);
        }
        let tail = tail.unwrap();
        assert_eq!(text(&doc, tail.get(b"Value").ok()), MARKER);
        let Some(Object::Stream(child)) = tail.get(b"Payload").ok().and_then(|p| deref(&doc, p)) else { panic!("terminal stream missing") };
        assert_eq!(decoded(&doc, child), PAYLOAD.as_bytes());
    }
}

#[test]
fn rejects_invalid_arguments() {
    for list in [
        &[][..],
        &["one.pdf", "two.pdf"],
        &["--engine", "other", "input.pdf"],
        &["--quality", "screen", "input.pdf"],
        &["--engine", "ghostscript", "--quality", "other", "input.pdf"],
        &["--unknown"],
    ] {
        let run = cli(list);
        assert!(!run.ok, "accepted {list:?}");
    }
    for flag in ["-h", "--help"] {
        let run = cli(&[flag]);
        assert!(run.ok, "{flag}: {}", run.stderr);
        assert!(run.stderr.contains("Usage: pdf-squeezer"), "{}", run.stderr);
    }
}

#[test]
fn trailing_flags_and_output_dir() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("doc.pdf");
    write(&input, &test_pdf(1000));
    let run = cli(&args(&[&input, &"-o", &out_dir.path(), &"-V"]));
    assert!(run.ok, "{}", run.stderr);
    let output = out_dir.path().join("doc.squeezed.pdf");
    assert!(run.stdout.starts_with(&format!("{}: ", output.display())), "{}", run.stdout);
    assert!(run.stderr.contains("input: "), "{}", run.stderr);
    read_file(&output);
    assert!(!cli(&args(&[&"--", &input, &"-V"])).ok, "treated an argument after -- as a flag");
}

#[test]
fn version() {
    for flag in ["-v", "--version"] {
        let run = cli(&[flag]);
        assert!(run.ok);
        let v = run.stdout.strip_suffix('\n').unwrap_or(&run.stdout);
        assert!(!v.is_empty() && !v.contains([' ', '\n']), "{flag} printed {:?}", run.stdout);
    }
}

#[test]
fn rejects_invalid_input() {
    for data in [&b""[..], b"not a PDF", b"%PDF-1.7\nbroken"] {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("bad.pdf"), dir.path().join("out.pdf"));
        write(&input, data);
        assert!(!cli(&args(&[&"-o", &output, &input])).ok, "accepted invalid PDF {data:?}");
        assert_missing(&output);
        assert_no_temps(dir.path());
    }
    let dir = tempfile::tempdir().unwrap();
    for input in [dir.path().to_path_buf(), dir.path().join("missing.pdf")] {
        assert!(!cli(&args(&[&input])).ok, "accepted {}", input.display());
    }
}

#[test]
fn no_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.pdf");
    let original = test_pdf(1000);
    write(&input, &original);
    for kind in ["same", "existing", "symlink", "hardlink"] {
        let output = match kind {
            "same" => input.clone(),
            _ => dir.path().join(format!("{kind}.pdf")),
        };
        match kind {
            "existing" => write(&output, &original),
            "symlink" => std::os::unix::fs::symlink(&input, &output).unwrap(),
            "hardlink" => fs::hard_link(&input, &output).unwrap(),
            _ => {}
        }
        assert!(!cli(&args(&[&"-o", &output, &input])).ok, "{kind}: overwrote an existing file");
        assert_eq!(read_file(&output), original);
        assert_eq!(read_file(&input), original);
    }
    assert_no_temps(dir.path());
}

#[test]
fn missing_ghostscript() {
    let empty = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &test_pdf(1000));
    let run = cli_env(&args(&[&"--engine", &"ghostscript", &"-o", &output, &input]), &[("PATH", empty.path().as_os_str())]);
    assert!(!run.ok && run.stderr.contains("Ghostscript not found"), "{}", run.stderr);
    assert_missing(&output);
    assert_no_temps(dir.path());
}

#[test]
fn ghostscript_rejects_dropped_images() {
    if !have("gs") {
        return;
    }
    let version = Command::new("gs").arg("--version").output().unwrap();
    let major: u32 = String::from_utf8_lossy(&version.stdout).split('.').next().and_then(|m| m.trim().parse().ok()).unwrap_or(0);
    if major < 10 {
        assert!(!integration(), "Ghostscript 10 or newer is required");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &broken_image_pdf());
    assert!(!cli(&args(&[&"--engine", &"ghostscript", &"-o", &output, &input])).ok, "accepted output with a dropped image");
    assert_missing(&output);
    assert_no_temps(dir.path());
}

#[test]
fn broken_ghostscript_output() {
    for exit in ["0", "1"] {
        let dir = tempfile::tempdir().unwrap();
        stub(&dir.path().join("gs"), &format!("#!/bin/sh\nprintf '%s\\n' '%PDF-1.7' 'broken'\nexit {exit}\n"));
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &test_pdf(1000));
        let run = cli_env(&args(&[&"--engine", &"ghostscript", &"-o", &output, &input]), &[("PATH", dir.path().as_os_str())]);
        assert!(!run.ok, "accepted broken Ghostscript output");
        assert_missing(&output);
        assert_no_temps(dir.path());
    }
}

#[test]
fn lossless_16bit_and_matte_preservation() {
    for matte in [false, true] {
        let mut rgb = Vec::new();
        let mut mask = Vec::new();
        for i in 0..4096u32 {
            rgb.extend_from_slice(&[0x80, i as u8, 0x80, i as u8, 0x80, i as u8]);
            mask.extend_from_slice(&[0x7f, i as u8]);
        }
        let matte_entry = if matte { "/Matte [0.5 0.5 0.5]" } else { "" };
        let mask_object =
            stream(&format!("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 16 {matte_entry}"), &mask);
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &image_pdf(16, "DeviceRGB", "/SMask 6 0 R", &rgb, &[mask_object]));
        let mut list = vec!["--images", "--force-recompression", "-V"];
        list.push(if matte { "--reduce-bit-depth" } else { "--lossless" });
        let run = cli(&plus(&list, &[&"-o", &output, &input]));
        assert!(run.ok, "{}", run.stderr);
        let doc = read_doc(&output, "");
        let (parent, alpha) = images(&doc)
            .into_iter()
            .find_map(|(_, s)| {
                let Ok(Object::Reference(m)) = s.dict.get(b"SMask") else { return None };
                let Some(Object::Stream(alpha)) = doc.objects.get(m) else { return None };
                Some((s, alpha))
            })
            .expect("soft-masked image");
        assert_eq!(int(&alpha.dict, b"BitsPerComponent"), Some(16), "16-bit soft mask lost precision");
        assert_eq!(decoded(&doc, alpha), mask, "soft mask changed");
        assert_eq!(int(&parent.dict, b"BitsPerComponent"), Some(16), "parent bit depth changed");
        if matte {
            assert_eq!(parent.content, rgb, "matte-backed image was transformed");
            assert!(run.stderr.contains("soft-mask matte"), "{}", run.stderr);
        }
    }
}

#[test]
fn final_deduplication_across_lossless_filters() {
    let data: Vec<u8> = [19u8, 200].repeat(2048);
    let compressed = deflate(&data);
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>"),
            content("", b"q 32 0 0 32 0 0 cm /A Do Q q 32 0 0 32 32 0 cm /B Do Q"),
            stream("/Type /XObject /Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray", &data),
            stream(
                "/Type /XObject /Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray /Filter /FlateDecode",
                &compressed,
            ),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &pdf);
    let run = cli(&args(&[&"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(images(&read_doc(&output, "")).len(), 1, "duplicate images were not merged");
}

#[test]
fn parallel_jobs_keep_input_order() {
    let dir = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut want = Vec::new();
    for i in 0..6 {
        // The first file is the largest, so a completion-order report would show it last.
        let padding = if i == 0 { 16 << 20 } else { 64000 };
        let mut data = test_pdf_with_text(padding, &format!("Document {i:02} text."));
        if i == 3 {
            data = b"%PDF-1.7\nbroken".to_vec();
        } else {
            want.push(output.path().join(format!("{i:02}.squeezed.pdf")));
        }
        write(&dir.path().join(format!("{i:02}.pdf")), &data);
    }
    let run = cli(&args(&[&"--jobs", &"4", &"-V", &"-o", &output.path(), &dir.path()]));
    assert!(!run.ok);
    let failure_lines: Vec<&str> = run.stderr.lines().filter(|l| l.starts_with("pdf-squeezer: ")).collect();
    assert_eq!(failure_lines.len(), 1, "expected one failure: {}", run.stderr);
    assert!(failure_lines[0].contains("03.pdf"), "{}", failure_lines[0]);
    let got: Vec<PathBuf> = run.stdout.lines().map(|l| PathBuf::from(l.split(": ").next().unwrap())).collect();
    assert_eq!(got, want, "reports out of order:\n{}", run.stdout);
    let prefix = format!("{}: input: ", dir.path().join("05.pdf").display());
    assert!(run.stderr.contains(&prefix), "parallel lines do not name their file:\n{}", run.stderr);
    for path in &want {
        let n = &path.file_name().unwrap().to_string_lossy()[..2];
        let doc = read_doc(path, "");
        assert_eq!(streams_containing(&doc, format!("Document {n} text.").as_bytes()), 1, "{} lacks its text", path.display());
    }
    assert_no_temps(output.path());
}

#[test]
fn profiles_batch_and_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let profile = profiles.path().join("profile.json");
    let run = cli(&args(&[&"--lossless", &"--save-profile", &profile]));
    assert!(run.ok, "{}", run.stderr);
    let saved: serde_json::Value = serde_json::from_slice(&read_file(&profile)).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["flags"]["lossless"], "true");
    assert_eq!(saved["flags"]["image-codecs"], "flate");
    fs::create_dir(dir.path().join("child")).unwrap();
    let date = SystemTime::UNIX_EPOCH + Duration::from_secs(1_580_702_706);
    for name in ["a.pdf", "child/b.pdf", "a.squeezed.pdf"] {
        let path = dir.path().join(name);
        write(&path, &test_pdf(64000));
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_times(fs::FileTimes::new().set_accessed(date).set_modified(date)).unwrap();
    }
    let base = args(&[&"--profile", &profile, &"--recursive", &"-o", &output.path(), &dir.path()]);
    let run = cli(&base);
    assert!(run.ok, "{}", run.stderr);
    for name in ["a.squeezed.pdf", "child/b.squeezed.pdf"] {
        let modified = fs::metadata(output.path().join(name)).unwrap().modified().unwrap();
        assert_eq!(modified, date, "timestamp changed for {name}");
    }
    assert_missing(&output.path().join("a.squeezed.squeezed.pdf"));
    assert!(!cli(&base).ok, "overwrote batch outputs");
    let mut numbered = base.clone();
    numbered.extend(["--collision".into(), "number".into()]);
    assert!(cli(&numbered).ok);
    read_file(&output.path().join("a.squeezed.1.pdf"));
    write(&profile, br#"{"version":1,"flags":{"output":"unexpected.pdf"}}"#);
    assert!(!cli(&base).ok, "accepted path-bearing profile");
}

#[test]
fn metadata_editing_and_required_output() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &metadata_pdf());
    let run = cli(&args(&[&"--metadata", &"Title=Résumé", &"--metadata", &"Author=", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let info = dict(&doc, doc.trailer.get(b"Info").ok());
    assert_eq!(text(&doc, info.get(b"Title").ok()), "Résumé");
    assert!(!info.has(b"Author"), "author remains");
    for obj in doc.objects.values() {
        if let Object::Stream(s) = obj {
            assert_ne!(name(&s.dict, b"Type"), Some(b"Metadata".as_slice()), "stale XMP remains");
        }
    }
}

#[test]
fn encryption_round_trip() {
    if !have("qpdf") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n);
    let (input, encrypted, squeezed, decrypted) = (p("in.pdf"), p("encrypted.pdf"), p("squeezed.pdf"), p("decrypted.pdf"));
    let (user, owner) = (p("user.txt"), p("owner.txt"));
    write(&input, &test_pdf(64000));
    write(&user, b"example-reader\n");
    write(&owner, b"example-owner\n");
    let run = cli(&args(&[&"--encrypt-user-file", &user, &"--encrypt-owner-file", &owner, &"-o", &encrypted, &input]));
    assert!(run.ok, "{}", run.stderr);
    assert!(pdf_squeezer::pdf::load(&read_file(&encrypted), "wrong").is_err(), "encrypted PDF opened without password");
    assert!(read_doc(&encrypted, "example-reader").encryption_state.is_some(), "missing encryption");
    let run = cli(&args(&[&"--password-file", &user, &"-o", &squeezed, &encrypted]));
    assert!(run.ok, "{}", run.stderr);
    assert!(read_doc(&squeezed, "example-reader").encryption_state.is_some(), "input encryption was discarded");
    let run = cli(&args(&[&"--password-file", &owner, &"--decrypt", &"-o", &decrypted, &encrypted]));
    assert!(run.ok, "{}", run.stderr);
    assert!(read_doc(&decrypted, "").encryption_state.is_none(), "decryption did not remove encryption");
}

/// Reads one gray sample from a PNG this CLI wrote.
fn png_gray_at(png: &[u8], x: usize, y: usize) -> u8 {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let (mut pos, mut idat, mut width, mut depth) = (8, Vec::new(), 0usize, 0u8);
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &png[pos + 4..pos + 8];
        let data = &png[pos + 8..pos + 8 + len];
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
                depth = data[8];
                assert_eq!(data[9], 0, "not a gray PNG");
            }
            b"IDAT" => idat.extend_from_slice(data),
            _ => {}
        }
        pos += 12 + len;
    }
    assert_eq!(depth, 8);
    let raw = pdf_squeezer::filters::inflate(&pdf_squeezer::error::Ctx::background(), &idat, 1 << 26).unwrap();
    let mut parms = lopdf::Dictionary::new();
    parms.set("Predictor", 15);
    parms.set("Columns", width as i64);
    let pixels = pdf_squeezer::filters::unpredict(raw, Some(&parms), 1 << 26).unwrap();
    pixels[y * width + x]
}

#[test]
fn extract_and_inline_images() {
    if !have("qpdf") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("inline.pdf");
    let mut page = b"q 64 0 0 64 0 0 cm BI /W 64 /H 64 /BPC 8 /CS /G ID\n".to_vec();
    page.extend(std::iter::repeat_n(b'x', 4096));
    page.extend_from_slice(b"\nEI Q\n");
    write(
        &input,
        &build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Contents 4 0 R >>"),
                content("", &page),
            ],
        ),
    );
    let output = dir.path().join("out.pdf");
    let run = cli(&args(&[&"--inline-images", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    first_image(&read_doc(&output, ""));
    let extracted = dir.path().join("extracted");
    let run = cli(&args(&[&"--extract", &"images", &"-o", &extracted, &output]));
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(fs::read_dir(&extracted).unwrap().count(), 1);
    assert!(!cli(&args(&[&"--extract", &"images", &"-o", &extracted, &output])).ok, "overwrote extraction directory");
}

#[test]
fn extract_writes_every_image_once() {
    let dir = tempfile::tempdir().unwrap();
    let (input, extracted) = (dir.path().join("in.pdf"), dir.path().join("extracted"));
    let image =
        |level: u8| stream("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8", &[level; 64 * 64]);
    // Page 2 draws page 1's image again next to its own, which must be written once.
    write(
        &input,
        &build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 7 0 R >> >> /Contents 5 0 R >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 7 0 R /B 8 0 R >> >> /Contents 6 0 R >>"),
                content("", b"q 64 0 0 64 0 0 cm /A Do Q\n"),
                content("", b"q 32 0 0 32 0 0 cm /A Do Q q 32 0 0 32 32 32 cm /B Do Q\n"),
                image(0x40),
                image(0xc0),
            ],
        ),
    );
    let run = cli(&args(&[&"--extract", &"images", &"-o", &extracted, &input]));
    assert!(run.ok, "{}", run.stderr);
    let mut names: Vec<String> = fs::read_dir(&extracted).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["image-000007.png", "image-000008.png"]);
    for (file, level) in [("image-000007.png", 0x40), ("image-000008.png", 0xc0)] {
        assert_eq!(png_gray_at(&read_file(&extracted.join(file)), 10, 10), level, "{file}");
    }
}

#[test]
fn ghostscript_compact_font_programs() {
    if !have("gs") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &test_pdf(64000));
    let run = cli(&args(&[&"--convert-fonts-cff", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let compact = doc.objects.values().any(|o| match o {
        Object::Stream(s) => matches!(name(&s.dict, b"Subtype"), Some(b"Type1C" | b"CIDFontType0C")),
        _ => false,
    });
    assert!(compact, "Ghostscript did not embed a compact font program");
}

#[test]
fn ghostscript_font_conversion_keeps_images_lossless() {
    if !have("gs") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    // pdfwrite only auto-selects DCT for large photographic images; use xorshift noise.
    let mut x: u32 = 2463534242;
    let samples: Vec<u8> = (0..3 * 1024 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    write(
        &input,
        &build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>"),
                content("", b"q 200 0 0 200 0 0 cm /Im Do Q\n"),
                stream("/Type /XObject /Subtype /Image /Width 1024 /Height 1024 /ColorSpace /DeviceRGB /BitsPerComponent 8", &samples),
            ],
        ),
    );
    // --timestamps now keeps the rewritten document even when it is larger.
    let run = cli(&args(&[&"--convert-fonts-cff", &"--timestamps", &"now", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let pipeline = pdf_squeezer::filters::pipeline(&doc, &first_image(&doc).dict).unwrap();
    assert!(pipeline.iter().all(|f| f.name != b"DCTDecode"), "font conversion re-encoded a lossless image as JPEG");
}

fn flavour(path: &Path) -> String {
    pdf_squeezer::tools::declared_flavour(&pdf_squeezer::error::Ctx::background(), path).unwrap()
}

#[test]
fn declared_pdfa_missing_validator() {
    let empty = tempfile::tempdir().unwrap();
    for count in [1, 2, pdf_squeezer::run::VALIDATION_BATCH + 1] {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let original = pdfa_pdf("2", "B");
        for i in 0..count {
            write(&dir.path().join(format!("{i:02}.pdf")), &original);
        }
        let run = cli_env(&args(&[&"-o", &output.path(), &dir.path()]), &[("PATH", empty.path().as_os_str())]);
        assert!(run.ok, "{}", run.stderr);
        assert_eq!(run.stderr.matches("declared PDF/A conformance is not verified").count(), 1, "expected one warning: {}", run.stderr);
        for i in 0..count {
            assert_eq!(read_file(&dir.path().join(format!("{i:02}.pdf"))), original, "input changed");
            assert_eq!(flavour(&output.path().join(format!("{i:02}.squeezed.pdf"))), "2b", "declaration changed");
        }
        assert_no_temps(output.path());
    }
}

#[test]
fn declared_pdfa1_keeps_classic_cross_reference() {
    let dir = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &pdfa_pdf("1", "B"));
    // No veraPDF: preserve the structure, warn, and publish.
    let run = cli_env(&args(&[&"-V", &"-o", &output, &input]), &[("PATH", empty.path().as_os_str())]);
    assert!(run.ok, "{}", run.stderr);
    let data = read_file(&output);
    assert!(!contains(&data, b"/XRef") && !contains(&data, b"/ObjStm"), "PDF/A-1 input was written with cross-reference or object streams");
    assert!(run.stderr.contains("declares PDF/A-1B") && run.stderr.contains("not verified"), "{}", run.stderr);
    let broken = dir.path().join("broken.pdf");
    let run = cli_env(&args(&[&"--privacy", &"-o", &broken, &input]), &[("PATH", empty.path().as_os_str())]);
    assert!(run.ok, "{}", run.stderr);
    assert!(run.stderr.contains("--privacy removes what it requires"), "{}", run.stderr);
}

#[test]
fn pdfa4_fails_closed_without_tools() {
    let dir = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &test_pdf(0));
    let run =
        cli_env(&args(&[&"--pdfa", &"4", &"-o", &output, &input]), &[("PATH", empty.path().as_os_str()), ("PDF_SQUEEZER_PYTHON", OsStr::new(""))]);
    assert!(!run.ok, "PDF/A-4 succeeded without its tools");
    assert_missing(&output);
    assert_no_temps(dir.path());
}

#[test]
fn pdfa_conversion() {
    if !integration() || !have("verapdf") || !have("qpdf") || !have_python_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("text.pdf");
    write(&input, &test_pdf(0));
    let attached = dir.path().join("attached.pdf");
    write(
        &attached,
        &build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(note.txt) 4 0 R] >> >> >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>"),
                obj("<< /Type /Filespec /F (note.txt) /EF << /F 5 0 R >> >>"),
                stream("/Type /EmbeddedFile", b"hello"),
            ],
        ),
    );
    for (name, source, header, flags, want) in [
        ("text-4", &input, "%PDF-2.0", &["--pdfa", "4", "-V"][..], "4"),
        ("text-2b", &input, "%PDF-1.7", &["--pdfa", "2b"], "2b"),
        ("attached-4f", &attached, "%PDF-2.0", &["--pdfa", "4"], "4f"),
        ("attached-3b", &attached, "%PDF-1.7", &["--pdfa", "3b"], "3b"),
        ("attached-stripped", &attached, "%PDF-2.0", &["--pdfa", "4", "--strip", "attachments"], "4"),
    ] {
        let out = dir.path().join(format!("{name}.pdf"));
        let run = cli(&plus(flags, &[&"-o", &out, source]));
        assert!(run.ok, "{name}: {}", run.stderr);
        assert!(read_file(&out).starts_with(header.as_bytes()), "{name}: header is not {header}");
        assert_eq!(flavour(&out), want, "{name}");
    }
    // PDF/A-2 refuses attachments it cannot prove to be PDF/A.
    let run = cli(&args(&[&"--pdfa", &"2b", &"-o", &dir.path().join("refused.pdf"), &attached]));
    assert!(!run.ok && run.stderr.contains("--pdfa 3b"), "{}", run.stderr);
    // A batch shares one validator run and publishes every compliant result.
    let batch = dir.path().join("batch");
    fs::create_dir(&batch).unwrap();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        write(&batch.join(name), &test_pdf(0));
    }
    let out_dir = dir.path().join("batch-out");
    fs::create_dir(&out_dir).unwrap();
    let run = cli(&args(&[&"--pdfa", &"4", &"-o", &out_dir, &batch]));
    assert!(run.ok, "batch: {}", run.stderr);
    for name in ["a.squeezed.pdf", "b.squeezed.pdf", "c.squeezed.pdf"] {
        assert_eq!(flavour(&out_dir.join(name)), "4");
    }
    let cmyk = dir.path().join("cmyk.pdf");
    write(
        &cmyk,
        &build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>"),
                content("", b"0 0 0 1 k 10 10 50 50 re f\n"),
            ],
        ),
    );
    let failed = dir.path().join("cmyk-a4.pdf");
    let run = cli(&args(&[&"--pdfa", &"4", &"-o", &failed, &cmyk]));
    assert!(!run.ok && run.stderr.contains("--output-intent"), "DeviceCMYK without a CMYK intent: {}", run.stderr);
    assert_missing(&failed);
    assert_no_temps(dir.path());
    // A declared PDF/A input that does not conform is published with a warning.
    let declared = dir.path().join("declared.pdf");
    write(&declared, &pdfa_pdf("2", "B"));
    let run = cli(&args(&[&"-o", &dir.path().join("declared-out.pdf"), &declared]));
    assert!(run.ok, "declared input: {}", run.stderr);
    assert!(run.stderr.contains("does not conform"), "{}", run.stderr);
}

#[test]
fn advanced_cli() {
    if !integration() || !have_python_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.pdf");
    write(&input, &image_pdf(8, "DeviceRGB", "", &[30u8, 80, 140].repeat(4096), &[]));
    for (i, flags) in [&["--dpi", "36"][..], &["--gray"], &["--bitmap"], &["--mrc"], &["--subset-fonts"], &["--convert-fonts-cff"]].iter().enumerate()
    {
        let output = dir.path().join(format!("out-{i}.pdf"));
        let run = cli(&plus(flags, &[&"-o", &output, &input]));
        assert!(run.ok, "{flags:?}: {}", run.stderr);
        read_doc(&output, "");
    }
    let (text_input, text_output) = (dir.path().join("text.pdf"), dir.path().join("text.txt"));
    write(&text_input, &test_pdf(0));
    let run = cli(&args(&[&"--extract", &"text", &"-o", &text_output, &text_input]));
    assert!(run.ok, "{}", run.stderr);
    assert!(contains(&read_file(&text_output), PAGE_TEXT.as_bytes()), "text extraction failed");
    assert_no_temps(dir.path());
}

#[test]
fn lossless_keeps_gray_endpoint_samples() {
    let samples: Vec<u8> = [0u8, 255].repeat(2048);
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &image_pdf(8, "DeviceGray", "", &samples, &[]));
    let run = cli(&args(&[&"--lossless", &"--force-recompression", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let image = first_image(&doc);
    assert_eq!(int(&image.dict, b"BitsPerComponent"), Some(8), "lossless mode changed 8-bit samples");
    assert_eq!(name(&image.dict, b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(decoded(&doc, image), samples);
}

#[test]
fn lossless_keeps_rgb_color_space() {
    let samples: Vec<u8> = [100u8, 100, 100].repeat(4096);
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &image_pdf(8, "DeviceRGB", "", &samples, &[]));
    let run = cli(&args(&[&"--lossless", &"--force-recompression", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let image = first_image(&doc);
    assert_eq!(name(&image.dict, b"ColorSpace"), Some(b"DeviceRGB".as_slice()), "lossless mode changed color semantics");
    assert_eq!(decoded(&doc, image), samples);
}

#[test]
fn null_annotations_read_as_absent() {
    for annots in ["/Annots null", "/Annots 5 0 R"] {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(
            &input,
            &build_pdf(
                64000,
                &[
                    obj("<< /Type /Catalog /Pages 2 0 R >>"),
                    obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                    obj(&format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Contents 4 0 R {annots} >>")),
                    content("", b"q Q\n"),
                    obj("null"),
                ],
            ),
        );
        let run = cli(&args(&[&"--strip", &"annotations", &"-o", &output, &input]));
        assert!(run.ok, "{annots}: {}", run.stderr);
    }
}

#[test]
fn strip_does_not_remove_named_resources() {
    for resource in ["B", "Metadata", "PieceInfo"] {
        let pdf = build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject 6 0 R >> /Contents 4 0 R >>"),
                content("", format!("q 64 0 0 64 0 0 cm /{resource} Do Q").as_bytes()),
                stream("/Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray", &[b'x'; 4096]),
                obj(&format!("<< /{resource} 5 0 R >>")),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &pdf);
        let run = cli(&args(&[&"--strip", &"threads,metadata,piece-info", &"-o", &output, &input]));
        assert!(run.ok, "{}", run.stderr);
        first_image(&read_doc(&output, ""));
    }
}

#[test]
fn monochrome_cli_uses_requested_codec() {
    if !integration() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.pdf");
    write(&input, &image_pdf(8, "DeviceGray", "", &[0u8, 255].repeat(2048), &[]));
    for codec in ["ccitt", "jbig2"] {
        let output = dir.path().join(format!("{codec}.pdf"));
        let run = cli(&args(&[&"--images", &"--force-recompression", &"--mono-codecs", &codec, &"-o", &output, &input]));
        assert!(run.ok, "{codec}: {}", run.stderr);
        let doc = read_doc(&output, "");
        let want: &[u8] = if codec == "jbig2" { b"JBIG2Decode" } else { b"CCITTFaxDecode" };
        assert_eq!(name(&first_image(&doc).dict, b"Filter"), Some(want), "requested {codec}");
    }
}

/// 4x4 blocks of pseudo-random levels: cheap for Flate with predictors, expensive
/// for JPEG, and not trivially repetitive for plain Flate.
fn blocks(w: usize, h: usize, levels: &[u8], seed: u32) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let n = ((x / 4) as u32).wrapping_add(seed).wrapping_mul(2654435761).wrapping_add(((y / 4) as u32).wrapping_mul(40503));
            out[y * w + x] = levels[((n >> 7) % levels.len() as u32) as usize];
        }
    }
    out
}

fn noise_jpeg(w: u16, h: u16, quality: u8) -> Vec<u8> {
    let pixels: Vec<u8> = (0..w as u32 * h as u32 * 3).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8 | 1).collect();
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality).encode(&pixels, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

#[test]
fn images_reencode_and_preserve() {
    let gray = blocks(512, 512, &[0, 128, 255], 1);
    let rgb: Vec<u8> = gray.iter().flat_map(|&v| [v, v, v]).collect();
    // Three distinct black-and-white images, so deduplication cannot merge them.
    let (bilevel, masked, mask) = (blocks(512, 256, &[0, 255], 2), blocks(512, 256, &[0, 255], 3), blocks(512, 256, &[0, 255], 4));
    let photo = noise_jpeg(64, 64, 50);
    let image = |name: &str, dict: &str, data: &[u8]| stream(&format!("/Type /XObject /Subtype /Image /Name /{name} {dict}"), data);
    let draw: String = ["A", "B", "C", "D", "E", "F"].iter().enumerate().map(|(i, n)| format!("q 30 0 0 30 {} 0 cm /{n} Do Q\n", 32 * i)).collect();
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /XObject << /A 5 0 R /B 6 0 R /C 7 0 R /D 8 0 R /E 9 0 R /F 10 0 R >> >> >>",
            ),
            content("", draw.as_bytes()),
            image("grayrgb", "/Width 512 /Height 512 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode", &deflate(&rgb)),
            image("bilevel", "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode", &deflate(&bilevel)),
            image("photo", "/Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode", &photo),
            image("mask", "/Width 8 /Height 8 /ImageMask true /BitsPerComponent 1", &[0xAA; 8]),
            image("indexed", "/Width 16 /Height 16 /ColorSpace [/Indexed /DeviceRGB 1 <000000ffffff>] /BitsPerComponent 8", &[0u8, 1].repeat(128)),
            image(
                "softmasked",
                "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /SMask 11 0 R",
                &deflate(&masked),
            ),
            image("softmask", "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode", &deflate(&mask)),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &pdf);
    let run = cli(&args(&[&"--images", &"-V", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    assert!(
        run.stderr.contains("images: 7 seen, ") && run.stderr.contains("preserved: color space Indexed 1, image mask 1, no gain"),
        "unexpected image stats:\n{}",
        run.stderr
    );
    let doc = read_doc(&output, "");
    let by_name: std::collections::HashMap<String, &lopdf::Stream> =
        images(&doc).into_iter().map(|(_, s)| (String::from_utf8_lossy(name(&s.dict, b"Name").unwrap()).into_owned(), s)).collect();
    assert_eq!(by_name.len(), 7);
    let gray_out = by_name["grayrgb"];
    assert_eq!(name(&gray_out.dict, b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(int(&gray_out.dict, b"BitsPerComponent"), Some(8));
    assert_eq!(decoded(&doc, gray_out), gray, "grayrgb: want lossless DeviceGray");
    let mut packed = vec![0u8; 512 / 8 * 256];
    for (i, &v) in bilevel.iter().enumerate() {
        if v != 0 {
            packed[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let bilevel_out = by_name["bilevel"];
    assert_eq!(int(&bilevel_out.dict, b"BitsPerComponent"), Some(1));
    assert_eq!(decoded(&doc, bilevel_out), packed, "bilevel: want 1-bit with the same pixels");
    let mask_out = by_name["softmask"];
    assert_eq!(int(&mask_out.dict, b"BitsPerComponent"), Some(8), "soft masks must stay 8-bit");
    assert_eq!(decoded(&doc, mask_out), mask);
    let masked_out = by_name["softmasked"];
    assert_eq!(int(&masked_out.dict, b"BitsPerComponent"), Some(1));
    assert!(masked_out.dict.has(b"SMask"));
    assert_eq!(by_name["photo"].content, photo, "photo bytes changed");
    assert_eq!(by_name["mask"].content, [0xAA; 8]);
    assert_eq!(by_name["indexed"].content, [0u8, 1].repeat(128));
    assert!(gray_out.content.len() + bilevel_out.content.len() < deflate(&rgb).len() + deflate(&bilevel).len(), "re-encoded images are not smaller");
}

#[test]
fn ghostscript_image_flags() {
    if !have("gs") {
        return;
    }
    // 300x300 color noise drawn at 72x72 pt: 300 dpi, incompressible, nowhere gray.
    let rgb: Vec<u8> = (0..300u32 * 300).flat_map(|i| [(i.wrapping_mul(2654435761) >> 24) as u8, 0, (i.wrapping_mul(40503) >> 12) as u8]).collect();
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>"),
            content("", b"q 72 0 0 72 14 14 cm /Im Do Q"),
            stream(
                "/Type /XObject /Subtype /Image /Width 300 /Height 300 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode",
                &deflate(&rgb),
            ),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &pdf);
    // The printer preset keeps 300 dpi and the colors on its own; the flags must override it.
    let run = cli(&args(&[&"--engine", &"ghostscript", &"--quality", &"printer", &"--dpi", &"72", &"--gray", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let found = images(&doc);
    assert_eq!(found.len(), 1);
    let image = found[0].1;
    assert_eq!(name(&image.dict, b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert!(int(&image.dict, b"Width").is_some_and(|w| w <= 100), "image was not downsampled");
}

#[test]
fn images_flag_needs_native_engine() {
    for list in [
        &["--images", "--engine", "ghostscript", "in.pdf"][..],
        &["--dpi", "72", "in.pdf"],
        &["--gray", "in.pdf"],
        &["--engine", "ghostscript", "--dpi", "-1", "in.pdf"],
    ] {
        assert!(!cli(list).ok, "accepted {list:?}");
    }
}

#[test]
fn per_input_destinations_preserve_argument_order() {
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n);
    let (first, second, out1, out2) = (p("z.pdf"), p("a.pdf"), p("first.pdf"), p("second.pdf"));
    write(&first, &test_pdf_with_text(64000, "First file text!!"));
    write(&second, &test_pdf_with_text(64000, "Other file text!!"));
    let run = cli(&args(&[&first, &second, &"-o", &out1, &"-o", &out2]));
    assert!(run.ok, "{}", run.stderr);
    for (path, text) in [(&out1, "First file text!!"), (&out2, "Other file text!!")] {
        assert_eq!(streams_containing(&read_doc(path, ""), text.as_bytes()), 1, "wrong input at {}", path.display());
    }
    let fresh = p("fresh.pdf");
    for inputs in [vec![first.clone(), first.clone()], vec![first.clone()], vec![dir.path().to_path_buf(), second.clone()]] {
        let mut list: Vec<OsString> = inputs.into_iter().map(Into::into).collect();
        list.extend(["-o".into(), fresh.clone().into(), "-o".into(), p("other-fresh.pdf").into()]);
        assert!(!cli(&list).ok, "accepted mismatched destinations");
        assert_missing(&fresh);
    }
    assert!(!cli(&args(&[&first, &second, &"-o", &fresh, &"-o", &out2])).ok, "accepted existing destination");
    assert_missing(&fresh);
}

#[test]
fn timestamps_modified_updates_dates_and_drops_xmp() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
    write(&input, &metadata_pdf());
    let run = cli(&args(&[&"--timestamps", &"modified", &"-o", &output, &input]));
    assert!(run.ok, "{}", run.stderr);
    let doc = read_doc(&output, "");
    let info = dict(&doc, doc.trailer.get(b"Info").ok());
    let created = text(&doc, info.get(b"CreationDate").ok());
    let modified = text(&doc, info.get(b"ModDate").ok());
    assert_eq!(created, "D:20260101120000Z");
    assert!(modified != created && modified.starts_with("D:"), "dates: {created} {modified}");
    assert!(!catalog(&doc).has(b"Metadata"), "stale XMP remains");
}

#[test]
fn independent_color_reduction() {
    let data: Vec<u8> = [0u8, 0, 0, 255, 255, 255].repeat(2048);
    for (mode, want, bits) in [("exact", b"DeviceGray".as_slice(), 1), ("preserve", b"DeviceRGB", 8)] {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.pdf"), dir.path().join("out.pdf"));
        write(&input, &image_pdf(8, "DeviceRGB", "", &data, &[]));
        let run = cli(&args(&[&"--images", &"--force-recompression", &"--color-reduction", &mode, &"-o", &output, &input]));
        assert!(run.ok, "{}", run.stderr);
        let doc = read_doc(&output, "");
        let image = first_image(&doc);
        assert_eq!(name(&image.dict, b"ColorSpace"), Some(want), "{mode}");
        assert_eq!(int(&image.dict, b"BitsPerComponent"), Some(bits), "{mode}");
    }
}

#[test]
fn compression_tiers_preserve_document_features() {
    if !integration() || !have_python_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("features.pdf");
    let create = Command::new(python())
        .arg("-c")
        .arg(
            r#"import sys,pymupdf,pikepdf
with pymupdf.open() as doc:
 page=doc.new_page(width=200,height=200)
 page.insert_text((10,30),"Retain searchable text")
 shape=page.new_shape();shape.draw_rect((10,50,60,80));shape.finish(color=(1,0,0),fill=(0,0,1));shape.commit()
 page.insert_link({"kind":pymupdf.LINK_URI,"from":pymupdf.Rect(10,10,80,35),"uri":"https://example.invalid/"})
 widget=pymupdf.Widget();widget.field_name="example";widget.field_value="Keep me";widget.field_type=pymupdf.PDF_WIDGET_TYPE_TEXT;widget.rect=pymupdf.Rect(10,100,180,130);page.add_widget(widget)
 doc.save(sys.argv[1])
with pikepdf.open(sys.argv[1],allow_overwriting_input=True) as doc:
 doc.attachments["note.txt"]=pikepdf.AttachedFileSpec(doc,b"keep attachment",filename="note.txt")
 doc.save(sys.argv[1])
"#,
        )
        .arg(&input)
        .output()
        .unwrap();
    assert!(create.status.success(), "create fixture: {}", String::from_utf8_lossy(&create.stderr));
    for level in ["light", "balanced", "medium", "strong", "heavy"] {
        let output = dir.path().join(format!("{level}.pdf"));
        let run = cli(&args(&[&"--compression", &level, &"--force-recompression", &"-o", &output, &input]));
        assert!(run.ok, "{level}: {}", run.stderr);
        let check = Command::new(python())
            .arg("-c")
            .arg(
                r#"import sys,pymupdf,pikepdf
with pymupdf.open(sys.argv[1]) as doc:
 assert len(doc)==1
 page=doc[0]
 assert "Retain searchable text" in page.get_text()
 assert len(page.get_drawings())>=1
 assert page.get_links()[0]["uri"]=="https://example.invalid/"
 assert next(page.widgets()).field_value=="Keep me"
with pikepdf.open(sys.argv[1]) as doc:
 assert doc.attachments["note.txt"].get_file().read_bytes()==b"keep attachment"
"#,
            )
            .arg(&output)
            .output()
            .unwrap();
        assert!(check.status.success(), "{level} lost features: {}", String::from_utf8_lossy(&check.stderr));
    }
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn signal(pid: u32, sig: i32) {
    // SAFETY: kill only sends a signal to the CLI under test.
    unsafe { libc::kill(pid as i32, sig) };
}

#[test]
fn hangup_stops_tools_and_removes_staging() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (input, pid_file) = (dir.path().join("input.pdf"), tools.path().join("pid"));
    write(&input, &test_pdf(0));
    let python = tools.path().join("python");
    stub(&python, "#!/bin/sh\necho $$ > \"$TEST_PID\"\nexec sleep 60\n");
    let mut child = Command::new(binary())
        .args([OsStr::new("--gray"), OsStr::new("-o"), dir.path().join("output.pdf").as_os_str(), input.as_os_str()])
        .env("PDF_SQUEEZER_PYTHON", &python)
        .env("TEST_PID", &pid_file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
            break pid;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the tool did not start");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    signal(child.id(), libc::SIGHUP);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the CLI kept running after SIGHUP");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(!pid_alive(pid), "the tool survived the hangup");
    assert_no_temps(dir.path());
}

#[test]
fn external_tools_use_input_snapshot() {
    for mode in ["encrypted", "inline", "text"] {
        let binary_name = if mode == "text" {
            if !integration() || !have_python_tools() {
                continue;
            }
            python()
        } else {
            OsString::from("qpdf")
        };
        if !have(&binary_name.to_string_lossy()) {
            continue;
        }
        let real = pdf_squeezer::process::look_path(&binary_name).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("input.pdf"), dir.path().join("output"));
        let mut list: Vec<OsString> = vec!["--extract".into(), "images".into(), "--inline-images".into()];
        if mode == "encrypted" {
            let (plain, owner) = (dir.path().join("plain.pdf"), dir.path().join("owner.txt"));
            write(&plain, &test_pdf(0));
            write(&owner, b"example-owner\n");
            assert!(cli(&args(&[&"--encrypt-owner-file", &owner, &"-o", &input, &plain])).ok);
            list = vec!["--password-file".into(), owner.into()];
        } else {
            write(&input, &test_pdf(0));
            if mode == "text" {
                list = vec!["--extract".into(), "text".into()];
            }
        }
        let wrapper = dir.path().join("qpdf");
        stub(
            &wrapper,
            "#!/bin/sh\nfor arg do\n  case \"$arg\" in \"$TEST_INPUT\"|\"--copy-encryption=$TEST_INPUT\") exit 91 ;; esac\ndone\nexec \"$TEST_TOOL\" \"$@\"\n",
        );
        let mut search = dir.path().as_os_str().to_os_string();
        search.push(":");
        search.push(std::env::var_os("PATH").unwrap_or_default());
        let mut env: Vec<(&str, &OsStr)> = vec![("TEST_INPUT", input.as_os_str()), ("TEST_TOOL", real.as_os_str()), ("PATH", &search)];
        if mode == "text" {
            env.push(("PDF_SQUEEZER_PYTHON", wrapper.as_os_str()));
        }
        list.extend(["-o".into(), output.clone().into(), input.clone().into()]);
        let run = cli_env(&list, &env);
        assert!(run.ok, "{mode}: an external tool reopened the input path: {}", run.stderr);
        assert_no_temps(dir.path());
    }
}

#[test]
fn tools_keep_scratch_files_in_staging() {
    for mode in ["ghostscript", "python"] {
        let (binary_name, mode_args): (OsString, &[&str]) = if mode == "python" {
            if !integration() || !have_python_tools() {
                continue;
            }
            (python(), &["--extract", "text"])
        } else {
            ("gs".into(), &["--engine", "ghostscript"])
        };
        if !have(&binary_name.to_string_lossy()) {
            continue;
        }
        let real = pdf_squeezer::process::look_path(&binary_name).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let (input, output, record) = (dir.path().join("input.pdf"), dir.path().join("output"), tools.path().join("record"));
        write(&input, &test_pdf(64000));
        // Ghostscript must read the staged path, not a pipe it would spool to a temporary copy.
        let wrapper = tools.path().join("gs");
        stub(
            &wrapper,
            "#!/bin/sh\nif [ \"$TEST_MODE\" = ghostscript ] && [ -n \"$(head -c 1)\" ]; then exit 92; fi\nprintf '%s\\n' \"$TMPDIR\" > \"$TEST_RECORD\"\nexec \"$TEST_TOOL\" \"$@\"\n",
        );
        let mut search = tools.path().as_os_str().to_os_string();
        search.push(":");
        search.push(std::env::var_os("PATH").unwrap_or_default());
        let mut env: Vec<(&str, &OsStr)> =
            vec![("TEST_MODE", OsStr::new(mode)), ("TEST_RECORD", record.as_os_str()), ("TEST_TOOL", real.as_os_str()), ("PATH", &search)];
        if mode == "python" {
            env.push(("PDF_SQUEEZER_PYTHON", wrapper.as_os_str()));
        }
        let run = cli_env(&plus(mode_args, &[&"-o", &output, &input]), &env);
        assert!(run.ok, "{mode}: {}", run.stderr);
        let scratch = fs::read_to_string(&record).unwrap();
        assert!(
            scratch.trim().starts_with(&dir.path().join(".pdf-squeezer-").to_string_lossy().into_owned()),
            "{mode} scratch directory {scratch:?} is outside staging"
        );
        assert_no_temps(dir.path());
    }
}

#[test]
fn finished_stages_are_removed_early() {
    if !have("qpdf") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (input, output, record) = (dir.path().join("input.pdf"), dir.path().join("output.pdf"), tools.path().join("record"));
    write(&input, &test_pdf(64000));
    // A Python stand-in that copies its input and counts stages; its arguments are -I script operation input output.
    let python = tools.path().join("python");
    stub(&python, "#!/bin/sh\nprintf '%s %s\\n' \"$3\" \"$(ls \"$(dirname \"$5\")\" | grep -c '^stage-')\" >> \"$TEST_RECORD\"\ncp \"$4\" \"$5\"\n");
    let run = cli_env(
        &args(&[&"--inline-images", &"--mono-codecs", &"ccitt", &"-o", &output, &input]),
        &[("TEST_RECORD", record.as_os_str()), ("PDF_SQUEEZER_PYTHON", python.as_os_str())],
    );
    assert!(run.ok, "{}", run.stderr);
    // Each pass sees the input snapshot, its own input, and its empty output.
    assert_eq!(fs::read_to_string(&record).unwrap(), "transform 3\nmonochrome 3\n");
    assert_no_temps(dir.path());
}

#[test]
fn canceled_jobs_remove_staging_files() {
    let dir = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    for i in 0..3 {
        write(&dir.path().join(format!("{i}.pdf")), &test_pdf(0));
    }
    let python = tools.path().join("python");
    stub(&python, "#!/bin/sh\nexec sleep 60\n");
    let start = Instant::now();
    let child = Command::new(binary())
        .args([OsStr::new("--jobs"), OsStr::new("2"), OsStr::new("--gray"), OsStr::new("-o"), output.path().as_os_str(), dir.path().as_os_str()])
        .env("PDF_SQUEEZER_PYTHON", &python)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    signal(child.id(), libc::SIGINT);
    let out = child.wait_with_output().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10), "cancellation waited {:?} for the tools", start.elapsed());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert_eq!(stderr.matches("context canceled").count(), 1, "want one cancellation error: {stderr}");
    assert_no_temps(output.path());
}

#[test]
fn jobs_overlap_within_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    for i in 0..4 {
        write(&dir.path().join(format!("{i}.pdf")), &test_pdf(0));
    }
    // A Python stand-in that records how many calls run at once. The first call
    // stays active until a second one has started, so sequential processing fails.
    let python = tools.path().join("python");
    stub(
        &python,
        r#"#!/bin/sh
mkdir "$TEST_DIR/active.$$"
echo $(( $(ls -d "$TEST_DIR"/active.* | wc -l) )) >> "$TEST_DIR/counts"
if mkdir "$TEST_DIR/first" 2>/dev/null; then
	for _ in $(seq 50); do
		[ $(( $(wc -l < "$TEST_DIR/counts") )) -ge 2 ] && break
		sleep 0.1
	done
fi
cp "$4" "$5"
rmdir "$TEST_DIR/active.$$"
"#,
    );
    let run = cli_env(
        &args(&[&"--jobs", &"2", &"--gray", &"-o", &output.path(), &dir.path()]),
        &[("PDF_SQUEEZER_PYTHON", python.as_os_str()), ("TEST_DIR", tools.path().as_os_str())],
    );
    assert!(run.ok, "{}", run.stderr);
    let most = fs::read_to_string(tools.path().join("counts")).unwrap().split_whitespace().map(|n| n.parse::<u32>().unwrap()).max().unwrap();
    assert_eq!(most, 2, "{most} tool calls ran at once with --jobs 2");
}

#[test]
fn non_utf8_directories_reach_python_and_java() {
    if !integration() || !have_python_tools() || !have("qpdf") || !have("verapdf") {
        return;
    }
    use std::os::unix::ffi::OsStrExt;
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join(OsStr::from_bytes(b"M\xe4rz"));
    if fs::create_dir(&dir).is_err() {
        return;
    }
    let aliases = tempfile::tempdir().unwrap();
    let (input, output) = (dir.join("in.pdf"), dir.join("out.pdf"));
    write(&input, &test_pdf(0));
    let run = cli_env(&args(&[&"--pdfa", &"4", &"-o", &output, &input]), &[("TMPDIR", aliases.path().as_os_str())]);
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(flavour(&output), "4");
    assert_no_temps(&dir);
    assert_eq!(fs::read_dir(aliases.path()).unwrap().count(), 0, "staging aliases remain");
}
