mod common;

use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use lopdf::{Dictionary, Object};
use pdf_squeezer::error::Ctx;
use pdf_squeezer::images::{ImageStats, optimize_images};
use pdf_squeezer::options::{Options, expand_inputs, parse_options};
use pdf_squeezer::process::Out;

fn sink() -> Out {
    Arc::new(Mutex::new(std::io::sink()))
}

fn osv(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn parse(items: &[&str]) -> pdf_squeezer::error::Result<Options> {
    parse_options(&Ctx::background(), &osv(items), &sink()).map(|p| p.opts)
}

fn optimize(pdf: &[u8], opts: &Options) -> (lopdf::Document, ImageStats, usize) {
    let mut doc = pdf_squeezer::pdf::load(pdf, "").unwrap();
    let jobs = Mutex::new(0);
    let stats = optimize_images(&Ctx::background(), &mut doc, opts, &|_| *jobs.lock().unwrap() += 1).unwrap();
    (doc, stats, jobs.into_inner().unwrap())
}

fn opts(codecs: &str, force: bool) -> Options {
    Options { image_memory: 512, image_quality: 75, image_codecs: codecs.into(), force, ..Options::default() }
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: mkfifo only creates a named pipe at a path in a test directory.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

#[test]
fn canceled_run() {
    let ctx = Ctx::background();
    ctx.cancel();
    let err = pdf_squeezer::run(&ctx, &osv(&["missing.pdf"]), &sink(), &sink()).unwrap_err();
    assert!(err.is_canceled(), "unexpected error: {err}");
}

#[test]
fn expand_inputs_skips_hidden_directories() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["visible.pdf", ".file.pdf", ".hidden/inside.pdf", ".hidden/.nested/skipped.pdf", "child/visible.pdf", "child/.nested/skipped.pdf"] {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write(&path, b"");
    }
    let root = dir.path();
    for (inputs, recursive, want) in [
        (vec![root.to_path_buf()], true, vec!["child/visible.pdf", "visible.pdf"]),
        (vec![root.to_path_buf()], false, vec!["visible.pdf"]),
        (vec![root.join(".hidden")], true, vec![".hidden/inside.pdf"]),
        (vec![root.join(".file.pdf")], true, vec![".file.pdf"]),
    ] {
        let inputs: Vec<OsString> = inputs.into_iter().map(Into::into).collect();
        let files = expand_inputs(&Ctx::background(), &inputs, recursive).unwrap();
        let canonical_root = fs::canonicalize(root).unwrap();
        let got: Vec<String> = files
            .iter()
            .map(|f| {
                let path = fs::canonicalize(&f.path).unwrap();
                path.strip_prefix(&canonical_root).unwrap().to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(got, want);
    }
}

#[test]
fn expand_inputs_follows_named_symlink_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (real, link) = (dir.path().join("real"), dir.path().join("link"));
    for name in ["a.pdf", "child/b.pdf"] {
        let path = real.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write(&path, b"");
    }
    std::os::unix::fs::symlink(&real, &link).unwrap();
    for (recursive, want) in [(false, "a.pdf"), (true, "a.pdf child/b.pdf")] {
        let files = expand_inputs(&Ctx::background(), &[link.clone().into()], recursive).unwrap();
        for f in &files {
            assert_eq!(f.path, link.join(&f.relative), "{} is not reported under the named directory", f.path.display());
        }
        let got: Vec<String> = files.iter().map(|f| f.relative.to_string_lossy().into_owned()).collect();
        assert_eq!(got.join(" "), want);
    }
}

#[test]
fn expand_inputs_skips_special_files() {
    let dir = tempfile::tempdir().unwrap();
    let pipe = dir.path().join("pipe.pdf");
    mkfifo(&pipe);
    let link = dir.path().join("link.pdf");
    std::os::unix::fs::symlink(&pipe, &link).unwrap();
    let regular = dir.path().join("document.pdf");
    write(&regular, &test_pdf(0));
    for input in [&pipe, &link] {
        let err = expand_inputs(&Ctx::background(), &[input.clone().into()], false).unwrap_err();
        assert!(err.to_string().contains("regular PDF file"), "{err}");
    }
    for recursive in [false, true] {
        let files = expand_inputs(&Ctx::background(), &[dir.path().into()], recursive).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(fs::canonicalize(&files[0].path).unwrap(), fs::canonicalize(&regular).unwrap());
    }
}

#[test]
fn prepared_input_snapshot() {
    for declared in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("input.pdf"), dir.path().join("output.pdf"));
        let original = if declared { pdfa_pdf("2", "B") } else { test_pdf(0) };
        write(&input, &original);
        let opts = parse(&[input.to_str().unwrap()]).unwrap();
        let staged = pdf_squeezer::run::prepare(&Ctx::background(), &input, &output, &opts, &sink()).unwrap();
        write(&input, b"replacement");
        assert_ne!(staged.original, input);
        assert_eq!(read_file(&staged.original), original, "input snapshot was lost or changed");
        if !declared {
            assert_eq!(staged.path, staged.original, "no-gain fallback did not reuse the input snapshot");
        }
        staged.cleanup();
    }
}

