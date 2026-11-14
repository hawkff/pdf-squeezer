//! Re-encodes image XObjects with the built-in codecs, in parallel within a memory budget.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread;

use lopdf::{Dictionary, Document, Object, ObjectId};
use sha2::{Digest, Sha256};

use crate::error::{Ctx, Error, Result};
use crate::filters::{self, Filter};
use crate::image_encoding::{EncodeError, PngKind, deflate_samples, image_samples, png_idat};
use crate::options::{Options, list_contains};
use crate::pdf::{canonical_dict, deref, number};

pub const MIN_IMAGE_SAVINGS: usize = 1024;
const MAX_IMAGE_BYTES: usize = crate::image_encoding::MAX_IMAGE_BYTES;

#[derive(Default, Debug)]
pub struct ImageStats {
    pub seen: usize,
    pub changed: usize,
    pub before: u64,
    pub after: u64,
    pub preserved: BTreeMap<String, usize>,
}

impl fmt::Display for ImageStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "images: {} seen, {} re-encoded, {} -> {} bytes", self.seen, self.changed, self.before, self.after)?;
        let mut reasons: Vec<String> = self.preserved.iter().map(|(reason, n)| format!("{reason} {n}")).collect();
        reasons.sort();
        if !reasons.is_empty() {
            write!(f, "; preserved: {}", reasons.join(", "))?;
        }
        Ok(())
    }
}

pub struct Job {
    pub id: ObjectId,
    pub aliases: Vec<ObjectId>,
    pub width: usize,
    pub height: usize,
    pub bits: usize,
    pub comps: usize,
    pub device: bool,
    pub soft_mask: bool,
    pub invert: Option<Vec<bool>>,
    pub pipeline: Vec<Filter>,
    /// The encoded samples, moved out of the document while the job runs.
    pub raw: Vec<u8>,
    pub cost: u64,
}

#[derive(Default, Debug)]
pub struct Encoding {
    pub data: Vec<u8>,
    pub filter: &'static [u8],
    pub parms: Option<Dictionary>,
    pub bpc: usize,
    pub gray: bool,
    pub clear_decode: bool,
    pub reason: String,
    pub err: Option<Error>,
}

fn preserve(reason: impl Into<String>) -> Encoding {
    Encoding { reason: reason.into(), ..Encoding::default() }
}

fn failed(err: Error) -> Encoding {
    Encoding { err: Some(err), ..Encoding::default() }
}

fn from_encode(e: EncodeError) -> Encoding {
    match e {
        EncodeError::Canceled => failed(Error::canceled()),
        EncodeError::Larger => failed(Error::new("encoded candidate exceeds the current winner")),
        EncodeError::Failed(why) => failed(Error::new(why)),
    }
}

pub fn image_stream(obj: &Object) -> Option<&lopdf::Stream> {
    match obj {
        Object::Stream(s) if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") => Some(s),
        _ => None,
    }
}

fn int(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    match deref(doc, dict.get(key).ok()?)? {
        Object::Integer(i) => Some(*i),
        _ => None,
    }
}

fn precheck(doc: &Document, dict: &Dictionary) -> String {
    if matches!(dict.get(b"ImageMask").ok().and_then(|o| deref(doc, o)), Some(Object::Boolean(true))) {
        return "image mask".into();
    }
    if dict.has(b"Mask") {
        return "mask entry".into();
    }
    let (w, h, bits) = (int(doc, dict, b"Width"), int(doc, dict, b"Height"), int(doc, dict, b"BitsPerComponent"));
    match (w, h, bits) {
        (Some(w), Some(h), Some(bits)) if w > 0 && h > 0 => {
            if ![1, 8, 16].contains(&bits) {
                return format!("{bits}-bit");
            }
            String::new()
        }
        _ => "invalid dimensions".into(),
    }
}

