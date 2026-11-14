//! Document-level changes: metadata, removals, standard fonts and stream deduplication.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use sha2::{Digest, Sha256};

use crate::error::{Ctx, Result};
use crate::filters;
use crate::images::image_stream;
use crate::options::{Options, list_contains};
use crate::pdf::{canonical_dict, catalog_mut, deref, dict_of, dict_of_mut, get, info_id, name_entry};

/// A text string in UTF-16BE with a byte-order mark, written as hex.
pub fn pdf_string(s: &str) -> Object {
    let mut data = vec![0xfe, 0xff];
    for unit in s.encode_utf16() {
        data.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(data, StringFormat::Hexadecimal)
}

/// The current UTC time as a PDF date, D:YYYYMMDDhhmmssZ.
pub fn pdf_date_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("D:{year:04}{month:02}{day:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn info_dict_mut(doc: &mut Document) -> &mut Dictionary {
    let existing = info_id(doc).filter(|id| matches!(doc.objects.get(id), Some(Object::Dictionary(_))));
    let id = match existing {
        Some(id) => id,
        None => {
            let id = doc.add_object(Dictionary::new());
            doc.trailer.set("Info", Object::Reference(id));
            id
        }
    };
    match doc.objects.get_mut(&id) {
        Some(Object::Dictionary(d)) => d,
        _ => unreachable!("info dictionary"),
    }
}

/// Empties the document information and drops the identifier, so the writer
/// gives the file a new one.
pub fn strip_metadata(doc: &mut Document) {
    let id = doc.add_object(Dictionary::new());
    doc.trailer.set("Info", Object::Reference(id));
    doc.trailer.remove(b"ID");
}

pub fn document_dates(doc: &Document) -> BTreeMap<&'static str, Object> {
    let mut dates = BTreeMap::new();
    let Some(Object::Dictionary(info)) = info_id(doc).and_then(|id| doc.objects.get(&id)) else { return dates };
    for key in ["CreationDate", "ModDate"] {
        if let Some(value @ Object::String(..)) = get(doc, info, key.as_bytes()) {
            dates.insert(key, value.clone());
        }
    }
    dates
}

pub fn restore_document_dates(doc: &mut Document, dates: &BTreeMap<&'static str, Object>) {
    if dates.is_empty() {
        return;
    }
    let info = info_dict_mut(doc);
    for (key, value) in dates {
        info.set(*key, value.clone());
    }
}

const STANDARD_FONTS: &str = "Courier,Courier-Bold,Courier-Oblique,Courier-BoldOblique,Helvetica,Helvetica-Bold,Helvetica-Oblique,Helvetica-BoldOblique,Times-Roman,Times-Bold,Times-Italic,Times-BoldItalic,Symbol,ZapfDingbats";

/// Whether a font dictionary embeds a standard font the viewer can supply itself.
fn removable_standard_font(doc: &Document, d: &Dictionary) -> bool {
    let (Some(name), Some(subtype)) = (name_entry(d, b"BaseFont"), name_entry(d, b"Subtype")) else { return false };
    if subtype != b"Type1" && subtype != b"TrueType" {
        return false;
    }
    if !list_contains(STANDARD_FONTS, &String::from_utf8_lossy(name)) {
        return false;
    }
    if let Ok(encoding) = d.get(b"Encoding") {
        match encoding {
            Object::Name(n) if list_contains("WinAnsiEncoding,MacRomanEncoding,StandardEncoding", &String::from_utf8_lossy(n)) => {}
            _ => return false,
        }
    }
    match d.get(b"FontDescriptor").ok().map(|f| deref(doc, f)) {
        None | Some(None) | Some(Some(Object::Dictionary(_) | Object::Null)) => true,
        Some(Some(_)) => false,
    }
}

fn is_name(obj: Option<&Object>, name: &[u8]) -> bool {
    matches!(obj, Some(Object::Name(n)) if n == name)
}

/// Whether the stream a Metadata key names is XMP, judged by the stream's own
/// type or subtype, never by its owner's type.
fn xmp_metadata(doc: &Document, owner: &Dictionary) -> bool {
    let Some(Object::Stream(metadata)) = get(doc, owner, b"Metadata") else { return false };
    let kind = get(doc, &metadata.dict, b"Type");
    let subtype = get(doc, &metadata.dict, b"Subtype");
    let xmp = is_name(subtype, b"XML") || is_name(subtype, b"XMP");
    is_name(kind, b"Metadata") || kind.is_none() && xmp
}