#[test]
fn replaced_inputs_do_not_block() {
    for mode in ["compress", "images", "text"] {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("input.pdf"), dir.path().join("output"));
        write(&input, &test_pdf(0));
        let files = expand_inputs(&Ctx::background(), &[dir.path().into()], false).unwrap();
        assert_eq!(files.len(), 1);
        fs::remove_file(&input).unwrap();
        mkfifo(&input);
        let path = files[0].path.clone();
        let out = output.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = if mode == "compress" {
                pdf_squeezer::run::prepare(&Ctx::background(), &path, &out, &Options::default(), &sink()).map(|s| s.cleanup())
            } else {
                let opts = Options { extract: mode.into(), ..Options::default() };
                pdf_squeezer::extract::extract(&Ctx::background(), &path, &out, &opts, &sink())
            };
            let _ = tx.send(result);
        });
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("opening the replaced input blocked");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("regular PDF file"), "{mode}: {err}");
        assert_missing(&output);
        assert_no_temps(dir.path());
    }
}

#[test]
fn image_budget_cancellation_and_decode() {
    let samples: Vec<u8> = [0u8, 255, 128, 64].repeat(1024);
    let lossless = Options { lossless: true, ..opts("flate", true) };
    let (doc, stats, _) = optimize(&image_pdf(8, "DeviceGray", "/Decode [1 0]", &samples, &[]), &lossless);
    assert_eq!(stats.changed, 1, "{stats}");
    let image = first_image(&doc);
    assert!(!image.dict.has(b"Decode"), "normalized Decode remains");
    let inverted: Vec<u8> = samples.iter().map(|v| !v).collect();
    assert_eq!(decoded(&doc, image), inverted, "inversion changed appearance");
    let ctx = Ctx::background();
    ctx.cancel();
    let mut doc = pdf_squeezer::pdf::load(&image_pdf(8, "DeviceGray", "", &samples, &[]), "").unwrap();
    assert!(optimize_images(&ctx, &mut doc, &lossless, &|_| {}).unwrap_err().is_canceled());
    let tiny = Options { image_memory: 1, ..lossless };
    let (_, stats, _) = optimize(&image_pdf(8, "DeviceGray", "", &samples, &[]), &tiny);
    assert_eq!(stats.preserved.get("memory budget"), Some(&1), "budget not enforced: {stats}");
    assert_eq!(stats.changed, 0);
}

#[test]
fn wide_image_budget_counts_filter_rows() {
    // One 2 MiB row: the samples fit 16 MiB, but six PNG filter rows add 12 MiB more.
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R >> >> /Contents 4 0 R >>"),
            content("", b"q 64 0 0 64 0 0 cm /A Do Q"),
            stream("/Type /XObject /Subtype /Image /Width 2097152 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[7u8; 4096]),
        ],
    );
    let (_, stats, jobs) = optimize(&pdf, &Options { image_memory: 16, lossless: true, ..opts("flate", true) });
    assert_eq!(stats.preserved.get("memory budget"), Some(&1), "{stats}");
    assert_eq!(jobs, 0);
}

fn decode_jpeg(data: &[u8], space: zune_core::colorspace::ColorSpace) -> Vec<u8> {
    let options = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(space);
    zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(data), options).decode().unwrap()
}