pub fn sample_size(w: i64, h: i64, comps: i64, bits: i64) -> Option<u64> {
    if w <= 0 || h <= 0 || !(1..=4).contains(&comps) || ![1, 8, 16].contains(&bits) {
        return None;
    }
    let max = MAX_IMAGE_BYTES as i64;
    // Divide before multiplying so hostile dimensions cannot overflow the guard.
    if w > max * 8 / (comps * bits) {
        return None;
    }
    let row = (w * comps * bits + 7) / 8;
    if h > max / row {
        return None;
    }
    Some((row * h) as u64)
}

/// Only identity and inversion Decode arrays are normalized. Other transfer
/// functions remain attached to their original bytes rather than being guessed.
fn image_decode(doc: &Document, dict: &Dictionary, comps: usize, mask: bool) -> (Option<Vec<bool>>, String) {
    let Some(decode) = dict.get(b"Decode").ok() else { return (None, String::new()) };
    let Some(Object::Array(a)) = deref(doc, decode) else { return (None, "decode entry".into()) };
    if a.len() != 2 * comps {
        return (None, "decode entry".into());
    }
    let mut invert = vec![false; comps];
    for c in 0..comps {
        let lo = deref(doc, &a[2 * c]).and_then(number);
        let hi = deref(doc, &a[2 * c + 1]).and_then(number);
        match (lo, hi) {
            (Some(lo), Some(hi)) if lo == 0.0 && hi == 1.0 || lo == 1.0 && hi == 0.0 => invert[c] = lo == 1.0,
            _ => return (None, "decode entry".into()),
        }
        if mask && invert[c] {
            return (None, "soft-mask decode".into());
        }
    }
    (Some(invert), String::new())
}

/// The JPEG decoder cannot apply PDF ColorTransform overrides. Preserve any
/// explicit override rather than guessing whether JPEG markers agree with it.
fn jpeg_decode_parameters(doc: &Document, dict: &Dictionary) -> String {
    let Some(parms) = dict.get(b"DecodeParms").ok() else { return String::new() };
    let mut obj = deref(doc, parms);
    if let Some(Object::Array(items)) = obj {
        match items.len() {
            0 => return String::new(),
            1 => obj = deref(doc, &items[0]),
            _ => return "jpeg decode parameters".into(),
        }
    }
    match obj {
        None | Some(Object::Null) => String::new(),
        Some(Object::Dictionary(d)) if d.has(b"ColorTransform") => "jpeg color transform".into(),
        Some(Object::Dictionary(_)) => String::new(),
        Some(_) => "jpeg decode parameters".into(),
    }
}

/// The size of an image's decoded samples, when its dictionary determines it.
pub fn decoded_size(doc: &Document, dict: &Dictionary) -> Option<u64> {
    let (comps, _, reason) = components(doc, dict);
    if !reason.is_empty() {
        return None;
    }
    sample_size(int(doc, dict, b"Width")?, int(doc, dict, b"Height")?, comps as i64, int(doc, dict, b"BitsPerComponent")?)
}

fn components(doc: &Document, dict: &Dictionary) -> (usize, bool, String) {
    let Some(cs) = dict.get(b"ColorSpace").ok().and_then(|c| deref(doc, c)) else {
        return (0, false, "color space".into());
    };
    match cs {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" => (1, true, String::new()),
            b"DeviceRGB" => (3, true, String::new()),
            b"DeviceCMYK" => (4, true, String::new()),
            other => (0, false, format!("color space {}", String::from_utf8_lossy(other))),
        },
        Object::Array(items) => {
            let Some(Object::Name(name)) = items.first().and_then(|n| deref(doc, n)) else {
                return (0, false, "color space".into());
            };
            if name == b"ICCBased"
                && items.len() == 2
                && let Some(Object::Stream(profile)) = deref(doc, &items[1])
                && let Some(n) = int(doc, &profile.dict, b"N")
                && [1, 3, 4].contains(&n)
            {
                return (n as usize, false, String::new());
            }
            (0, false, format!("color space {}", String::from_utf8_lossy(name)))
        }
        _ => (0, false, "color space".into()),
    }
}