#[derive(Default)]
struct Change {
    remove: Vec<&'static [u8]>,
    annots: Option<Vec<Object>>,
    standard_font: bool,
}

pub fn transform_document(ctx: &Ctx, doc: &mut Document, opts: &Options) -> Result<()> {
    if opts.privacy {
        strip_metadata(doc);
    }
    let update_dates = opts.timestamps == "now" || opts.timestamps == "modified";
    if !opts.metadata.is_empty() || update_dates {
        let date = pdf_date_now();
        let info = info_dict_mut(doc);
        if update_dates && !opts.privacy {
            info.set("ModDate", Object::string_literal(date.clone()));
            if opts.timestamps == "now" {
                info.set("CreationDate", Object::string_literal(date));
            }
        }
        for setting in &opts.metadata {
            let (key, value) = setting.split_once('=').unwrap_or((setting, ""));
            if value.is_empty() {
                info.remove(key.as_bytes());
            } else if key == "CreationDate" || key == "ModDate" {
                info.set(key, Object::string_literal(value));
            } else {
                info.set(key, pdf_string(value));
            }
        }
    }
    let strip = |category: &str| list_contains(&opts.strip, category);
    let remove_images = strip("images");
    let metadata_edits = opts.privacy || !opts.metadata.is_empty() || update_dates || strip("metadata");
    let ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    let mut changes: Vec<(ObjectId, Change)> = Vec::new();
    let mut emptied: Vec<ObjectId> = Vec::new();
    for id in ids {
        ctx.check()?;
        let obj = &doc.objects[&id];
        if remove_images && image_stream(obj).is_some() {
            emptied.push(id);
            continue;
        }
        let Some(d) = dict_of(obj) else { continue };
        let stream = matches!(obj, Object::Stream(_));
        let kind = get(doc, d, b"Type").and_then(|t| t.as_name().ok());
        let mut change = Change::default();
        if metadata_edits && xmp_metadata(doc, d) {
            change.remove.push(b"Metadata");
        }
        if opts.privacy || strip("piece-info") {
            let subtype = get(doc, d, b"Subtype");
            let owner = matches!(kind, Some(b"Catalog" | b"Page" | b"XObject"));
            if owner || stream && (is_name(subtype, b"Form") || is_name(subtype, b"Image")) {
                change.remove.extend([b"PieceInfo".as_slice(), b"LastModified"]);
            }
        }
        if kind == Some(b"Page") {
            if remove_images || strip("thumbnails") {
                change.remove.push(b"Thumb");
            }
            if strip("threads") {
                change.remove.push(b"B");
            }
        }
        if name_entry(d, b"Subtype") == Some(b"Image") && strip("alternates") {
            change.remove.push(b"Alternates");
        }
        if strip("tags") {
            for key in [b"StructParent".as_slice(), b"StructParents"] {
                if matches!(get(doc, d, key), Some(Object::Integer(_))) {
                    change.remove.push(key);
                }
            }
        }
        change.standard_font = opts.remove_standard_fonts && removable_standard_font(doc, d);
        if name_entry(d, b"Type") == Some(b"Page")
            && let Some(annots) = get(doc, d, b"Annots")
        {
            let Object::Array(annots) = annots else {
                crate::bail!("page {} {} annotations are not an array", id.0, id.1);
            };
            let mut retained = Vec::new();
            for a in annots {
                let annot = match deref(doc, a) {
                    Some(Object::Dictionary(ad)) => Some(ad),
                    None | Some(Object::Null) => None,
                    Some(_) => crate::bail!("page {} {} has an annotation that is not a dictionary", id.0, id.1),
                };
                let category = match annot.and_then(|ad| name_entry(ad, b"Subtype")) {
                    Some(b"Widget") => "forms",
                    Some(b"Link") => "links",
                    _ => "annotations",
                };
                if !strip(category) {
                    retained.push(a.clone());
                }
            }
            if retained.len() != annots.len() {
                change.annots = Some(retained);
            }
        }
        if !change.remove.is_empty() || change.annots.is_some() || change.standard_font {
            changes.push((id, change));
        }
    }
    for id in emptied {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"XObject".to_vec()));
        dict.set("Subtype", Object::Name(b"Form".to_vec()));
        dict.set("BBox", Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]));
        doc.objects.insert(id, Object::Stream(Stream::new(dict, b"q Q\n".to_vec())));
    }
    for (id, change) in changes {
        let Some(d) = doc.objects.get_mut(&id).and_then(dict_of_mut) else { continue };
        for key in change.remove {
            d.remove(key);
        }
        if let Some(retained) = change.annots {
            if retained.is_empty() {
                d.remove(b"Annots");
            } else {
                d.set("Annots", Object::Array(retained));
            }
        }
        if change.standard_font {
            d.remove(b"FontDescriptor");
            d.set("Subtype", Object::Name(b"Type1".to_vec()));
        }
    }
    if let Some(root) = catalog_mut(doc) {
        let mut drop = |cond: bool, key: &[u8]| {
            if cond {
                root.remove(key);
            }
        };
        drop(strip("forms"), b"AcroForm");
        drop(strip("threads"), b"Threads");
        drop(strip("output-intents"), b"OutputIntents");
        drop(strip("tags"), b"StructTreeRoot");
        drop(strip("tags"), b"MarkInfo");
        drop(opts.privacy || strip("web-capture"), b"SpiderInfo");
    }
    Ok(())
}