#[test]
fn reencode_gray_jpeg() {
    for white in [false, true] {
        let pixels: Vec<u8> = (0..65 * 65).map(|i| if white { 255 } else { ((i % 65 + i / 65) * 2) as u8 }).collect();
        let mut raw = Vec::new();
        jpeg_encoder::Encoder::new(&mut raw, 95).encode(&pixels, 65, 65, jpeg_encoder::ColorType::Luma).unwrap();
        let want = decode_jpeg(&raw, zune_core::colorspace::ColorSpace::Luma);
        let pdf = image_pdf(8, "DeviceGray", "/Filter /DCTDecode", &raw, &[]);
        let text = String::from_utf8_lossy(&pdf).into_owned();
        assert!(text.contains("/Width 64 /Height 64"));
        let at = pdf.windows(20).position(|w| w == b"/Width 64 /Height 64").unwrap();
        let mut pdf = pdf;
        pdf[at..at + 20].copy_from_slice(b"/Width 65 /Height 65");
        let (doc, stats, _) = optimize(&pdf, &opts("flate", true));
        assert_eq!(stats.changed, 1, "{stats}");
        let image = first_image(&doc);
        let bits = int(&image.dict, b"BitsPerComponent").unwrap();
        assert_eq!(bits, if white { 1 } else { 8 });
        let data = decoded(&doc, image);
        for y in 0..65 {
            for x in 0..65 {
                let got = if bits == 1 { if data[y * 9 + x / 8] & (0x80 >> (x % 8)) != 0 { 255 } else { 0 } } else { data[y * 65 + x] };
                assert_eq!(got, want[y * 65 + x], "pixel ({x},{y})");
            }
        }
    }
}

#[test]
fn jpeg_color_transform_preservation() {
    let pixels: Vec<u8> = [30u8, 80, 140].repeat(64 * 64);
    let mut raw = Vec::new();
    jpeg_encoder::Encoder::new(&mut raw, 75).encode(&pixels, 64, 64, jpeg_encoder::ColorType::Rgb).unwrap();
    let want = decode_jpeg(&raw, zune_core::colorspace::ColorSpace::RGB);
    for transform in [0, 1] {
        for form in [
            "direct",
            "indirect dictionary",
            "indirect value",
            "array",
            "indirect array",
            "indirect array dictionary",
            "absent",
            "empty",
            "empty array",
            "null array",
        ] {
            let parms = format!("<< /ColorTransform {transform} >>");
            let mut extra = "/Filter /DCTDecode /DecodeParms ".to_string();
            let mut more = Vec::new();
            let mut preserved = true;
            match form {
                "direct" => extra.push_str(&parms),
                "indirect dictionary" => {
                    extra.push_str("6 0 R");
                    more.push(obj(&parms));
                }
                "indirect value" => {
                    extra.push_str("<< /ColorTransform 6 0 R >>");
                    more.push(obj(&transform.to_string()));
                }
                "array" => extra = format!("/Filter [/DCTDecode] /DecodeParms [{parms}]"),
                "indirect array" => {
                    extra = "/Filter [/DCTDecode] /DecodeParms 6 0 R".into();
                    more.push(obj(&format!("[{parms}]")));
                }
                "indirect array dictionary" => {
                    extra = "/Filter [/DCTDecode] /DecodeParms [6 0 R]".into();
                    more.push(obj(&parms));
                }
                "absent" => (extra, preserved) = ("/Filter /DCTDecode".into(), false),
                "empty" => (extra, preserved) = (extra + "<< >>", false),
                "empty array" => (extra, preserved) = (extra + "[]", false),
                _ => (extra, preserved) = (extra + "[null]", false),
            }
            let pdf = image_pdf(8, "DeviceRGB", &extra, &raw, &more);
            let before = pdf_squeezer::pdf::load(&pdf, "").unwrap();
            let before_dict = first_image(&before).dict.clone();
            let (doc, stats, _) = optimize(&pdf, &opts("flate", true));
            let image = first_image(&doc);
            if preserved {
                assert_eq!(stats.changed, 0, "{form}: {stats}");
                assert_eq!(stats.preserved.get("jpeg color transform"), Some(&1), "{form}: {stats}");
                assert_eq!(image.content, raw);
                assert_eq!(image.dict, before_dict);
            } else {
                assert_eq!(stats.changed, 1, "{form}: {stats}");
                assert_eq!(name(&image.dict, b"Filter"), Some(b"FlateDecode".as_slice()));
                assert_eq!(decoded(&doc, image), want, "{form}: default transform changed samples");
            }
        }
    }
}