fn filter_kind(pipeline: &[Filter]) -> String {
    if let [only] = pipeline
        && only.name == b"DCTDecode"
    {
        return "jpeg".into();
    }
    for f in pipeline {
        match f.name.as_slice() {
            b"FlateDecode" | b"LZWDecode" | b"ASCIIHexDecode" | b"ASCII85Decode" | b"RunLengthDecode" | b"CCITTFaxDecode" => {}
            other => return format!("filter {}", String::from_utf8_lossy(other)),
        }
    }
    "lossless".into()
}

/// Whether any dictionary carries DefaultRGB or DefaultGray, which change what
/// DeviceRGB means. Unusually deep nesting answers yes to stay safe.
fn has_default_color_space(doc: &Document) -> bool {
    let mut stack: Vec<(&Object, usize)> = doc.objects.values().map(|o| (o, 0)).collect();
    while let Some((obj, depth)) = stack.pop() {
        if depth > 100 {
            return true;
        }
        let items: Box<dyn Iterator<Item = &Object>> = match obj {
            Object::Dictionary(d) | Object::Stream(lopdf::Stream { dict: d, .. }) => {
                if d.has(b"DefaultRGB") || d.has(b"DefaultGray") {
                    return true;
                }
                Box::new(d.iter().map(|(_, v)| v))
            }
            Object::Array(a) => Box::new(a.iter()),
            _ => continue,
        };
        stack.extend(items.map(|o| (o, depth + 1)));
    }
    false
}

fn digest(key: &[u8], raw: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(format!("{}:", key.len()).as_bytes());
    h.update(key);
    h.update(raw);
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_slice());
    out
}

fn dict_key(dict: &Dictionary) -> Vec<u8> {
    let mut d = dict.clone();
    d.remove(b"Length");
    let mut out = Vec::new();
    canonical_dict(&d, &mut out);
    out
}