fn stream_dictionary_key(dict: &Dictionary) -> Vec<u8> {
    let mut d = dict.clone();
    d.remove(b"Length");
    let mut out = Vec::new();
    canonical_dict(&d, &mut out);
    out
}

/// Only generalized lossless filters are normalized. Encoded image filters keep
/// their parameters, including bitonal polarity and globals.
fn normalized_key(doc: &Document, s: &Stream) -> (Vec<u8>, bool) {
    let normalize = filters::pipeline(doc, &s.dict).is_ok_and(|p| {
        p.iter().all(|f| matches!(f.name.as_slice(), b"FlateDecode" | b"LZWDecode" | b"ASCIIHexDecode" | b"ASCII85Decode" | b"RunLengthDecode"))
    });
    let mut d = s.dict.clone();
    d.remove(b"Length");
    if normalize {
        d.remove(b"Filter");
        d.remove(b"DecodeParms");
    }
    let mut out = Vec::new();
    canonical_dict(&d, &mut out);
    (out, normalize)
}

fn stream_identity(ctx: &Ctx, doc: &Document, s: &Stream) -> Option<(Vec<u8>, Vec<u8>)> {
    let (key, normalize) = normalized_key(doc, s);
    if !normalize {
        return Some((key, s.content.clone()));
    }
    let pipeline = filters::pipeline(doc, &s.dict).ok()?;
    let data = filters::decode(ctx, &s.content, &pipeline, IDENTITY_LIMIT).ok()?;
    Some((key, data))
}

fn digest(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(format!("{}:", key.len()).as_bytes());
    h.update(key);
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_slice());
    out
}

const IDENTITY_LIMIT: usize = 32 << 20;