#[test]
fn images_skip_unprofitable_streams() {
    let samples = vec![128u8; 64 * 64];
    let raw = deflate(&samples);
    assert!(raw.len() < pdf_squeezer::images::MIN_IMAGE_SAVINGS);
    for force in [false, true] {
        let (doc, stats, jobs) = optimize(&image_pdf(8, "DeviceGray", "/Filter /FlateDecode", &raw, &[]), &opts("flate", force));
        let image = first_image(&doc);
        if force {
            assert!(stats.changed == 1 && jobs == 1, "forced encoding was skipped: {stats}, jobs {jobs}");
        } else {
            assert!(stats.changed == 0 && stats.preserved.get("no gain") == Some(&1) && jobs == 0, "{stats}, jobs {jobs}");
            assert_eq!(image.content, raw);
        }
        assert_eq!(decoded(&doc, image), samples);
    }
}

#[test]
fn duplicate_mask_encoding_work() {
    let mask: Vec<u8> = [0x7fu8, 0x81].repeat(4096);
    let body = vec![30u8; 4096];
    let mask_object = stream("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 16", &mask);
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 128 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>"),
            content("", b"q 64 0 0 64 0 0 cm /A Do Q q 64 0 0 64 64 0 cm /B Do Q"),
            stream("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 7 0 R", &body),
            stream("/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 8 0 R", &body),
            mask_object.clone(),
            mask_object,
        ],
    );
    let o = Options { reduce_bits: true, ..opts("flate", true) };
    let (doc, stats, jobs) = optimize(&pdf, &o);
    assert!(stats.seen == 4 && stats.changed == 4 && jobs == 3, "mask work: {stats}, jobs={jobs}");
    for id in [7, 8] {
        let Some(Object::Stream(m)) = doc.objects.get(&(id, 0)) else { panic!("mask {id} missing") };
        assert_eq!(int(&m.dict, b"BitsPerComponent"), Some(16));
        assert_eq!(decoded(&doc, m), mask, "shared mask changed samples or precision");
    }
    for (id, want) in [(5, 7), (6, 8)] {
        let Some(Object::Stream(s)) = doc.objects.get(&(id, 0)) else { panic!() };
        assert_eq!(s.dict.get(b"SMask").unwrap().as_reference().unwrap(), (want, 0), "mask references changed before final deduplication");
    }
}

#[test]
fn stream_deduplication_raw_alias_chain() {
    let data = b"q Q\n".repeat(1024);
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Contents [4 0 R 5 0 R 6 0 R] >>"),
            content("", &data),
            stream("/Filter /FlateDecode", &deflate(&data)),
            content("", &data),
        ],
    );
    let mut doc = pdf_squeezer::pdf::load(&pdf, "").unwrap();
    pdf_squeezer::document::deduplicate_streams(&Ctx::background(), &mut doc, true, 512).unwrap();
    let Some(Object::Dictionary(page)) = doc.objects.get(&(3, 0)) else { panic!() };
    let Ok(Object::Array(contents)) = page.get(b"Contents") else { panic!() };
    let refs: Vec<_> = contents.iter().map(|c| c.as_reference().unwrap()).collect();
    assert!(refs[0] == refs[1] && refs[1] == refs[2], "raw alias chain did not reach the normalized representative: {refs:?}");
}

#[test]
fn heavy_flate_candidates_preserve_samples() {
    for bits in [8u8, 16] {
        let data = vec![0u8; 64 * 64 * bits as usize / 8];
        let o = Options { compression: "heavy".into(), color_reduction: "preserve".into(), ..opts("flate", true) };
        let (doc, stats, _) = optimize(&image_pdf(bits, "DeviceGray", "", &data, &[]), &o);
        assert_eq!(stats.changed, 1, "{stats}");
        let image = first_image(&doc);
        assert_eq!(decoded(&doc, image), data);
        assert_eq!(int(&image.dict, b"BitsPerComponent"), Some(bits as i64));
    }
}

