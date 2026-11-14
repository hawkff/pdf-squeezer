//! Loading, checking and writing documents. lopdf parses; the writer here packs
//! object streams, compresses the cross-reference stream and encrypts with AES-256.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::Path;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use lopdf::encryption::EncryptionState;
use lopdf::xref::{Xref, XrefEntry};
use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId, StringFormat};

use crate::bail;
use crate::error::{Ctx, Error, Result, path_error};

const MAX_DEREF: usize = 32;

/// Follows references. Missing objects read as absent, as the PDF format defines.
pub fn deref<'a>(doc: &'a Document, mut obj: &'a Object) -> Option<&'a Object> {
    for _ in 0..MAX_DEREF {
        match obj {
            Object::Reference(id) => obj = doc.objects.get(id)?,
            _ => return Some(obj),
        }
    }
    None
}

/// The dictionary of a dictionary or stream object.
pub fn dict_of(obj: &Object) -> Option<&Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

pub fn dict_of_mut(obj: &mut Object) -> Option<&mut Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&mut s.dict),
        _ => None,
    }
}

pub fn get<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    deref(doc, dict.get(key).ok()?).filter(|o| !o.is_null())
}

pub fn get_dict<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    match get(doc, dict, key)? {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

pub fn get_name<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    match get(doc, dict, key)? {
        Object::Name(n) => Some(n),
        _ => None,
    }
}

/// A direct integer entry.
pub fn int_entry(dict: &Dictionary, key: &[u8]) -> Option<i64> {
    match dict.get(key).ok()? {
        Object::Integer(i) => Some(*i),
        _ => None,
    }
}

/// A direct name entry.
pub fn name_entry<'a>(dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    match dict.get(key).ok()? {
        Object::Name(n) => Some(n),
        _ => None,
    }
}

