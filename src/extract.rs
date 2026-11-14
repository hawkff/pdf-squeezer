//! --extract: image files named by object number, or text through the Python tools.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::bail;
use crate::error::{Ctx, Error, Result, path_error};
use crate::files::{Staging, check_pdf, copy_ctx, open_input, publish_file, read_all, stage_path, write_new};
use crate::filters::{self, Filter};
use crate::image_encoding::{EncodeError, MAX_IMAGE_BYTES, PngKind, png_idat};
use crate::images::image_stream;
use crate::options::Options;
use crate::pdf::{self, deref, dict_of, get, number, page_ids};
use crate::process::Out;
use crate::tools::{python_pdf, qpdf};

pub fn extract(ctx: &Ctx, input: &Path, output: &Path, opts: &Options, stderr: &Out) -> Result<()> {
    let (mut file, _) = open_input(input)?;
    check_pdf(&file)?;
    let staging = Staging::create(output)?;
    let result = extract_in(ctx, &mut file, &staging.path, output, opts, stderr);
    staging.remove();
    result
}

fn extract_in(ctx: &Ctx, file: &mut fs::File, work: &Path, output: &Path, opts: &Options, stderr: &Out) -> Result<()> {
    // Every tool works on a private snapshot of the opened input, never its path.
    let mut data = None;
    let mut snapshot = None;
    if opts.extract == "text" || opts.inline {
        let path = stage_path(work)?;
        let mut stage = fs::OpenOptions::new().write(true).truncate(true).open(&path).map_err(|e| path_error("open", &path, &e))?;
        copy_ctx(ctx, file, &mut stage)?;
        snapshot = Some(path);
    }
    if opts.extract == "text" {
        let staged = work.join("text.txt");
        python_pdf(ctx, "text", snapshot.as_deref().unwrap(), &staged, opts, stderr)?;
        publish_file(ctx, &staged, output)?;
        return Ok(());
    }
    if let Some(snapshot) = &snapshot {
        let staged = work.join("inline.pdf");
        let args =
            ["--externalize-inline-images".into(), "--ii-min-bytes=0".into(), snapshot.as_os_str().to_os_string(), staged.as_os_str().to_os_string()];
        qpdf(ctx, &args, &opts.password_bytes, stderr)?;
        data = Some(fs::read(&staged).map_err(|e| path_error("open", &staged, &e))?);
    }
    let data = match data {
        Some(d) => d,
        None => read_all(ctx, file)?,
    };
    let doc = pdf::load(&data, &opts.password)?;
    drop(data);
    let images_dir = work.join("images");
    fs::DirBuilder::new().mode(0o700).create(&images_dir).map_err(|e| path_error("mkdir", &images_dir, &e))?;
    let mut seen = HashSet::new();
    for (index, page) in page_ids(&doc)?.into_iter().enumerate() {
        for id in page_images(&doc, page) {
            ctx.check()?;
            if !seen.insert(id) {
                continue;
            }
            let Some(Object::Stream(stream)) = doc.objects.get(&id) else { continue };
            let (bytes, kind) =
                render(ctx, &doc, &stream.dict, &stream.content).map_err(|e| e.context(format!("page {} image obj#{}", index + 1, id.0)))?;
            let name = format!("image-{:06}.{kind}", id.0);
            write_new(ctx, &images_dir.join(name), &mut bytes.as_slice())?;
        }
    }
    let mut entries: Vec<_> = fs::read_dir(&images_dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    fs::DirBuilder::new().mode(0o700).create(output).map_err(|e| path_error("mkdir", output, &e))?;
    let copied = (|| {
        for entry in entries {
            let mut source = fs::File::open(entry.path())?;
            write_new(ctx, &output.join(entry.file_name()), &mut source)?;
        }
        Ok(())
    })();
    if copied.is_err() {
        let _ = fs::remove_dir_all(output);
    }
    copied
}

/// Image XObjects a page uses, through its resources and nested forms, plus its thumbnail.
fn page_images(doc: &Document, page: ObjectId) -> BTreeSet<ObjectId> {
    let mut images = BTreeSet::new();
    let mut forms = HashSet::new();
    let mut pending: Vec<&Dictionary> = Vec::new();
    // Resources are inherited from ancestors in the page tree.
    let mut node = doc.objects.get(&page).and_then(dict_of);
    let mut hops = 0;
    while let Some(d) = node {
        if let Some(Object::Dictionary(resources)) = get(doc, d, b"Resources") {
            pending.push(resources);
            break;
        }
        hops += 1;
        node = if hops < 64 { get(doc, d, b"Parent").and_then(dict_of) } else { None };
    }
    while let Some(resources) = pending.pop() {
        let Some(Object::Dictionary(xobjects)) = get(doc, resources, b"XObject") else { continue };
        for (_, value) in xobjects.iter() {
            let Object::Reference(id) = value else { continue };
            match doc.objects.get(id) {
                Some(obj) if image_stream(obj).is_some() => {
                    images.insert(*id);
                }
                Some(Object::Stream(form)) if forms.insert(*id) => {
                    if let Some(Object::Dictionary(inner)) = get(doc, &form.dict, b"Resources") {
                        pending.push(inner);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(Object::Dictionary(page)) = doc.objects.get(&page)
        && let Ok(Object::Reference(thumb)) = page.get(b"Thumb")
        && doc.objects.get(thumb).is_some_and(|t| matches!(t, Object::Stream(_)))
    {
        images.insert(*thumb);
    }
    images
}

enum Space {
    Gray,
    Rgb,
    Cmyk,
    Indexed { base: Box<Space>, palette: Vec<u8>, high: usize },
}

impl Space {
    fn comps(&self) -> usize {
        match self {
            Space::Gray | Space::Indexed { .. } => 1,
            Space::Rgb => 3,
            Space::Cmyk => 4,
        }
    }
}

fn color_space(ctx: &Ctx, doc: &Document, obj: Option<&Object>) -> Result<Space> {
    let Some(obj) = obj else { bail!("image has no color space") };
    match obj {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"CalGray" | b"G" => Ok(Space::Gray),
            b"DeviceRGB" | b"CalRGB" | b"RGB" => Ok(Space::Rgb),
            b"DeviceCMYK" | b"CMYK" => Ok(Space::Cmyk),
            other => bail!("unsupported color space {}", String::from_utf8_lossy(other)),
        },
        Object::Array(items) => {
            let name = items.first().and_then(|n| deref(doc, n)).and_then(|n| n.as_name().ok()).unwrap_or(b"");
            match name {
                b"CalGray" => Ok(Space::Gray),
                b"CalRGB" => Ok(Space::Rgb),
                b"ICCBased" => {
                    let Some(Object::Stream(profile)) = items.get(1).and_then(|p| deref(doc, p)) else { bail!("invalid ICCBased color space") };
                    match get(doc, &profile.dict, b"N").and_then(number) {
                        Some(1.0) => Ok(Space::Gray),
                        Some(3.0) => Ok(Space::Rgb),
                        Some(4.0) => Ok(Space::Cmyk),
                        _ => bail!("unsupported ICCBased color space"),
                    }
                }
                b"Indexed" | b"I" => {
                    let base = color_space(ctx, doc, items.get(1).and_then(|b| deref(doc, b)))?;
                    let high = items.get(2).and_then(|h| deref(doc, h)).and_then(number).unwrap_or(-1.0);
                    if !(0.0..=255.0).contains(&high) || matches!(base, Space::Indexed { .. }) {
                        bail!("invalid Indexed color space");
                    }
                    let palette = match items.get(3).and_then(|l| deref(doc, l)) {
                        Some(Object::String(bytes, _)) => bytes.clone(),
                        Some(Object::Stream(s)) => {
                            let pipeline = filters::pipeline(doc, &s.dict).map_err(Error::new)?;
                            filters::decode(ctx, &s.content, &pipeline, 1 << 20).map_err(|e| Error::new(e.to_string()))?
                        }
                        _ => bail!("invalid Indexed lookup table"),
                    };
                    Ok(Space::Indexed { base: Box::new(base), palette, high: high as usize })
                }
                other => bail!("unsupported color space {}", String::from_utf8_lossy(other)),
            }
        }
        _ => bail!("invalid color space"),
    }
}

fn crc_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32fast::hash(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn png_file(width: usize, height: usize, depth: u8, color: u8, palette: Option<&[u8]>, idat: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[depth, color, 0, 0, 0]);
    crc_chunk(&mut out, b"IHDR", &header);
    if let Some(palette) = palette {
        crc_chunk(&mut out, b"PLTE", palette);
    }
    crc_chunk(&mut out, b"IDAT", idat);
    crc_chunk(&mut out, b"IEND", &[]);
    out
}

/// Rows with the None filter, for depths the adaptive encoder does not handle.
fn plain_idat(samples: &[u8], row: usize, height: usize) -> Vec<u8> {
    let mut rows = Vec::with_capacity((row + 1) * height);
    for line in samples.chunks(row).take(height) {
        rows.push(0);
        rows.extend_from_slice(line);
    }
    crate::image_encoding::deflate_samples(&Ctx::background(), &rows, false, 0).unwrap_or_default()
}

fn tiff_cmyk(width: usize, height: usize, samples: &[u8]) -> Vec<u8> {
    let entries: [(u16, u16, u32, u32); 11] = [
        (256, 4, 1, width as u32),
        (257, 4, 1, height as u32),
        (258, 3, 4, 0), // BitsPerSample offset, filled below
        (259, 3, 1, 1),
        (262, 3, 1, 5),
        (273, 4, 1, 0), // StripOffsets, filled below
        (277, 3, 1, 4),
        (278, 4, 1, height as u32),
        (279, 4, 1, samples.len() as u32),
        (284, 3, 1, 1),
        (332, 3, 1, 1),
    ];
    let ifd = 8u32;
    let ifd_len = 2 + entries.len() as u32 * 12 + 4;
    let bits_at = ifd + ifd_len;
    let data_at = bits_at + 8;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&ifd.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        let value = match tag {
            258 => bits_at,
            273 => data_at,
            _ => value,
        };
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if kind == 3 && count == 1 {
            out.extend_from_slice(&(value as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..4 {
        out.extend_from_slice(&8u16.to_le_bytes());
    }
    out.extend_from_slice(samples);
    out
}

fn invert_if_requested(doc: &Document, dict: &Dictionary, comps: usize, bits: usize, samples: &mut [u8]) {
    let Some(Object::Array(decode)) = get(doc, dict, b"Decode") else { return };
    if decode.len() != 2 * comps || bits > 8 && bits != 16 {
        return;
    }
    let inverted: Vec<bool> = decode
        .chunks(2)
        .map(|pair| deref(doc, &pair[0]).and_then(number) == Some(1.0) && deref(doc, &pair[1]).and_then(number) == Some(0.0))
        .collect();
    if inverted.iter().all(|&i| i) {
        for s in samples.iter_mut() {
            *s = !*s;
        }
    } else if bits >= 8 {
        let step = bits / 8;
        for (i, s) in samples.iter_mut().enumerate() {
            if inverted[i / step % comps] {
                *s = !*s;
            }
        }
    }
}

/// Renders one image to a file, mirroring the formats of earlier releases:
/// JPEG and JPEG 2000 data as stored, CMYK as TIFF, everything else as PNG.
fn render(ctx: &Ctx, doc: &Document, dict: &Dictionary, raw: &[u8]) -> Result<(Vec<u8>, &'static str)> {
    let mut pipeline: Vec<Filter> = filters::pipeline(doc, dict).map_err(Error::new)?;
    let last = pipeline.last().map(|f| f.name.clone());
    let passthrough = match last.as_deref() {
        Some(b"DCTDecode") => Some("jpg"),
        Some(b"JPXDecode") => Some("jpx"),
        Some(b"JBIG2Decode") => bail!("unsupported extracted image format \"jbig2\""),
        _ => None,
    };
    if let Some(kind) = passthrough {
        pipeline.pop();
        let data = filters::decode(ctx, raw, &pipeline, MAX_IMAGE_BYTES).map_err(|e| Error::new(e.to_string()))?;
        return Ok((data, kind));
    }
    let int = |key: &[u8]| get(doc, dict, key).and_then(number).map(|n| n as i64);
    let (Some(w), Some(h)) = (int(b"Width"), int(b"Height")) else { bail!("image has invalid dimensions") };
    if w <= 0 || h <= 0 || w > 1 << 20 || h > 1 << 20 {
        bail!("image has invalid dimensions");
    }
    let (w, h) = (w as usize, h as usize);
    let mask = matches!(get(doc, dict, b"ImageMask"), Some(Object::Boolean(true)));
    let ccitt = last.as_deref() == Some(b"CCITTFaxDecode");
    for f in &mut pipeline {
        if f.name == b"CCITTFaxDecode" && !f.parms.as_ref().is_some_and(|p| p.has(b"Rows")) {
            f.parms.get_or_insert_with(Dictionary::new).set("Rows", h as i64);
        }
    }
    let space =
        if mask || ccitt && get(doc, dict, b"ColorSpace").is_none() { Space::Gray } else { color_space(ctx, doc, get(doc, dict, b"ColorSpace"))? };
    let bits = if mask || ccitt { 1 } else { int(b"BitsPerComponent").unwrap_or(8) as usize };
    if ![1, 2, 4, 8, 16].contains(&bits) {
        bail!("unsupported {bits}-bit image");
    }
    let comps = space.comps();
    let row = (w * comps * bits).div_ceil(8);
    let mut samples = filters::decode(ctx, raw, &pipeline, MAX_IMAGE_BYTES).map_err(|e| Error::new(e.to_string()))?;
    if samples.len() < row * h {
        bail!("image data is shorter than its dimensions");
    }
    samples.truncate(row * h);
    if !matches!(space, Space::Indexed { .. }) {
        invert_if_requested(doc, dict, comps, bits, &mut samples);
    }
    let encode = |kind| {
        png_idat(ctx, &samples, w, h, kind, false, 0).map_err(|e| match e {
            EncodeError::Canceled => Error::canceled(),
            other => Error::new(format!("png: {other:?}")),
        })
    };
    match space {
        Space::Gray => {
            let idat = match bits {
                8 => encode(PngKind::Gray8)?,
                16 => encode(PngKind::Gray16)?,
                _ => plain_idat(&samples, row, h),
            };
            Ok((png_file(w, h, bits as u8, 0, None, &idat), "png"))
        }
        Space::Rgb => {
            if bits < 8 {
                bail!("unsupported {bits}-bit RGB image");
            }
            let idat = encode(if bits == 16 { PngKind::Rgb16 } else { PngKind::Rgb8 })?;
            Ok((png_file(w, h, bits as u8, 2, None, &idat), "png"))
        }
        Space::Cmyk => {
            if bits != 8 {
                bail!("unsupported {bits}-bit CMYK image");
            }
            Ok((tiff_cmyk(w, h, &samples), "tif"))
        }
        Space::Indexed { base, palette, high } => {
            if bits == 16 {
                bail!("unsupported 16-bit indexed image");
            }
            let entries = high + 1;
            let mut rgb = Vec::with_capacity(entries * 3);
            for i in 0..entries {
                match *base {
                    Space::Gray => {
                        let g = palette.get(i).copied().unwrap_or(0);
                        rgb.extend_from_slice(&[g, g, g]);
                    }
                    Space::Rgb => rgb.extend((0..3).map(|c| palette.get(3 * i + c).copied().unwrap_or(0))),
                    Space::Cmyk => {
                        let v: Vec<u32> = (0..4).map(|c| palette.get(4 * i + c).copied().unwrap_or(0) as u32).collect();
                        rgb.extend((0..3).map(|c| ((255 - v[c]) * (255 - v[3]) / 255) as u8));
                    }
                    Space::Indexed { .. } => unreachable!(),
                }
            }
            Ok((png_file(w, h, bits as u8, 3, Some(&rgb), &plain_idat(&samples, row, h)), "png"))
        }
    }
}