#[test]
fn targeted_document_controls() {
    let mut doc = pdf_squeezer::pdf::load(&metadata_pdf(), "").unwrap();
    let mut spider = Dictionary::new();
    spider.set("V", 1);
    pdf_squeezer::pdf::catalog_mut(&mut doc).unwrap().set("SpiderInfo", spider);
    let o = Options { strip: "web-capture".into(), ..Options::default() };
    pdf_squeezer::document::transform_document(&Ctx::background(), &mut doc, &o).unwrap();
    let root = catalog(&doc);
    assert!(!root.has(b"SpiderInfo"), "web capture remains");
    assert!(root.has(b"Metadata"), "targeted removal discarded XMP");
    let info = dict(&doc, doc.trailer.get(b"Info").ok());
    assert_eq!(text(&doc, info.get(b"Title").ok()), "Secret Report");
}

#[test]
fn password_file_reuse_and_metadata_dates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("password.txt");
    write(&path, b"fictional-example-password\n");
    let p = path.to_str().unwrap();
    assert!(parse(&["--encrypt-user-file", p, "--encrypt-owner-file", p, "input.pdf"]).is_err(), "same file bypassed distinct passwords");
    assert!(parse(&["--metadata", "CreationDate=D:20261301120000Z", "input.pdf"]).is_err(), "invalid calendar date accepted");
    let o = parse(&["--password-file", p, "--encrypt-owner-file", p, "input.pdf"]).unwrap();
    assert_eq!(o.password, "fictional-example-password");
    assert_eq!(o.encrypt_owner, o.password);
}

#[test]
fn tier_image_opt_out_by_profile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disabled.json");
    write(&path, br#"{"version":1,"flags":{"images":"false"}}"#);
    for level in ["light", "balanced", "medium", "strong", "heavy"] {
        let o = parse(&["--compression", level, "in.pdf", "--profile", path.to_str().unwrap()]).unwrap();
        assert!(!o.images && !o.reduce_bits && o.dpi == 0 && !o.clip && !o.inline, "{level}");
    }
}

#[test]
fn profile_export_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (input, baseline) = (dir.path().join("input.json"), dir.path().join("baseline.json"));
    let profile = |path: &Path, name: &str| write(path, serde_json::json!({"version": 1, "name": name, "flags": {}}).to_string().as_bytes());
    let save = |output: &Path, paths: &[&Path]| {
        let mut items = vec!["--save-profile".to_string(), output.to_string_lossy().into_owned()];
        for p in paths {
            items.extend(["--profile".to_string(), p.to_string_lossy().into_owned()]);
        }
        let items: Vec<&str> = items.iter().map(String::as_str).collect();
        parse(&items)
    };
    profile(&input, "x");
    save(&baseline, &[&input]).unwrap();
    let length = 1 + pdf_squeezer::profiles::MAX_PROFILE_BYTES - read_file(&baseline).len();
    profile(&input, &"x".repeat(length));
    let at_limit = dir.path().join("at-limit.json");
    save(&at_limit, &[&input]).unwrap();
    assert_eq!(read_file(&at_limit).len(), pdf_squeezer::profiles::MAX_PROFILE_BYTES, "boundary does not include the final newline");
    pdf_squeezer::profiles::load_profiles(&Ctx::background(), &at_limit, &sink()).unwrap();
    profile(&input, &"x".repeat(length + 1));
    let too_large = dir.path().join("too-large.json");
    assert!(save(&too_large, &[&input]).is_err(), "accepted oversized export");
    assert_missing(&too_large);
    let other = dir.path().join("other.json");
    profile(&input, &"a".repeat(600000));
    profile(&other, &"b".repeat(600000));
    assert!(save(&too_large, &[&input, &other]).is_err(), "accepted oversized bundle");
    assert_missing(&too_large);
}