pub fn number(obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

pub fn root_id(doc: &Document) -> Option<ObjectId> {
    doc.trailer.get(b"Root").ok()?.as_reference().ok()
}

pub fn info_id(doc: &Document) -> Option<ObjectId> {
    doc.trailer.get(b"Info").ok()?.as_reference().ok()
}

pub fn catalog(doc: &Document) -> Option<&Dictionary> {
    match doc.objects.get(&root_id(doc)?)? {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

pub fn catalog_mut(doc: &mut Document) -> Option<&mut Dictionary> {
    let id = root_id(doc)?;
    match doc.objects.get_mut(&id)? {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// Pushes the references a direct object holds.
pub fn references(obj: &Object, out: &mut Vec<ObjectId>) {
    let mut stack = vec![obj];
    while let Some(obj) = stack.pop() {
        match obj {
            Object::Reference(id) => out.push(*id),
            Object::Array(items) => stack.extend(items.iter()),
            Object::Dictionary(d) => stack.extend(d.iter().map(|(_, v)| v)),
            Object::Stream(s) => stack.extend(s.dict.iter().map(|(_, v)| v)),
            _ => {}
        }
    }
}

/// Objects reachable from the trailer's Root and Info.
pub fn reachable(doc: &Document) -> BTreeSet<ObjectId> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<ObjectId> = [root_id(doc), info_id(doc)].into_iter().flatten().collect();
    let mut refs = Vec::new();
    while let Some(id) = pending.pop() {
        let Some(obj) = doc.objects.get(&id) else { continue };
        if !seen.insert(id) {
            continue;
        }
        refs.clear();
        references(obj, &mut refs);
        pending.extend(refs.iter().filter(|r| !seen.contains(r)));
    }
    seen
}

/// Drops objects nothing reaches, such as replaced duplicates and stripped data.
pub fn prune(doc: &mut Document) {
    let keep = reachable(doc);
    doc.objects.retain(|id, _| keep.contains(id));
}

/// Whether the cross-reference data places an object somewhere in the file. An offset
/// outside the file names nothing, and readers treat such an object as null.
fn in_use(xref: &Xref, id: ObjectId, size: usize) -> bool {
    match xref.get(id.0) {
        Some(XrefEntry::Normal { generation, offset }) => *generation == id.1 && *offset > 0 && (*offset as usize) < size,
        Some(XrefEntry::Compressed { .. }) => id.1 == 0,
        _ => false,
    }
}

/// Fails when a reachable object exists in the cross-reference data, or in the data the
/// file was written with, but could not be read. A damaged object is never dropped
/// silently from the output.
fn check_graph(doc: &Document, written: Option<&Xref>, size: usize) -> Result<()> {
    let Some(root) = root_id(doc) else { bail!("trailer has no document catalog") };
    let mut seen = HashSet::new();
    let mut pending = vec![root];
    pending.extend(info_id(doc));
    let mut refs = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match doc.objects.get(&id) {
            Some(obj) => {
                refs.clear();
                references(obj, &mut refs);
                pending.extend(refs.iter().filter(|r| !seen.contains(r)));
            }
            None if in_use(&doc.reference_table, id, size) || written.is_some_and(|x| in_use(x, id, size)) => {
                bail!("object {} {} is damaged and cannot be read", id.0, id.1)
            }
            None => {}
        }
    }
    if catalog(doc).is_none() {
        bail!("document catalog is missing or not a dictionary");
    }
    page_ids(doc)?;
    Ok(())
}

/// The pages in document order.
pub fn page_ids(doc: &Document) -> Result<Vec<ObjectId>> {
    let catalog = catalog(doc).ok_or_else(|| Error::new("document catalog is missing"))?;
    let Some(Object::Reference(root)) = catalog.get(b"Pages").ok() else {
        bail!("document has no page tree");
    };
    let mut pages = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![*root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            bail!("page tree has a cycle at object {} {}", id.0, id.1);
        }
        let Some(Object::Dictionary(node)) = doc.objects.get(&id) else {
            bail!("page tree node {} {} is not a dictionary", id.0, id.1);
        };
        let kind = name_entry(node, b"Type");
        let kids = get(doc, node, b"Kids");
        if kind == Some(b"Pages") || kind.is_none() && kids.is_some() {
            let Some(Object::Array(kids)) = kids else { bail!("page tree node {} {} has no kids", id.0, id.1) };
            for kid in kids.iter().rev() {
                match kid {
                    Object::Reference(kid) => stack.push(*kid),
                    _ => bail!("page tree node {} {} has a direct kid", id.0, id.1),
                }
            }
        } else {
            pages.push(id);
        }
    }
    Ok(pages)
}

fn is_container(obj: &Object) -> bool {
    match obj {
        Object::Stream(s) => matches!(name_entry(&s.dict, b"Type"), Some(b"ObjStm" | b"XRef")),
        Object::Dictionary(d) => d.has(b"Linearized"),
        _ => false,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn is_space(b: u8) -> bool {
    matches!(b, 0 | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// Whether stream data ending at `end` is followed by whitespace and `endstream`.
fn ends_stream(data: &[u8], end: usize) -> bool {
    let rest = data.get(end..).unwrap_or_default();
    rest[rest.iter().take_while(|&&b| is_space(b)).count()..].starts_with(b"endstream")
}

/// The /Length a stream object starting at `object` declares, read from its dictionary.
fn declared_length(doc: &Document, object: &[u8]) -> Option<usize> {
    let keyword = find(object, b"stream")?;
    let header = lopdf::content::Content::decode(&object[..keyword + 6]).ok()?;
    let [obj, stream] = header.operations.as_slice() else { return None };
    let (true, [Object::Dictionary(dict)]) = (obj.operator == "obj" && stream.operator == "stream", stream.operands.as_slice()) else {
        return None;
    };
    let n = deref(doc, dict.get(b"Length").ok()?).and_then(number)?;
    (n >= 0.0 && n.fract() == 0.0).then_some(n as usize)
}

/// Streams whose /Length is an indirect object are read after parsing, by that
/// length alone. When the length is wrong or unreadable, the data reaches up to
/// the next `endstream` instead, as other readers recover it.
fn recover_streams(doc: &mut Document, data: &[u8]) -> Result<()> {
    let base = data.windows(5).position(|w| w == b"%PDF-").unwrap_or(0);
    let encrypted = doc.encryption_state.is_some();
    // The parser also reads up to `endstream` when more than one end-of-line follows
    // the declared data, and its /Length then counts the extra bytes. The declared
    // length holds when only whitespace is left over.
    let declared: Vec<(ObjectId, usize)> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            let Object::Stream(s) = obj else { return None };
            if encrypted || s.start_position.is_some() || !s.content.last().is_some_and(|&b| is_space(b)) {
                return None;
            }
            let Some(XrefEntry::Normal { offset, generation }) = doc.reference_table.get(id.0) else { return None };
            let n = declared_length(doc, data.get(base + *offset as usize..)?)?;
            (*generation == id.1 && n < s.content.len() && s.content[n..].iter().all(|&b| is_space(b))).then_some((*id, n))
        })
        .collect();
    for (id, n) in declared {
        if let Some(Object::Stream(s)) = doc.objects.get_mut(&id) {
            s.content.truncate(n);
            s.dict.set("Length", n as i64);
        }
    }
    for (id, obj) in doc.objects.iter_mut() {
        let Object::Stream(s) = obj else { continue };
        let Some(start) = s.start_position.and_then(|p| p.checked_add(base)) else { continue };
        if ends_stream(data, start + s.content.len()) || encrypted && !s.content.is_empty() {
            continue;
        }
        let end = data.get(start..).and_then(|rest| find(rest, b"endstream"));
        let (Some(end), false) = (end, encrypted) else {
            bail!("object {} {} has stream data that cannot be read", id.0, id.1);
        };
        let mut body = &data[start..start + end];
        body = body.strip_suffix(b"\r\n").or_else(|| body.strip_suffix(b"\n")).or_else(|| body.strip_suffix(b"\r")).unwrap_or(body);
        s.set_content(body.to_vec());
    }
    Ok(())
}

/// Parses a document and checks its structure. An encrypted document is decrypted
/// with password; a missing or wrong password fails.
pub fn load(data: &[u8], password: &str) -> Result<Document> {
    let mut doc = parse(data, password, false)?;
    let Err(err) = settle(&mut doc, data, None) else { return Ok(doc) };
    // Cross-reference data can read and still point past the objects it names. Other
    // readers then rebuild the table by scanning for objects, and so does this, as long
    // as every object either table places in the file loads.
    let written = &doc.reference_table;
    let mut rebuilt = parse(data, password, true).ok();
    if let Some(mut candidate) = rebuilt.take() {
        if settle(&mut candidate, data, Some(written)).is_ok() {
            return Ok(candidate);
        }
        rebuilt = Some(candidate);
    }
    // Other readers also skip what stands where a dictionary key belongs but is not a
    // name, such as a stray `)` or the second word of a name written with a space. Each
    // table finds the objects that did not load at its own offsets.
    for (failed, rebuild) in [Some((&doc, false)), rebuilt.as_ref().map(|r| (r, true))].into_iter().flatten() {
        if let Some(patched) = without_junk_keys(data, failed)
            && let Ok(mut repaired) = parse(&patched, password, rebuild)
            && settle(&mut repaired, &patched, rebuild.then_some(written)).is_ok()
        {
            return Ok(repaired);
        }
    }
    Err(err)
}

#[derive(Clone, Copy)]
enum Token<'a> {
    DictOpen,
    DictClose,
    ArrayOpen,
    ArrayClose,
    Name,
    Text,
    Word(&'a [u8]),
    /// A delimiter that cannot start an object here, such as `)`.
    Stray,
}

#[derive(Clone)]
struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

fn is_delimiter(b: u8) -> bool {
    b"()<>[]{}/%".contains(&b)
}

impl<'a> Lexer<'a> {
    fn next(&mut self) -> Option<(Token<'a>, Range<usize>)> {
        loop {
            match *self.data.get(self.pos)? {
                b if is_space(b) => self.pos += 1,
                b'%' => {
                    while self.data.get(self.pos).is_some_and(|&b| b != b'\n' && b != b'\r') {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let rest = &self.data[self.pos..];
        let regular = |s: &[u8]| s.iter().take_while(|&&b| !is_space(b) && !is_delimiter(b)).count();
        let (token, len) = match rest {
            [b'<', b'<', ..] => (Token::DictOpen, 2),
            [b'>', b'>', ..] => (Token::DictClose, 2),
            [b'[', ..] => (Token::ArrayOpen, 1),
            [b']', ..] => (Token::ArrayClose, 1),
            [b'(', ..] => (Token::Text, literal_len(rest)?),
            [b'<', ..] => (Token::Text, find(rest, b">")? + 1),
            [b'/', ..] => (Token::Name, 1 + regular(&rest[1..])),
            [b')' | b'>' | b'{' | b'}', ..] => (Token::Stray, 1),
            _ => {
                let n = regular(rest);
                (Token::Word(&rest[..n]), n)
            }
        };
        let start = self.pos;
        self.pos += len;
        Some((token, start..self.pos))
    }
}

/// The length of the literal string that starts `s`, with nested parentheses and escapes.
fn literal_len(s: &[u8]) -> Option<usize> {
    let (mut depth, mut i) = (0usize, 0);
    while i < s.len() {
        match s[i] {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_int(word: &[u8]) -> bool {
    !word.is_empty() && word.iter().all(u8::is_ascii_digit)
}

/// The tokens that stand where a dictionary key belongs but are not names, in the object
/// at `offset`. Anything else out of place gives no ranges, so such objects still fail.
fn junk_keys(data: &[u8], offset: usize) -> Vec<Range<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Frame {
        Array,
        Key,
        Value,
    }
    let mut lexer = Lexer { data, pos: offset };
    let header: Vec<Token> = (0..3).map_while(|_| lexer.next().map(|(t, _)| t)).collect();
    if !matches!(header.as_slice(), [Token::Word(n), Token::Word(g), Token::Word(b"obj")] if is_int(n) && is_int(g)) {
        return Vec::new();
    }
    let (mut junk, mut stack) = (Vec::new(), Vec::new());
    loop {
        let Some((token, range)) = lexer.next() else { return Vec::new() };
        match (stack.last().copied(), token) {
            (_, Token::Word(b"obj" | b"endobj" | b"stream" | b"endstream")) => return Vec::new(),
            (Some(Frame::Key), Token::Name) => {
                stack.pop();
                stack.push(Frame::Value);
                continue;
            }
            (Some(Frame::Key), Token::DictClose) | (Some(Frame::Array), Token::ArrayClose) => {
                stack.pop();
            }
            (Some(Frame::Key), Token::Word(_) | Token::Text | Token::Stray) => {
                junk.push(range);
                continue;
            }
            (Some(Frame::Key), _) => return Vec::new(),
            (_, Token::DictOpen) => {
                stack.push(Frame::Key);
                continue;
            }
            (_, Token::ArrayOpen) => {
                stack.push(Frame::Array);
                continue;
            }
            (top, Token::Word(word)) => {
                // A reference is one value of three words.
                let mut ahead = lexer.clone();
                if top == Some(Frame::Value)
                    && is_int(word)
                    && let (Some((Token::Word(generation), _)), Some((Token::Word(b"R"), _))) = (ahead.next(), ahead.next())
                    && is_int(generation)
                {
                    lexer = ahead;
                }
            }
            (_, Token::Name | Token::Text) => {}
            _ => return Vec::new(),
        }
        // A value is complete.
        match stack.last_mut() {
            None => return junk,
            Some(frame @ Frame::Value) => *frame = Frame::Key,
            Some(_) => {}
        }
    }
}

/// A copy of the file with junk dictionary keys blanked in the objects that did not load.
/// Blanks keep every offset in the file valid.
fn without_junk_keys(data: &[u8], doc: &Document) -> Option<Vec<u8>> {
    let base = data.windows(5).position(|w| w == b"%PDF-").unwrap_or(0);
    let mut objects: Vec<(usize, bool)> = doc
        .reference_table
        .entries
        .iter()
        .filter_map(|(&number, entry)| match *entry {
            XrefEntry::Normal { offset, generation } => Some((base + offset as usize, !doc.objects.contains_key(&(number, generation)))),
            _ => None,
        })
        .collect();
    objects.sort_unstable();
    let mut patched: Option<Vec<u8>> = None;
    for (i, &(offset, missing)) in objects.iter().enumerate() {
        // Each scan stops where the next object starts, so together they read the file once.
        let end = objects.get(i + 1).map_or(data.len(), |&(next, _)| next).min(data.len());
        if !missing || offset >= end {
            continue;
        }
        for range in junk_keys(&data[..end], offset) {
            patched.get_or_insert_with(|| data.to_vec())[range].fill(b' ');
        }
    }
    patched
}

fn parse(data: &[u8], password: &str, rebuild: bool) -> Result<Document> {
    let options = LoadOptions { password: Some(password.to_string()), force_reconstruct: rebuild, ..LoadOptions::default() };
    let doc = Document::load_mem_with_options(data, options).map_err(|e| match e {
        lopdf::Error::InvalidPassword => Error::new("the password is incorrect; provide it with --password-file"),
        e => Error::new(e.to_string()),
    })?;
    if doc.trailer.has(b"Encrypt") && doc.encryption_state.is_none() {
        bail!("the PDF is encrypted; provide its password with --password-file");
    }
    Ok(doc)
}

/// Drops object and cross-reference streams, recovers stream data and checks the object graph.
fn settle(doc: &mut Document, data: &[u8], written: Option<&Xref>) -> Result<()> {
    doc.objects.retain(|_, obj| !is_container(obj));
    recover_streams(doc, data)?;
    check_graph(doc, written, data.len())?;
    prune(doc);
    Ok(())
}

pub fn load_path(ctx: &Ctx, path: &Path, password: &str) -> Result<Document> {
    ctx.check()?;
    let data = std::fs::read(path).map_err(|e| path_error("open", path, &e))?;
    ctx.check()?;
    load(&data, password)
}

/// Whether the document carries a signed signature field.
pub fn signed(doc: &Document) -> bool {
    let Some(form) = catalog(doc).and_then(|c| get_dict(doc, c, b"AcroForm")) else { return false };
    let Some(Object::Array(fields)) = get(doc, form, b"Fields") else { return false };
    let mut stack: Vec<&Object> = fields.iter().collect();
    let mut seen = HashSet::new();
    while let Some(field) = stack.pop() {
        if let Object::Reference(id) = field
            && !seen.insert(*id)
        {
            continue;
        }
        let Some(Object::Dictionary(field)) = deref(doc, field) else { continue };
        if get_name(doc, field, b"FT") == Some(b"Sig") && get(doc, field, b"V").is_some() {
            return true;
        }
        if let Some(Object::Array(kids)) = get(doc, field, b"Kids") {
            stack.extend(kids.iter());
        }
    }
    false
}

pub struct WriteOptions {
    pub object_streams: bool,
    pub encryption: Option<EncryptionState>,
    /// Give the file a new identifier instead of keeping the input's.
    pub new_id: bool,
}

fn regular_start(obj: &Object) -> bool {
    matches!(obj, Object::Null | Object::Boolean(_) | Object::Integer(_) | Object::Real(_) | Object::Reference(_))
}

fn regular_end(obj: &Object) -> bool {
    matches!(obj, Object::Null | Object::Boolean(_) | Object::Integer(_) | Object::Real(_) | Object::Reference(_) | Object::Name(_))
}

pub fn write_real(out: &mut Vec<u8>, value: f32) {
    if value.is_finite() {
        out.extend_from_slice(format!("{value}").as_bytes());
    } else {
        out.push(b'0');
    }
}

pub fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &b in name {
        if !(33..=126).contains(&b) || b"()<>[]{}/%#".contains(&b) {
            out.extend_from_slice(format!("#{b:02X}").as_bytes());
        } else {
            out.push(b);
        }
    }
}

fn write_string(out: &mut Vec<u8>, text: &[u8], format: StringFormat) {
    match format {
        StringFormat::Literal => {
            out.push(b'(');
            for &b in text {
                match b {
                    b'(' | b')' | b'\\' => out.extend_from_slice(&[b'\\', b]),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    _ => out.push(b),
                }
            }
            out.push(b')');
        }
        StringFormat::Hexadecimal => {
            out.push(b'<');
            for &b in text {
                out.extend_from_slice(format!("{b:02x}").as_bytes());
            }
            out.push(b'>');
        }
    }
}

/// Serializes a direct object. References go through `map`; a reference to an
/// object that is not written becomes null.
pub fn write_object(out: &mut Vec<u8>, obj: &Object, map: &dyn Fn(ObjectId) -> Option<u32>) {
    match obj {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(true) => out.extend_from_slice(b"true"),
        Object::Boolean(false) => out.extend_from_slice(b"false"),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Real(r) => write_real(out, *r),
        Object::Name(n) => write_name(out, n),
        Object::String(text, format) => write_string(out, text, *format),
        Object::Array(items) => {
            out.push(b'[');
            let mut prev: Option<&Object> = None;
            for item in items {
                if prev.is_some_and(regular_end) && regular_start(item) {
                    out.push(b' ');
                }
                write_object(out, item, map);
                prev = Some(item);
            }
            out.push(b']');
        }
        Object::Dictionary(d) => write_dict(out, d, map, None),
        Object::Stream(s) => write_dict(out, &s.dict, map, Some(s.content.len())),
        Object::Reference(id) => match map(*id) {
            Some(n) => out.extend_from_slice(format!("{n} 0 R").as_bytes()),
            None => out.extend_from_slice(b"null"),
        },
    }
}

/// Writes a dictionary; a stream dictionary gets its actual, direct Length.
fn write_dict(out: &mut Vec<u8>, d: &Dictionary, map: &dyn Fn(ObjectId) -> Option<u32>, length: Option<usize>) {
    out.extend_from_slice(b"<<");
    let mut wrote_length = false;
    for (key, value) in d.iter() {
        write_name(out, key);
        if key.as_slice() == b"Length"
            && let Some(n) = length
        {
            out.push(b' ');
            out.extend_from_slice(n.to_string().as_bytes());
            wrote_length = true;
            continue;
        }
        if regular_start(value) {
            out.push(b' ');
        }
        write_object(out, value, map);
    }
    if let (Some(n), false) = (length, wrote_length) {
        out.extend_from_slice(format!("/Length {n}").as_bytes());
    }
    out.extend_from_slice(b">>");
}

/// A serialization with sorted keys, for comparing objects by content.
pub fn canonical(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Array(items) => {
            out.push(b'[');
            for item in items {
                canonical(item, out);
                out.push(b' ');
            }
            out.push(b']');
        }
        Object::Dictionary(d) => canonical_dict(d, out),
        Object::Stream(s) => canonical_dict(&s.dict, out),
        Object::Reference(id) => out.extend_from_slice(format!("{} {} R", id.0, id.1).as_bytes()),
        other => write_object(out, other, &|_| None),
    }
}

pub fn canonical_dict(d: &Dictionary, out: &mut Vec<u8>) {
    let mut entries: Vec<(&Vec<u8>, &Object)> = d.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    out.extend_from_slice(b"<<");
    for (key, value) in entries {
        write_name(out, key);
        out.push(b' ');
        canonical(value, out);
        out.push(b' ');
    }
    out.extend_from_slice(b">>");
}

/// The header version as major and minor numbers; anything unreadable is 1.7.
fn header_version(version: &str) -> (u32, u32) {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok());
    match (major, minor) {
        (Some(major @ 1..=2), Some(minor @ 0..=9)) => (major, minor),
        _ => (1, 7),
    }
}

fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), Compression::new(level));
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

const OBJECTS_PER_STREAM: usize = 100;

enum Entry {
    Direct(u64),
    Packed(u32, u32),
}

/// Writes the reachable objects, renumbered from 1. Non-stream objects go into object
/// streams unless the classic layout is requested.
pub fn write(ctx: &Ctx, doc: &Document, path: &Path, opts: &WriteOptions) -> Result<()> {
    let file = OpenOptions::new().write(true).truncate(true).create(true).open(path).map_err(|e| path_error("open", path, &e))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    write_to(ctx, doc, &mut w, opts)?;
    w.flush()?;
    Ok(())
}

pub fn write_to(ctx: &Ctx, doc: &Document, w: &mut dyn Write, opts: &WriteOptions) -> Result<()> {
    let ids: Vec<ObjectId> = reachable(doc).into_iter().collect();
    let numbers: HashMap<ObjectId, u32> = ids.iter().enumerate().map(|(i, id)| (*id, i as u32 + 1)).collect();
    let map = |id: ObjectId| numbers.get(&id).copied();
    let Some(root) = root_id(doc).and_then(map) else { bail!("document catalog is missing") };
    let info = info_id(doc).and_then(map);
    let mut next = ids.len() as u32 + 1;
    let encrypt_number = opts.encryption.as_ref().map(|_| {
        next += 1;
        next - 1
    });

    let (major, minor) = header_version(&doc.version);
    let version = if opts.object_streams && (major, minor) < (1, 5) { "1.5".to_string() } else { format!("{major}.{minor}") };
    let mut offset: u64 = 0;
    let emit = |w: &mut dyn Write, bytes: &[u8], offset: &mut u64| -> Result<()> {
        w.write_all(bytes)?;
        *offset += bytes.len() as u64;
        Ok(())
    };
    let mut header = format!("%PDF-{version}\n%").into_bytes();
    // Raw high bytes mark the file as binary for transfer tools.
    header.extend_from_slice(&[0xe2, 0xe3, 0xcf, 0xd3, b'\n']);
    emit(w, &header, &mut offset)?;
    let mut entries: Vec<Option<Entry>> = Vec::new();
    entries.resize_with(next as usize + 64, || None);

    let mut packed: Vec<(u32, &Object)> = Vec::new();
    let mut buffer = Vec::new();
    for id in &ids {
        ctx.check()?;
        let number = numbers[id];
        let obj = &doc.objects[id];
        if opts.object_streams && !matches!(obj, Object::Stream(_)) {
            packed.push((number, obj));
            continue;
        }
        buffer.clear();
        buffer.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        let owned;
        let obj = match &opts.encryption {
            Some(state) => {
                let mut copy = obj.clone();
                lopdf::encryption::encrypt_object(state, (number, 0), &mut copy).map_err(|e| Error::new(format!("encrypt object: {e}")))?;
                owned = copy;
                &owned
            }
            None => obj,
        };
        entries[number as usize] = Some(Entry::Direct(offset));
        match obj {
            Object::Stream(s) => {
                write_dict(&mut buffer, &s.dict, &map, Some(s.content.len()));
                buffer.extend_from_slice(b"\nstream\n");
                emit(w, &buffer, &mut offset)?;
                emit(w, &s.content, &mut offset)?;
                emit(w, b"\nendstream\nendobj\n", &mut offset)?;
            }
            other => {
                write_object(&mut buffer, other, &map);
                buffer.extend_from_slice(b"\nendobj\n");
                emit(w, &buffer, &mut offset)?;
            }
        }
    }

    for batch in packed.chunks(OBJECTS_PER_STREAM) {
        ctx.check()?;
        let container = next;
        next += 1;
        if entries.len() <= next as usize {
            entries.resize_with(next as usize + 64, || None);
        }
        let mut header = Vec::new();
        let mut body = Vec::new();
        for (index, (number, obj)) in batch.iter().enumerate() {
            header.extend_from_slice(format!("{number} {} ", body.len()).as_bytes());
            write_object(&mut body, obj, &map);
            body.push(b'\n');
            entries[*number as usize] = Some(Entry::Packed(container, index as u32));
        }
        let first = header.len();
        header.extend_from_slice(&body);
        let mut content = zlib(&header, 9);
        if let Some(state) = &opts.encryption {
            let mut stream = Object::Stream(lopdf::Stream::new(Dictionary::new(), content));
            lopdf::encryption::encrypt_object(state, (container, 0), &mut stream).map_err(|e| Error::new(format!("encrypt object stream: {e}")))?;
            let Object::Stream(s) = stream else { unreachable!() };
            content = s.content;
        }
        entries[container as usize] = Some(Entry::Direct(offset));
        let dict =
            format!("{container} 0 obj\n<</Type/ObjStm/N {}/First {first}/Filter/FlateDecode/Length {}>>\nstream\n", batch.len(), content.len());
        emit(w, dict.as_bytes(), &mut offset)?;
        emit(w, &content, &mut offset)?;
        emit(w, b"\nendstream\nendobj\n", &mut offset)?;
    }

    if let (Some(state), Some(number)) = (&opts.encryption, encrypt_number) {
        let dict = state.encode().map_err(|e| Error::new(format!("encryption dictionary: {e}")))?;
        entries[number as usize] = Some(Entry::Direct(offset));
        buffer.clear();
        buffer.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        write_object(&mut buffer, &Object::Dictionary(dict), &map);
        buffer.extend_from_slice(b"\nendobj\n");
        emit(w, &buffer, &mut offset)?;
    }

    let id = trailer_id(doc, opts.new_id);
    let mut trailer = Dictionary::new();
    trailer.set("Root", Object::Reference((root, 0)));
    if let Some(info) = info {
        trailer.set("Info", Object::Reference((info, 0)));
    }
    if let Some(number) = encrypt_number {
        trailer.set("Encrypt", Object::Reference((number, 0)));
    }
    trailer.set("ID", Object::Array(id.iter().map(|part| Object::String(part.clone(), StringFormat::Hexadecimal)).collect()));
    let identity = |id: ObjectId| Some(id.0);

    if opts.object_streams {
        let xref_number = next;
        let size = xref_number + 1;
        entries.resize_with(size as usize, || None);
        entries[xref_number as usize] = Some(Entry::Direct(offset));
        let widest = entries
            .iter()
            .flatten()
            .map(|e| match e {
                Entry::Direct(o) => *o,
                Entry::Packed(c, _) => *c as u64,
            })
            .max()
            .unwrap_or(0);
        let width = ((64 - widest.leading_zeros()).div_ceil(8)).max(1) as usize;
        let row = 1 + width + 2;
        let mut rows = Vec::with_capacity(size as usize * (row + 1));
        let mut prior = vec![0u8; row];
        let mut current = vec![0u8; row];
        for (number, entry) in entries.iter().enumerate().take(size as usize) {
            let (kind, field, extra): (u8, u64, u16) = match entry {
                _ if number == 0 => (0, 0, 65535),
                Some(Entry::Direct(o)) => (1, *o, 0),
                Some(Entry::Packed(c, i)) => (2, *c as u64, *i as u16),
                None => (0, 0, 0),
            };
            current[0] = kind;
            current[1..1 + width].copy_from_slice(&field.to_be_bytes()[8 - width..]);
            current[1 + width..].copy_from_slice(&extra.to_be_bytes());
            rows.push(2); // PNG Up predictor
            rows.extend(current.iter().zip(&prior).map(|(c, p)| c.wrapping_sub(*p)));
            std::mem::swap(&mut prior, &mut current);
        }
        let content = zlib(&rows, 9);
        trailer.set("Type", Object::Name(b"XRef".to_vec()));
        trailer.set("Size", size as i64);
        trailer.set("W", Object::Array(vec![1.into(), (width as i64).into(), 2.into()]));
        trailer.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        let mut parms = Dictionary::new();
        parms.set("Columns", row as i64);
        parms.set("Predictor", 12);
        trailer.set("DecodeParms", parms);
        buffer.clear();
        buffer.extend_from_slice(format!("{xref_number} 0 obj\n").as_bytes());
        write_dict(&mut buffer, &trailer, &identity, Some(content.len()));
        buffer.extend_from_slice(b"\nstream\n");
        let start = offset;
        emit(w, &buffer, &mut offset)?;
        emit(w, &content, &mut offset)?;
        emit(w, format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes(), &mut offset)?;
    } else {
        let size = next;
        let start = offset;
        buffer.clear();
        buffer.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for entry in entries.iter().take(size as usize).skip(1) {
            match entry {
                Some(Entry::Direct(o)) => buffer.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes()),
                _ => buffer.extend_from_slice(b"0000000000 00001 f \n"),
            }
        }
        trailer.set("Size", size as i64);
        buffer.extend_from_slice(b"trailer\n");
        write_dict(&mut buffer, &trailer, &identity, None);
        buffer.extend_from_slice(format!("\nstartxref\n{start}\n%%EOF\n").as_bytes());
        emit(w, &buffer, &mut offset)?;
    }
    Ok(())
}

/// Keeps the input's two identifiers unless a new one is requested or none exists.
fn trailer_id(doc: &Document, fresh: bool) -> [Vec<u8>; 2] {
    if !fresh
        && let Ok(Object::Array(parts)) = doc.trailer.get(b"ID")
        && let [Object::String(a, _), Object::String(b, _)] = parts.as_slice()
    {
        return [a.clone(), b.clone()];
    }
    let mut id = vec![0u8; 16];
    if getrandom::fill(&mut id).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        id = nanos.to_le_bytes().to_vec();
    }
    [id.clone(), id]
}

/// AES-256 (revision 6) with the CLI's permission policies.
pub fn aes256(user: &str, owner: &str, permissions: &str) -> Result<EncryptionState> {
    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use lopdf::{EncryptionVersion, Permissions};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    let mut key = vec![0u8; 32];
    getrandom::fill(&mut key).map_err(|e| Error::new(format!("random key: {e}")))?;
    let permissions = match permissions {
        "print" => Permissions::PRINTABLE | Permissions::PRINTABLE_IN_HIGH_QUALITY,
        "none" => Permissions::empty(),
        _ => Permissions::all(),
    };
    let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let version = EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
        file_encryption_key: &key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: owner,
        user_password: user,
        permissions,
    };
    EncryptionState::try_from(version).map_err(|e| Error::new(format!("encryption: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_separators_round_trip() {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"Page".to_vec()));
        d.set("Box", Object::Array(vec![0.into(), Object::Real(1.5), Object::Reference((7, 0)), Object::Name(b"a b".to_vec())]));
        d.set("T", Object::String(b"a(b)\\c\r".to_vec(), StringFormat::Literal));
        let mut out = Vec::new();
        write_object(&mut out, &Object::Dictionary(d), &|id| Some(id.0 + 1));
        assert_eq!(String::from_utf8_lossy(&out), "<</Type/Page/Box[0 1.5 8 0 R/a#20b]/T(a\\(b\\)\\\\c\\r)>>");
    }

    #[test]
    fn junk_key_ranges() {
        let data = b"4 0 obj\n<< /A /B,C D /E [1 (x) 2] /F << /G 1 0 R ) >> (s) /H (p(a)r\\)) % c )\n /I <ab> >>\nendobj";
        let junk: Vec<&[u8]> = junk_keys(data, 0).into_iter().map(|r| &data[r]).collect();
        assert_eq!(junk, [b"D".as_slice(), b")", b"(s)"]);
        for damaged in [b"4 0 obj << /A [1 >> endobj".as_slice(), b"4 0 obj << /A 1 endobj", b"<< /A 1 B >>", b"4 0 obj << [ ] >> endobj"] {
            assert!(junk_keys(damaged, 0).is_empty(), "{}", String::from_utf8_lossy(damaged));
        }
    }

    #[test]
    fn stream_ends_after_whitespace() {
        assert!(ends_stream(b"abc\nendstream", 3));
        assert!(ends_stream(b"abc\r\n\r\n endstream", 3));
        assert!(ends_stream(b"abcendstream", 3));
        assert!(!ends_stream(b"abc x\nendstream", 3));
        assert!(!ends_stream(b"abc", 3));
    }

    #[test]
    fn versions() {
        assert_eq!(header_version("1.7"), (1, 7));
        assert_eq!(header_version("2.0"), (2, 0));
        assert_eq!(header_version("1.4 garbage"), (1, 4));
        assert_eq!(header_version("x"), (1, 7));
    }
}