/// Resolves the document before starting workers. Only this function mutates the
/// document; workers own the bytes they encode. The budget covers estimated
/// working buffers, not the parsed document.
pub fn optimize_images(ctx: &Ctx, doc: &mut Document, opts: &Options, logf: &dyn Fn(String)) -> Result<ImageStats> {
    let mut stats = ImageStats::default();
    let mut soft_masks = HashSet::new();
    for obj in doc.objects.values() {
        if let Some(s) = image_stream(obj)
            && let Ok(Object::Reference(mask)) = s.dict.get(b"SMask")
        {
            soft_masks.insert(*mask);
        }
    }
    let defaults = has_default_color_space(doc);
    let ids: Vec<ObjectId> = doc.objects.iter().filter(|(_, o)| image_stream(o).is_some()).map(|(id, _)| *id).collect();
    let budget = (opts.image_memory as u64) << 20;
    let mut jobs: Vec<Job> = Vec::new();
    let mut mask_jobs: HashMap<[u8; 32], usize> = HashMap::new();
    let mut job_keys: Vec<Vec<u8>> = Vec::new();
    for id in ids {
        ctx.check()?;
        let stream = image_stream(&doc.objects[&id]).expect("image stream");
        let dict = &stream.dict;
        let raw_len = stream.content.len();
        let soft_mask = soft_masks.contains(&id);
        let mut job = Job {
            id,
            aliases: Vec::new(),
            width: 0,
            height: 0,
            bits: 0,
            comps: 0,
            device: false,
            soft_mask,
            invert: None,
            pipeline: Vec::new(),
            raw: Vec::new(),
            cost: 0,
        };
        let mut reason = precheck(doc, dict);
        if reason.is_empty() {
            match filters::pipeline(doc, dict) {
                Ok(p) => job.pipeline = p,
                Err(_) => reason = "filter pipeline".into(),
            }
        }
        if reason.is_empty() && filter_kind(&job.pipeline) == "jpeg" {
            reason = jpeg_decode_parameters(doc, dict);
        }
        if reason.is_empty() {
            let (comps, device, why) = components(doc, dict);
            job.comps = comps;
            job.device = device && !(defaults && comps == 3);
            reason = why;
        }
        if reason.is_empty()
            && let Ok(mask) = dict.get(b"SMask")
            && let Object::Reference(_) = mask
        {
            match deref(doc, mask) {
                Some(Object::Stream(m)) if m.dict.has(b"Matte") => reason = "soft-mask matte".into(),
                Some(Object::Stream(_)) => {}
                _ => reason = "invalid soft mask".into(),
            }
        }
        if reason.is_empty() {
            let (invert, why) = image_decode(doc, dict, job.comps, soft_mask);
            job.invert = invert;
            reason = why;
        }
        if reason.is_empty() && !opts.force && raw_len < MIN_IMAGE_SAVINGS {
            reason = "no gain".into();
        }
        if reason.is_empty() {
            let (w, h, bits) =
                (int(doc, dict, b"Width").unwrap_or(0), int(doc, dict, b"Height").unwrap_or(0), int(doc, dict, b"BitsPerComponent").unwrap_or(0));
            match sample_size(w, h, job.comps as i64, bits) {
                None => reason = "too large".into(),
                Some(n) => {
                    (job.width, job.height, job.bits) = (w as usize, h as usize, bits as usize);
                    // Samples, one transformed copy, an encoder's output and six PNG filter rows, plus fixed codec state.
                    job.cost = 3 * n + 6 * n.div_ceil(h as u64) + 2 * raw_len as u64 + (8 << 20);
                    if job.cost > budget {
                        reason = "memory budget".into();
                    }
                }
            }
        }
        stats.seen += 1;
        stats.before += raw_len as u64;
        if !reason.is_empty() {
            *stats.preserved.entry(reason).or_default() += 1;
            stats.after += raw_len as u64;
            continue;
        }
        if soft_mask {
            let key = dict_key(dict);
            let sum = digest(&key, &stream.content);
            if let Some(&index) = mask_jobs.get(&sum) {
                if job_keys[index] == key && jobs[index].raw == stream.content {
                    jobs[index].aliases.push(id);
                    continue;
                }
            } else {
                mask_jobs.insert(sum, jobs.len());
            }
            job_keys.push(key);
        } else {
            job_keys.push(Vec::new());
        }
        let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else { unreachable!() };
        job.raw = std::mem::take(&mut stream.content);
        jobs.push(job);
    }
    drop(job_keys);

    let workers = thread::available_parallelism().map_or(1, |n| n.get());
    let total = jobs.len();
    let work = ctx.child();
    let mut first_err: Option<Error> = None;
    let mut queue = jobs.into_iter().peekable();
    thread::scope(|scope| {
        let (results, finished) = mpsc::channel::<(Job, Encoding)>();
        let (mut active, mut used, mut done) = (0usize, 0u64, 0usize);
        loop {
            if first_err.is_none()
                && let Some(e) = work.err()
            {
                first_err = Some(e);
            }
            while first_err.is_none() && active < workers && queue.peek().is_some_and(|j| used + j.cost <= budget) {
                let job = queue.next().unwrap();
                let cost = job.cost;
                let (results, work_ctx) = (results.clone(), work.clone());
                let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                    let encoding = catch_unwind(AssertUnwindSafe(|| reencode(&work_ctx, &job, opts)))
                        .unwrap_or_else(|_| failed(Error::new("image encoder panicked")));
                    let _ = results.send((job, encoding));
                });
                match spawned {
                    Ok(_) => {
                        active += 1;
                        used += cost;
                    }
                    // The document is discarded on error, so the job's bytes are not needed.
                    Err(e) => {
                        first_err = Some(Error::new(format!("cannot start an image worker: {e}")));
                        work.cancel();
                    }
                }
            }
            if active == 0 {
                break;
            }
            let Ok((mut job, encoding)) = finished.recv() else { break };
            active -= 1;
            used -= job.cost;
            done += 1;
            if let Some(err) = encoding.err {
                if first_err.is_none() {
                    first_err = Some(if err.is_canceled() { err } else { err.context(format!("image object {}", job.id.0)) });
                    work.cancel();
                }
                continue;
            }
            if first_err.is_some() {
                continue;
            }
            let count = 1 + job.aliases.len();
            if !encoding.reason.is_empty() {
                *stats.preserved.entry(encoding.reason.clone()).or_default() += count;
                stats.after += (count * job.raw.len()) as u64;
                if let Some(Object::Stream(s)) = doc.objects.get_mut(&job.id) {
                    s.content = std::mem::take(&mut job.raw);
                }
            } else {
                for id in std::iter::once(job.id).chain(job.aliases.iter().copied()) {
                    if let Some(Object::Stream(s)) = doc.objects.get_mut(&id) {
                        apply_encoding(s, &encoding);
                    }
                }
                stats.changed += count;
                stats.after += (count * encoding.data.len()) as u64;
            }
            logf(format!("image {done}/{total}: object {}", job.id.0));
        }
    });
    if let Some(e) = first_err.or_else(|| ctx.err()) {
        return Err(e);
    }
    Ok(stats)
}