#[test]
fn profile_bundles_and_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second, saved) = (dir.path().join("first.json"), dir.path().join("second.json"), dir.path().join("bundle.json"));
    write(&first, br#"{"version":1,"name":"Light","flags":{"images":"true","image-quality":"80"}}"#);
    write(&second, br#"{"version":1,"name":"Heavy","flags":{"compression":"heavy","image-quality":"70"},"metadata":["Title=Example"]}"#);
    let (f, s, b) = (first.to_str().unwrap(), second.to_str().unwrap(), saved.to_str().unwrap());
    let o = parse(&["--profile", f, "--profile", s, "--profile-entry", "Heavy", "--image-quality", "88", "--save-profile", b]).unwrap();
    assert!(o.compression == "heavy" && o.image_quality == 88 && o.reduce_bits && o.metadata.len() == 1, "{o:?}");
    let bundle: serde_json::Value = serde_json::from_slice(&read_file(&saved)).unwrap();
    assert_eq!(bundle["version"], 2);
    assert_eq!(bundle["profiles"].as_array().unwrap().len(), 2);
    assert_eq!(bundle["profiles"][0]["flags"]["image-quality"], "80");
    assert_eq!(bundle["profiles"][1]["flags"]["image-quality"], "88");
    for selector in ["2", "Heavy"] {
        let o = parse(&["--profile", b, "--profile-entry", selector, "in.pdf"]).unwrap();
        assert!(o.image_quality == 88 && o.compression == "heavy", "selection {selector}");
    }
    assert_eq!(parse(&["--profile", b, "in.pdf"]).unwrap().image_quality, 80);
    for bad in [
        r#"{"version":2,"profiles":[]}"#.to_string(),
        r#"{"version":2,"profiles":[{"version":1,"flags":{}},{"version":1,"flags":{"password-file":"example.txt"}}]}"#.into(),
        r#"{"version":2,"profiles":[{"version":3}]}"#.into(),
        r#"{"version":1,"flags":{},"unknown":true}"#.into(),
        r#"{"version":1,"flags":{}} {}"#.into(),
        format!(r#"{{"version":1,"flags":{{}}}}{}"#, " ".repeat(1 << 20)),
    ] {
        write(&saved, bad.as_bytes());
        assert!(parse(&["--profile", b, "in.pdf"]).is_err(), "accepted invalid or unsafe profile {}", &bad[..bad.len().min(80)]);
    }
}

fn plist(python_code: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new(python()).args(["-I", "-c", python_code]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

#[test]
fn legacy_property_list_profiles() {
    if !have(&python().to_string_lossy()) {
        return;
    }
    let data = r#"[{"title":"Light","optimizeImages":true,"imageQuality":0.8,"imageResolution":300,"colorConversion":0,"reduceColorComplexity":false,"removeTitle":true,"customTitle":"Example","stripSpiderInfo":true,"updateModificationDate":true},{"title":"Strong","optimizeImages":true,"imageQuality":0.5,"imageResolution":144,"downsampleMonochromeScans":true,"updateModificationDate":true,"updateModificationAndCreationDate":true}]"#;
    for format in ["FMT_XML", "FMT_BINARY"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.pdfscp");
        let encoded =
            plist(&format!("import json,plistlib,sys;sys.stdout.buffer.write(plistlib.dumps(json.loads('{data}'),fmt=plistlib.{format}))")).unwrap();
        write(&path, &encoded);
        let p = path.to_str().unwrap();
        let o = parse(&["--profile", p, "in.pdf"]).unwrap();
        assert!(
            o.image_quality == 80 && o.dpi == 300 && o.timestamps == "modified" && o.color_reduction == "preserve" && o.strip == "web-capture",
            "{o:?}"
        );
        assert_eq!(o.metadata, ["Title=Example"]);
        let o = parse(&["--profile", p, "--profile-entry", "Strong", "--image-quality", "65", "in.pdf"]).unwrap();
        assert!(o.image_quality == 65 && o.dpi == 144 && o.mono_dpi == 144 && o.timestamps == "now", "{o:?}");
    }
}

#[test]
fn legacy_profile_optional_settings() {
    for (settings, tier, images, quality, dpi) in [
        (r#"{"removeAuthor":true,"customAuthor":"Example","stripSpiderInfo":true}"#, "", false, 75, 0),
        (r#"{"optimizeImages":true}"#, "", true, 75, 0),
        (r#"{"imageQuality":0.8}"#, "", false, 80, 0),
        (r#"{"imageResolution":144}"#, "", false, 75, 144),
        (r#"{"removeAuthor":true,"customAuthor":"Example"}"#, "light", true, 90, 0),
    ] {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(settings).unwrap();
        let profile = pdf_squeezer::profiles::legacy_profile(&map).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write(&path, &serde_json::to_vec(&profile).unwrap());
        let mut items = vec!["--profile", path.to_str().unwrap(), "in.pdf"];
        if !tier.is_empty() {
            items.extend(["--compression", tier]);
        }
        let o = parse(&items).unwrap();
        assert!(o.images == images && o.image_quality == quality && o.dpi == dpi, "{settings}: {o:?}");
        if map.contains_key("removeAuthor") {
            assert_eq!(o.metadata, ["Author=Example"]);
        }
    }
}

#[test]
fn legacy_inactive_images_and_binary_trailer() {
    if !have(&python().to_string_lossy()) {
        return;
    }
    let data = plist(
        "import plistlib, sys\nfor n in range(1024):\n data = plistlib.dumps([dict(title='Example', uuid='x'*n, optimizeImages=False, clipImages=True, forceRecompression=True)], fmt=plistlib.FMT_BINARY)\n if data[-1] in b' \\t\\r\\n':\n  sys.stdout.buffer.write(data)\n  break\nelse:\n sys.exit(1)\n",
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inactive.pdfscp");
    write(&path, &data);
    let o = parse(&["--profile", path.to_str().unwrap(), "in.pdf"]).unwrap();
    assert!(!o.images && !o.clip && !o.force, "{o:?}");
    let output = dir.path().join("output.pdfscp");
    assert!(parse(&["--save-profile", output.to_str().unwrap()]).is_err(), "wrote JSON under a .pdfscp extension");
    assert_missing(&output);
}

#[test]
fn indirect_stream_lengths_that_are_wrong() {
    let page = b"BT /F1 12 Tf 20 20 Td (Recovered text) Tj ET";
    for length in ["3", "500", "4 0 R"] {
        // Object 5 holds the length; "4 0 R" makes the stream's length refer to itself.
        let mut stream =
            format!("<< /Length {} >>\nstream\n", if length == "4 0 R" { "4 0 R".to_string() } else { "5 0 R".to_string() }).into_bytes();
        stream.extend_from_slice(page);
        stream.extend_from_slice(b"\nendstream");
        let pdf = build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 50] /Contents 4 0 R >>"),
                stream,
                obj(if length == "4 0 R" { "0" } else { length }),
            ],
        );
        let doc = pdf_squeezer::pdf::load(&pdf, "").unwrap_or_else(|e| panic!("{length}: {e}"));
        let Some(Object::Stream(s)) = doc.objects.get(&(4, 0)) else { panic!("{length}: content stream missing") };
        assert_eq!(s.content, page, "{length}: content was not recovered");
    }
}

#[test]
fn declared_lengths_hold_before_extra_end_of_lines() {
    let file = b"embedded bytes\n";
    for length in [file.len().to_string(), "6 0 R".into()] {
        let mut padded = format!("<< /Type /EmbeddedFile /Length {length} >>\nstream\n").into_bytes();
        padded.extend_from_slice(file);
        padded.extend_from_slice(b"\n\r\nendstream");
        let pdf = build_pdf(
            0,
            &[
                obj("<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(a.txt) 5 0 R] >> >> >>"),
                obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] >>"),
                padded,
                obj("<< /Type /Filespec /F (a.txt) /EF << /F 4 0 R >> >>"),
                obj(&file.len().to_string()),
            ],
        );
        let doc = pdf_squeezer::pdf::load(&pdf, "").unwrap();
        let Some(Object::Stream(s)) = doc.objects.get(&(4, 0)) else { panic!("{length}: embedded file missing") };
        assert_eq!(s.content, file, "{length}");
    }
}

#[test]
fn tiny_flate_images_recompress() {
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R >> >> /Contents 4 0 R >>"),
            content("", b"q 64 0 0 64 0 0 cm /A Do Q"),
            stream(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                &deflate(&[7]),
            ),
        ],
    );
    let (doc, stats, _) = optimize(&pdf, &opts("flate", true));
    assert_eq!(stats.seen, 1, "{stats}");
    assert_eq!(decoded(&doc, first_image(&doc)), [7]);
}

#[test]
fn shifted_objects_load_and_damaged_objects_fail() {
    // A comment after the page tree moves the later objects past their cross-reference offsets.
    let pdf = test_pdf(0);
    let at = pdf.windows(8).position(|w| w == b"4 0 obj\n").unwrap();
    let shifted = [&pdf[..at], b"% inserted comment\n".as_slice(), &pdf[at..]].concat();
    let doc = pdf_squeezer::pdf::load(&shifted, "").unwrap();
    let Some(Object::Stream(page)) = doc.objects.get(&(5, 0)) else { panic!("content stream missing") };
    assert!(contains(&page.content, PAGE_TEXT.as_bytes()));
    // An array left open damages the font, and no rebuild or repair can read it.
    let damaged = String::from_utf8(pdf).unwrap().replace("/BaseFont /Helvetica", "/BaseFont [Helvetica");
    let err = pdf_squeezer::pdf::load(damaged.as_bytes(), "").unwrap_err();
    assert!(err.to_string().contains("object 4 0 is damaged"), "{err}");
}

#[test]
fn junk_where_keys_belong_is_skipped() {
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R ) >>"),
            obj("<< /Type /Font /Subtype /Type1 /BaseFont /Arial,Bold MS /Encoding /WinAnsiEncoding >>"),
            content("", b"BT /F1 12 Tf 20 100 Td (Text) Tj ET\n"),
        ],
    );
    let doc = pdf_squeezer::pdf::load(&pdf, "").unwrap();
    let page = dict(&doc, doc.objects.get(&(3, 0)));
    assert_eq!(page.get(b"Contents").unwrap().as_reference().unwrap(), (5, 0));
    let font = dict(&doc, doc.objects.get(&(4, 0)));
    assert_eq!(name(font, b"BaseFont"), Some(b"Arial,Bold".as_slice()));
    assert_eq!(name(font, b"Encoding"), Some(b"WinAnsiEncoding".as_slice()));
}

#[test]
fn shifted_objects_with_junk_keys_load() {
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"),
            obj("<< /Type /Font /Subtype /Type1 /BaseFont /Arial,Bold MS /Encoding /WinAnsiEncoding >>"),
            content("", b"BT /F1 12 Tf 20 100 Td (Text) Tj ET\n"),
        ],
    );
    // A line that is not an object moves the font and the page text past their offsets.
    let at = pdf.windows(8).position(|w| w == b"4 0 obj\n").unwrap();
    let shifted = [&pdf[..at], b"garbage\n".as_slice(), &pdf[at..]].concat();
    let doc = pdf_squeezer::pdf::load(&shifted, "").unwrap();
    let font = dict(&doc, doc.objects.get(&(4, 0)));
    assert_eq!(name(font, b"BaseFont"), Some(b"Arial,Bold".as_slice()));
    let Some(Object::Stream(page)) = doc.objects.get(&(5, 0)) else { panic!("content stream missing") };
    assert!(contains(&page.content, b"(Text) Tj"));
}