fn image_stream_dict(dict: &Dictionary) -> bool {
    name_entry(dict, b"Subtype") == Some(b"Image")
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

/// Adler-32 of a stream's decoded data, which equal data shares in every encoding.
/// Plain Flate streams carry it in their trailer, so they need no decoding here.
fn data_checksum(ctx: &Ctx, doc: &Document, s: &Stream) -> Option<u32> {
    let (_, normalize) = normalized_key(doc, s);
    if !normalize {
        // Other encodings only match byte for byte, which the raw pass covers.
        return None;
    }
    // Identity decoding stops at 32 MiB, so larger images cannot be compared anyway.
    if image_stream_dict(&s.dict) && crate::images::decoded_size(doc, &s.dict).is_some_and(|n| n > IDENTITY_LIMIT as u64) {
        return None;
    }
    let pipeline = filters::pipeline(doc, &s.dict).ok()?;
    let plain = |f: &filters::Filter| f.parms.as_ref().and_then(|p| p.get(b"Predictor").ok()).is_none_or(|p| matches!(p, Object::Integer(1)));
    // Decoding every large predicted image costs seconds. Their trailers checksum the
    // predicted rows, which equal images share unless two encoders chose different
    // row filters; such pairs stay separate, and the final comparison stays exact.
    let large = s.content.len() > 1 << 20 || crate::images::decoded_size(doc, &s.dict).is_some_and(|n| n > 4 << 20);
    match pipeline.as_slice() {
        [] => Some(adler32(&s.content)),
        // Some writers count the end-of-line before endstream in /Length; then the last
        // four bytes are not the checksum, and the data is decoded instead.
        [f] if f.name == b"FlateDecode" && (plain(f) || large) && !s.content.ends_with(b"\n") && !s.content.ends_with(b"\r") => {
            let tail = s.content.get(s.content.len().checked_sub(4)?..)?;
            Some(u32::from_be_bytes(tail.try_into().ok()?))
        }
        _ => filters::decode(ctx, &s.content, &pipeline, IDENTITY_LIMIT).ok().map(|data| adler32(&data)),
    }
}

/// Points references to identical streams at one copy, keeping the smallest
/// encoding. With normalize, streams that differ only in lossless filters match.
pub fn deduplicate_streams(ctx: &Ctx, doc: &mut Document, normalize: bool, memory_mib: i64) -> Result<()> {
    let mut keys: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    let mut peers: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut ids = Vec::new();
    for (id, obj) in &doc.objects {
        ctx.check()?;
        let Object::Stream(s) = obj else { continue };
        let key = if normalize { normalized_key(doc, s).0 } else { stream_dictionary_key(&s.dict) };
        *peers.entry(key.clone()).or_default() += 1;
        keys.insert(*id, key);
        ids.push(*id);
    }
    let mut raw_seen: HashMap<[u8; 32], ObjectId> = HashMap::new();
    let mut replace: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut candidates = Vec::new();
    for id in ids {
        ctx.check()?;
        // Different normalized dictionaries cannot match, regardless of bytes.
        if peers[&keys[&id]] < 2 {
            continue;
        }
        let Object::Stream(s) = &doc.objects[&id] else { continue };
        // Exact encodings need no decoding, including aliases re-encoded together.
        let raw_key = stream_dictionary_key(&s.dict);
        let raw_digest = digest(&raw_key, &s.content);
        if let Some(&original) = raw_seen.get(&raw_digest) {
            let Object::Stream(other) = &doc.objects[&original] else { continue };
            if raw_key == stream_dictionary_key(&other.dict) && s.content == other.content {
                replace.insert(id, original);
                continue;
            }
        } else {
            raw_seen.insert(raw_digest, id);
        }
        if normalize {
            candidates.push(id);
        }
    }
    // Streams whose decoded data may match, by dictionary and Adler-32 of that data.
    // Each decode can hold two 32 MiB stages, so the image budget bounds the workers.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = ((memory_mib.max(1) as usize) / (2 * (IDENTITY_LIMIT >> 20))).clamp(1, cores).min(candidates.len());
    let sums: Vec<Option<u32>> = {
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<u32>>> = candidates.iter().map(|_| Mutex::new(None)).collect();
        let work = || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= candidates.len() || ctx.done() {
                    break;
                }
                if let Object::Stream(s) = &doc.objects[&candidates[i]] {
                    *slots[i].lock().unwrap() = data_checksum(ctx, doc, s);
                }
            }
        };
        std::thread::scope(|scope| {
            // Helpers that cannot start leave their share to this thread.
            for _ in 1..workers {
                if std::thread::Builder::new().spawn_scoped(scope, work).is_err() {
                    break;
                }
            }
            work();
        });
        slots.into_iter().map(|slot| slot.into_inner().unwrap()).collect()
    };
    ctx.check()?;
    let mut groups: HashMap<(Vec<u8>, u32), Vec<ObjectId>> = HashMap::new();
    for (id, sum) in candidates.into_iter().zip(sums) {
        if let Some(sum) = sum {
            groups.entry((keys[&id].clone(), sum)).or_default().push(id);
        }
    }
    let mut groups: Vec<Vec<ObjectId>> = groups.into_values().filter(|g| g.len() > 1).collect();
    groups.sort();
    for group in groups {
        let mut seen: HashMap<[u8; 32], ObjectId> = HashMap::new();
        for id in group {
            ctx.check()?;
            let Object::Stream(s) = &doc.objects[&id] else { continue };
            let Some((key, data)) = stream_identity(ctx, doc, s) else { continue };
            let sum = digest(&key, &data);
            let Some(&original) = seen.get(&sum) else {
                seen.insert(sum, id);
                continue;
            };
            let Object::Stream(other) = &doc.objects[&original] else { continue };
            let Some((other_key, other_data)) = stream_identity(ctx, doc, other) else { continue };
            if key != other_key || data != other_data {
                continue;
            }
            // Keep the smallest encoding, not whichever object happens to come first.
            let (duplicate, retained) = if s.content.len() < other.content.len() {
                seen.insert(sum, id);
                (original, id)
            } else {
                (id, original)
            };
            replace.insert(duplicate, retained);
        }
    }
    if replace.is_empty() {
        return Ok(());
    }
    // Later candidates can supersede earlier representatives. Compress the chains
    // once before rewriting references, rather than leaving dangling aliases.
    let resolved: HashMap<ObjectId, ObjectId> = replace
        .keys()
        .map(|&start| {
            let mut target = replace[&start];
            let mut hops = 0;
            while let Some(&next) = replace.get(&target) {
                target = next;
                hops += 1;
                if hops > replace.len() {
                    break;
                }
            }
            (start, target)
        })
        .collect();
    for obj in doc.objects.values_mut() {
        ctx.check()?;
        let mut stack: Vec<&mut Object> = vec![obj];
        while let Some(obj) = stack.pop() {
            match obj {
                Object::Reference(id) => {
                    if let Some(target) = resolved.get(id) {
                        *id = *target;
                    }
                }
                Object::Array(items) => stack.extend(items.iter_mut()),
                Object::Dictionary(d) => stack.extend(d.iter_mut().map(|(_, v)| v)),
                Object::Stream(s) => stack.extend(s.dict.iter_mut().map(|(_, v)| v)),
                _ => {}
            }
        }
    }
    for (key, value) in doc.trailer.iter_mut() {
        if key.as_slice() != b"ID"
            && let Object::Reference(id) = value
            && let Some(target) = resolved.get(id)
        {
            *id = *target;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_strings() {
        let date = pdf_date_now();
        assert!(date.starts_with("D:20") && date.ends_with('Z') && date.len() == 17, "{date}");
        assert_eq!(pdf_string("A"), Object::String(vec![0xfe, 0xff, 0, b'A'], StringFormat::Hexadecimal));
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        crate::image_encoding::deflate_samples(&Ctx::background(), data, false, 0).unwrap()
    }

    #[test]
    fn dedup_mixed_filters_keeps_smallest() {
        let data = b"equivalent stream samples\n".repeat(256);
        let compressed = deflate(&data);
        let hex = |d: &[u8]| {
            let mut s: Vec<u8> = d.iter().flat_map(|b| format!("{b:02x}").into_bytes()).collect();
            s.push(b'>');
            s
        };
        let mut doc = Document::with_version("1.7");
        let mut refs = Vec::new();
        let mut add = |doc: &mut Document, filters: &[&str], content: Vec<u8>, extra: Option<(&str, Object)>| {
            let mut dict = Dictionary::new();
            dict.set("TestGroup", Object::Name(b"mixed".to_vec()));
            match filters {
                [] => {}
                [one] => dict.set("Filter", Object::Name(one.as_bytes().to_vec())),
                many => dict.set("Filter", Object::Array(many.iter().map(|f| Object::Name(f.as_bytes().to_vec())).collect())),
            }
            if let Some((k, v)) = extra {
                dict.set(k, v);
            }
            let id = doc.add_object(Stream::new(dict, content));
            refs.push(id);
        };
        add(&mut doc, &[], data.clone(), None);
        add(&mut doc, &["ASCIIHexDecode"], hex(&data), None);
        add(&mut doc, &["FlateDecode"], compressed.clone(), None);
        add(&mut doc, &["ASCIIHexDecode", "FlateDecode"], hex(&compressed), None);
        add(&mut doc, &[], data.clone(), None);
        for transform in [0, 1] {
            let mut parms = Dictionary::new();
            parms.set("ColorTransform", transform);
            add(&mut doc, &["DCTDecode"], compressed.clone(), Some(("DecodeParms", Object::Dictionary(parms))));
        }
        let mut catalog = Dictionary::new();
        catalog.set("TestStreams", Object::Array(refs.iter().map(|&id| Object::Reference(id)).collect()));
        let root = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(root));
        deduplicate_streams(&Ctx::background(), &mut doc, true, 512).unwrap();
        let Object::Dictionary(catalog) = &doc.objects[&root] else { panic!() };
        let Ok(Object::Array(after)) = catalog.get(b"TestStreams") else { panic!() };
        let after: Vec<ObjectId> = after.iter().map(|o| o.as_reference().unwrap()).collect();
        for (i, id) in after[..5].iter().enumerate() {
            assert_eq!(*id, refs[2], "stream {i} did not select the smallest encoding");
        }
        assert!(after[5] != after[6] && after[5] != refs[2] && after[6] != refs[2]);
    }
}