pub fn apply_encoding(stream: &mut lopdf::Stream, out: &Encoding) {
    stream.set_content(out.data.clone());
    let d = &mut stream.dict;
    d.set("BitsPerComponent", out.bpc as i64);
    d.set("Filter", Object::Name(out.filter.to_vec()));
    match &out.parms {
        Some(p) => d.set("DecodeParms", p.clone()),
        None => {
            d.remove(b"DecodeParms");
        }
    }
    if out.gray {
        d.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
    }
    if out.clear_decode {
        d.remove(b"Decode");
    }
}

/// CCITT streams without /Rows decode to the image height.
fn with_rows(pipeline: &[Filter], height: usize) -> Vec<Filter> {
    pipeline
        .iter()
        .map(|f| {
            let mut f = f.clone();
            if f.name == b"CCITTFaxDecode" && !f.parms.as_ref().is_some_and(|p| p.has(b"Rows")) {
                f.parms.get_or_insert_with(Dictionary::new).set("Rows", height as i64);
            }
            f
        })
        .collect()
}

fn decode_jpeg(raw: &[u8], w: usize, h: usize) -> std::result::Result<(Vec<u8>, usize), &'static str> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().set_strict_mode(true).set_max_width(1 << 16).set_max_height(1 << 16);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(raw), options);
    decoder.decode_headers().map_err(|_| "undecodable jpeg")?;
    let info = decoder.info().ok_or("undecodable jpeg")?;
    if info.width as usize != w || info.height as usize != h {
        return Err("undecodable jpeg");
    }
    let (space, comps) = match decoder.input_colorspace() {
        Some(ColorSpace::Luma) => (ColorSpace::Luma, 1),
        Some(ColorSpace::YCbCr | ColorSpace::RGB) => (ColorSpace::RGB, 3),
        _ => return Err("jpeg color model"),
    };
    decoder.set_options(options.jpeg_set_out_colorspace(space));
    let pixels = decoder.decode().map_err(|_| "undecodable jpeg")?;
    Ok((pixels, comps))
}

fn encode_jpeg(samples: &[u8], w: usize, h: usize, comps: usize, quality: i64) -> Result<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let mut out = Vec::with_capacity(samples.len() / 8);
    let mut encoder = Encoder::new(&mut out, quality.clamp(1, 100) as u8);
    // 4:2:0 chroma at every quality, so light keeps the sizes of earlier releases.
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder.set_optimized_huffman_tables(true);
    let color = if comps == 1 { ColorType::Luma } else { ColorType::Rgb };
    encoder.encode(samples, w as u16, h as u16, color).map_err(|e| Error::new(format!("jpeg: {e}")))?;
    Ok(out)
}