#[test]
fn flate_streams_with_counted_end_of_line_still_deduplicate() {
    let data = b"BT /F1 12 Tf (same page text) Tj ET\n".repeat(64);
    let mut padded = deflate(&data);
    // This writer counted the end-of-line before endstream in /Length.
    padded.push(b'\n');
    let pdf = build_pdf(
        0,
        &[
            obj("<< /Type /Catalog /Pages 2 0 R >>"),
            obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Contents [4 0 R 5 0 R] >>"),
            stream("/Filter /FlateDecode", &padded),
            content("", &data),
        ],
    );
    let mut doc = pdf_squeezer::pdf::load(&pdf, "").unwrap();
    pdf_squeezer::document::deduplicate_streams(&Ctx::background(), &mut doc, true, 512).unwrap();
    let Some(Object::Dictionary(page)) = doc.objects.get(&(3, 0)) else { panic!() };
    let Ok(Object::Array(contents)) = page.get(b"Contents") else { panic!() };
    assert_eq!(contents[0], contents[1], "equal content streams were not merged");
}

#[test]
fn latin1_password_files_reach_qpdf_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("password.txt");
    write(&path, b"caf\xe9\x80\n");
    let o = parse(&["--password-file", path.to_str().unwrap(), "input.pdf"]).unwrap();
    assert_eq!(o.password_bytes, b"caf\xe9\x80");
    // PDFDocEncoding: 0xE9 is é and 0x80 is a bullet.
    assert_eq!(o.password, "café\u{2022}");
    let owner = dir.path().join("owner.txt");
    write(&owner, b"\xff\n");
    assert!(parse(&["--encrypt-owner-file", owner.to_str().unwrap(), "input.pdf"]).is_err(), "AES-256 needs UTF-8 passwords");
}

#[test]
fn run_finishes_quickly_on_cancel() {
    let ctx = Ctx::background();
    let start = Instant::now();
    ctx.cancel();
    assert!(pdf_squeezer::run(&ctx, &osv(&["-V", "x.pdf"]), &sink(), &sink()).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
}