fn high_bytes(samples: &[u8]) -> Vec<u8> {
    samples.iter().step_by(2).copied().collect()
}

fn all_gray(rgb: &[u8], bits: usize) -> bool {
    let n = bits / 8;
    rgb.chunks_exact(3 * n).all(|p| p[..n] == p[n..2 * n] && p[..n] == p[2 * n..])
}

fn gray_channel(rgb: &[u8], bits: usize) -> Vec<u8> {
    let n = bits / 8;
    rgb.chunks_exact(3 * n).flat_map(|p| p[..n].iter().copied()).collect()
}

fn only_black_and_white(gray: &[u8]) -> bool {
    gray.iter().all(|&v| v == 0 || v == 255)
}

fn keep_encoding(raw_len: usize, out: Encoding, force: bool) -> Encoding {
    let saved = raw_len as i64 - out.data.len() as i64;
    if !force && (saved < MIN_IMAGE_SAVINGS as i64 || saved < (raw_len as i64 + 49) / 50) {
        return preserve("no gain");
    }
    out
}

pub fn reencode(ctx: &Ctx, job: &Job, opts: &Options) -> Encoding {
    if let Some(e) = ctx.err() {
        return failed(e);
    }
    let (w, h) = (job.width, job.height);
    let (mut bits, mut comps) = (job.bits, job.comps);
    let Some(expected) = sample_size(w as i64, h as i64, comps as i64, bits as i64) else {
        return preserve("too large");
    };
    if bits == 1 && comps != 1 {
        return preserve("multichannel 1-bit");
    }
    if bits == 1 && !opts.mono_codecs.is_empty() && opts.mono_codecs != "flate" {
        return preserve("external monochrome codec");
    }
    let kind = filter_kind(&job.pipeline);
    if opts.lossless && kind == "jpeg" {
        return preserve("lossy original");
    }
    let mut samples: Vec<u8> = match kind.as_str() {
        "lossless" => {
            let decoded = image_samples(ctx, &job.raw, &with_rows(&job.pipeline, h), expected as usize, h);
            if let Some(e) = ctx.err() {
                return failed(e);
            }
            match decoded {
                Ok(data) if data.len() as u64 == expected => data,
                Ok(_) => return preserve("length mismatch"),
                Err(_) => return preserve("undecodable"),
            }
        }
        "jpeg" => {
            if comps == 4 {
                return preserve("CMYK JPEG");
            }
            let (mut data, mut actual) = match decode_jpeg(&job.raw, w, h) {
                Ok(decoded) => decoded,
                Err(reason) => return preserve(reason),
            };
            if let Some(e) = ctx.err() {
                return failed(e);
            }
            if actual == 1 && comps == 3 && job.device {
                data = data.iter().flat_map(|&v| [v, v, v]).collect();
                actual = 3;
            }
            if actual != comps {
                return preserve("jpeg color model");
            }
            if data.len() != w * h * comps {
                return preserve("length mismatch");
            }
            bits = 8;
            data
        }
        _ => return preserve(kind),
    };
    if let Some(e) = ctx.err() {
        return failed(e);
    }
    let mut out = Encoding { bpc: bits, clear_decode: job.invert.is_some(), ..Encoding::default() };
    if let Some(invert) = &job.invert {
        let step = if bits >= 8 { bits / 8 } else { 0 };
        for (i, sample) in samples.iter_mut().enumerate() {
            if i % 65536 == 0 && ctx.done() {
                return failed(Error::canceled());
            }
            let component = i.checked_div(step).map_or(0, |n| n % comps);
            if invert[component] {
                *sample = !*sample;
            }
        }
    }
    if bits == 16 && opts.reduce_bits && !job.soft_mask {
        samples = high_bytes(&samples);
        bits = 8;
        out.bpc = 8;
    }
    if comps == 3 && job.device && !opts.lossless && opts.color_reduction != "preserve" && all_gray(&samples, bits) {
        samples = gray_channel(&samples, bits);
        comps = 1;
        out.gray = true;
    }
    let heavy = opts.compression == "heavy";
    if bits == 1 {
        return match deflate_samples(ctx, &samples, heavy, 0) {
            Ok(data) => {
                out.data = data;
                out.filter = b"FlateDecode";
                out.bpc = 1;
                keep_encoding(job.raw.len(), out, opts.force)
            }
            Err(e) => from_encode(e),
        };
    }
    let bilevel = bits == 8
        && comps == 1
        && !opts.lossless
        && opts.color_reduction != "preserve"
        && !job.soft_mask
        && job.device
        && only_black_and_white(&samples);
    if comps == 4 {
        // The JPEG and PNG encoders cannot keep CMYK sample semantics.
        if !list_contains(&opts.image_codecs, "flate") {
            return preserve("no permitted CMYK codec");
        }
        return match deflate_samples(ctx, &samples, heavy, 0) {
            Ok(data) => {
                out.data = data;
                out.filter = b"FlateDecode";
                keep_encoding(job.raw.len(), out, opts.force)
            }
            Err(e) => from_encode(e),
        };
    }
    let png_kind = match (bilevel, comps, bits) {
        (true, _, _) => PngKind::Bilevel,
        (_, 1, 16) => PngKind::Gray16,
        (_, 1, _) => PngKind::Gray8,
        (_, _, 16) => PngKind::Rgb16,
        _ => PngKind::Rgb8,
    };
    if bilevel {
        out.bpc = 1;
    }
    // JPEG is usually cheaper. Bound competing encoders by its actual payload.
    if w < 65536 && h < 65536 && bits == 8 && !bilevel && !job.soft_mask && !opts.lossless && list_contains(&opts.image_codecs, "jpeg") {
        match encode_jpeg(&samples, w, h, comps, opts.image_quality) {
            Ok(data) => {
                out.data = data;
                out.filter = b"DCTDecode";
            }
            Err(e) => return failed(e),
        }
    }
    if list_contains(&opts.image_codecs, "flate") || bilevel || job.soft_mask {
        match png_idat(ctx, &samples, w, h, png_kind, heavy, out.data.len()) {
            Ok(data) => {
                // Preserve the original tie order: PNG, raw Flate, then JPEG.
                out.data = data;
                out.filter = b"FlateDecode";
                let mut parms = Dictionary::new();
                parms.set("Predictor", 15);
                parms.set("Colors", comps as i64);
                parms.set("BitsPerComponent", out.bpc as i64);
                parms.set("Columns", w as i64);
                out.parms = Some(parms);
            }
            Err(EncodeError::Larger) => {}
            Err(e) => return from_encode(e),
        }
    }
    if heavy && !bilevel && (list_contains(&opts.image_codecs, "flate") || job.soft_mask) {
        match deflate_samples(ctx, &samples, true, out.data.len()) {
            Ok(data) => {
                if out.data.is_empty() || data.len() < out.data.len() || data.len() == out.data.len() && out.filter == b"DCTDecode" {
                    out.data = data;
                    out.filter = b"FlateDecode";
                    out.parms = None;
                }
            }
            Err(EncodeError::Larger) => {}
            Err(e) => return from_encode(e),
        }
    }
    if out.data.is_empty() {
        return preserve("no permitted codec");
    }
    keep_encoding(job.raw.len(), out, opts.force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_sizes() {
        assert!(sample_size(i64::MAX, i64::MAX, 4, 16).is_none());
        assert!(sample_size(0, 1, 1, 8).is_none());
        assert_eq!(sample_size(9, 2, 1, 1), Some(4));
    }

    #[test]
    fn stats_text() {
        let mut s = ImageStats { seen: 7, changed: 2, before: 10, after: 5, ..Default::default() };
        s.preserved.insert("no gain".into(), 2);
        s.preserved.insert("color space Indexed".into(), 1);
        s.preserved.insert("image mask".into(), 1);
        assert_eq!(s.to_string(), "images: 7 seen, 2 re-encoded, 10 -> 5 bytes; preserved: color space Indexed 1, image mask 1, no gain 2");
    }

    fn job(width: usize, height: usize, bits: usize, comps: usize, raw: Vec<u8>) -> Job {
        Job { id: (1, 0), aliases: vec![], width, height, bits, comps, device: true, soft_mask: false, invert: None, pipeline: vec![], raw, cost: 0 }
    }

    #[test]
    fn jpeg_dimension_limit() {
        for (w, h) in [(65536, 1), (1, 65536)] {
            for comps in [1, 3] {
                for codecs in ["flate,jpeg", "flate", "jpeg"] {
                    let mut samples = vec![30u8; w * h * comps];
                    if comps == 3 {
                        for i in (1..samples.len()).step_by(3) {
                            samples[i] = 80;
                        }
                    }
                    let opts = Options { image_codecs: codecs.into(), image_quality: 75, force: true, ..Options::default() };
                    let out = reencode(&Ctx::background(), &job(w, h, 8, comps, samples.clone()), &opts);
                    assert!(out.err.is_none());
                    if codecs == "jpeg" {
                        assert_eq!(out.reason, "no permitted codec");
                    } else {
                        assert_eq!(out.reason, "");
                        assert_eq!(out.filter, b"FlateDecode");
                    }
                }
            }
        }
    }

    #[test]
    fn candidates_and_tie_order() {
        let ctx = Ctx::background();
        for bits in [8, 16] {
            for comps in [1, 3] {
                for heavy in [false, true] {
                    for flat in [false, true] {
                        let (w, h) = (64, 64);
                        let samples: Vec<u8> = (0..w * h * comps * bits / 8)
                            .map(|i| if flat { 83 } else { ((i as u32).wrapping_mul(2654435761) >> 13) as u8 })
                            .collect();
                        let mut opts = Options {
                            image_codecs: "flate,jpeg".into(),
                            image_quality: 45,
                            force: true,
                            color_reduction: "preserve".into(),
                            ..Options::default()
                        };
                        if heavy {
                            opts.compression = "heavy".into();
                        }
                        let j = job(w, h, bits, comps, samples.clone());
                        let got = reencode(&ctx, &j, &opts);
                        assert!(got.err.is_none() && got.reason.is_empty(), "{bits} {comps} {heavy} {flat}: {got:?}");
                        let kind = match (comps, bits) {
                            (1, 16) => PngKind::Gray16,
                            (1, _) => PngKind::Gray8,
                            (_, 16) => PngKind::Rgb16,
                            _ => PngKind::Rgb8,
                        };
                        let mut want = png_idat(&ctx, &samples, w, h, kind, heavy, 0).unwrap();
                        let mut want_filter: &[u8] = b"FlateDecode";
                        let mut predictor = true;
                        if heavy {
                            let raw = deflate_samples(&ctx, &samples, true, 0).unwrap();
                            if raw.len() < want.len() {
                                (want, predictor) = (raw, false);
                            }
                        }
                        if bits == 8 {
                            let jpeg = encode_jpeg(&samples, w, h, comps, 45).unwrap();
                            if jpeg.len() < want.len() {
                                (want, want_filter, predictor) = (jpeg, b"DCTDecode", false);
                            }
                        }
                        assert_eq!(got.filter, want_filter);
                        assert_eq!(got.parms.is_some(), predictor);
                        assert_eq!(got.data, want);
                        assert_eq!(j.raw, samples);
                    }
                }
            }
        }
    }
}
