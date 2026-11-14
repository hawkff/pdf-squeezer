//! Strict decoders for the lossless PDF stream filters, bounded by an output limit.

use std::fmt;

use flate2::{Decompress, FlushDecompress, Status};
use lopdf::{Dictionary, Document, Object};

use crate::error::Ctx;
use crate::pdf::{deref, int_entry};

#[derive(Clone, Debug)]
pub struct Filter {
    pub name: Vec<u8>,
    pub parms: Option<Dictionary>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Canceled,
    Limit,
    Unsupported(String),
    Invalid(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Canceled => f.write_str("context canceled"),
            DecodeError::Limit => f.write_str("decoded stream exceeds its size limit"),
            DecodeError::Unsupported(name) => write!(f, "unsupported filter {name}"),
            DecodeError::Invalid(why) => f.write_str(why),
        }
    }
}

type Decoded = Result<Vec<u8>, DecodeError>;

fn invalid(why: impl Into<String>) -> DecodeError {
    DecodeError::Invalid(why.into())
}

/// Expands the abbreviations that inline images use.
fn full_name(name: &[u8]) -> Vec<u8> {
    let full: &[u8] = match name {
        b"AHx" => b"ASCIIHexDecode",
        b"A85" => b"ASCII85Decode",
        b"LZW" => b"LZWDecode",
        b"Fl" => b"FlateDecode",
        b"RL" => b"RunLengthDecode",
        b"CCF" => b"CCITTFaxDecode",
        b"DCT" => b"DCTDecode",
        other => other,
    };
    full.to_vec()
}

/// Reads /Filter and /DecodeParms, resolving indirect values.
pub fn pipeline(doc: &Document, dict: &Dictionary) -> Result<Vec<Filter>, String> {
    let Some(filter) = dict.get(b"Filter").ok().and_then(|f| deref(doc, f)) else {
        return Ok(Vec::new());
    };
    let names: Vec<Vec<u8>> = match filter {
        Object::Name(name) => vec![full_name(name)],
        Object::Array(items) => items
            .iter()
            .map(|item| match deref(doc, item) {
                Some(Object::Name(name)) => Ok(full_name(name)),
                _ => Err("filter entry is not a name".to_string()),
            })
            .collect::<Result<_, _>>()?,
        Object::Null => Vec::new(),
        _ => return Err("filter is not a name or array".into()),
    };
    let parms = dict.get(b"DecodeParms").ok().and_then(|p| deref(doc, p));
    let mut list: Vec<Option<Dictionary>> = vec![None; names.len()];
    match parms {
        None | Some(Object::Null) => {}
        Some(Object::Dictionary(d)) => {
            if let Some(first) = list.first_mut() {
                *first = Some(resolve_dict(doc, d));
            }
        }
        Some(Object::Array(items)) => {
            for (slot, item) in list.iter_mut().zip(items) {
                match deref(doc, item) {
                    Some(Object::Dictionary(d)) => *slot = Some(resolve_dict(doc, d)),
                    None | Some(Object::Null) => {}
                    _ => return Err("decode parameters entry is not a dictionary".into()),
                }
            }
        }
        _ => return Err("decode parameters are not a dictionary or array".into()),
    }
    Ok(names.into_iter().zip(list).map(|(name, parms)| Filter { name, parms }).collect())
}

/// A copy whose top-level values are direct, so decoders need no document.
fn resolve_dict(doc: &Document, dict: &Dictionary) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in dict.iter() {
        let value = deref(doc, value).cloned().unwrap_or(Object::Null);
        out.set(key.clone(), value);
    }
    out
}

fn parm(parms: Option<&Dictionary>, key: &[u8], default: i64) -> i64 {
    parms.and_then(|p| int_entry(p, key)).unwrap_or(default)
}

fn bool_parm(parms: Option<&Dictionary>, key: &[u8], default: bool) -> bool {
    match parms.and_then(|p| p.get(key).ok()) {
        Some(Object::Boolean(b)) => *b,
        _ => default,
    }
}

/// Applies every filter in order; each stage's output is bounded by limit.
pub fn decode(ctx: &Ctx, raw: &[u8], filters: &[Filter], limit: usize) -> Decoded {
    let mut data: Option<Vec<u8>> = None;
    for filter in filters {
        if ctx.done() {
            return Err(DecodeError::Canceled);
        }
        let input = data.as_deref().unwrap_or(raw);
        let parms = filter.parms.as_ref();
        let output = match filter.name.as_slice() {
            b"FlateDecode" => unpredict(inflate(ctx, input, limit)?, parms, limit)?,
            b"LZWDecode" => unpredict(lzw(input, parms, limit)?, parms, limit)?,
            b"ASCIIHexDecode" => ascii_hex(input, limit)?,
            b"ASCII85Decode" => ascii85(input, limit)?,
            b"RunLengthDecode" => run_length(input, limit)?,
            b"CCITTFaxDecode" => ccitt(input, parms, limit)?,
            // The document was decrypted on load; an Identity crypt filter changes nothing.
            b"Crypt" if parms.and_then(|p| p.get(b"Name").ok()).is_none_or(|n| n.as_name().ok() == Some(b"Identity")) => input.to_vec(),
            other => return Err(DecodeError::Unsupported(String::from_utf8_lossy(other).into_owned())),
        };
        data = Some(output);
    }
    Ok(data.unwrap_or_else(|| raw.to_vec()))
}

/// Inflates a zlib stream. A corrupt stream or checksum fails; a stream that ends
/// early returns what it produced, and callers check the length they expect.
pub fn inflate(ctx: &Ctx, raw: &[u8], limit: usize) -> Decoded {
    if raw.is_empty() {
        return Err(invalid("empty Flate stream"));
    }
    let mut d = Decompress::new(true);
    let mut out: Vec<u8> = Vec::with_capacity(raw.len().saturating_mul(4).max(64).min(limit.saturating_add(1)));
    loop {
        if ctx.done() {
            return Err(DecodeError::Canceled);
        }
        if out.len() == out.capacity() {
            if out.len() > limit {
                return Err(DecodeError::Limit);
            }
            let grow = out.capacity().max(64 << 10).min(limit + 1 - out.len());
            out.reserve_exact(grow.max(1));
        }
        let consumed = d.total_in() as usize;
        let before = out.len();
        let status = d.decompress_vec(&raw[consumed..], &mut out, FlushDecompress::None).map_err(|e| invalid(format!("flate: {e}")))?;
        if out.len() > limit {
            return Err(DecodeError::Limit);
        }
        match status {
            Status::StreamEnd => return Ok(out),
            Status::Ok | Status::BufError => {
                let progressed = d.total_in() as usize != consumed || out.len() != before;
                if !progressed && out.len() < out.capacity() {
                    // The input ended before the stream did.
                    return Ok(out);
                }
            }
        }
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reverses PNG (10-15) and TIFF (2) predictors.
pub fn unpredict(data: Vec<u8>, parms: Option<&Dictionary>, limit: usize) -> Decoded {
    let predictor = parm(parms, b"Predictor", 1);
    if predictor == 1 {
        return Ok(data);
    }
    let colors = parm(parms, b"Colors", 1);
    let bits = parm(parms, b"BitsPerComponent", 8);
    let columns = parm(parms, b"Columns", 1);
    if !(1..=32).contains(&colors) || ![1, 2, 4, 8, 16].contains(&bits) || !(1..=1 << 24).contains(&columns) {
        return Err(invalid("invalid predictor parameters"));
    }
    let row = ((colors * bits * columns + 7) / 8) as usize;
    let bpp = ((colors * bits + 7) / 8) as usize;
    match predictor {
        2 => tiff_predictor(data, row, colors as usize, bits as usize),
        10..=15 => {
            // A declared row can be far longer than the data; size buffers by what exists.
            let width = row.min(data.len());
            let mut out = Vec::with_capacity(data.len());
            let mut prior = vec![0u8; width];
            for chunk in data.chunks(row + 1) {
                let kind = chunk[0];
                let mut current = chunk[1..].to_vec();
                for i in 0..current.len() {
                    let left = if i >= bpp { current[i - bpp] } else { 0 };
                    let up = prior[i];
                    let corner = if i >= bpp { prior[i - bpp] } else { 0 };
                    current[i] = current[i].wrapping_add(match kind {
                        0 => 0,
                        1 => left,
                        2 => up,
                        3 => ((left as u16 + up as u16) / 2) as u8,
                        4 => paeth(left, up, corner),
                        _ => return Err(invalid(format!("invalid PNG predictor filter type {kind}"))),
                    });
                }
                out.extend_from_slice(&current);
                if out.len() > limit {
                    return Err(DecodeError::Limit);
                }
                prior[..current.len()].copy_from_slice(&current);
            }
            Ok(out)
        }
        _ => Err(invalid(format!("unsupported predictor {predictor}"))),
    }
}

fn tiff_predictor(mut data: Vec<u8>, row: usize, colors: usize, bits: usize) -> Decoded {
    for line in data.chunks_mut(row) {
        match bits {
            8 => {
                for i in colors..line.len() {
                    line[i] = line[i].wrapping_add(line[i - colors]);
                }
            }
            16 => {
                let samples = line.len() / 2;
                for i in colors..samples {
                    let prev = u16::from_be_bytes([line[2 * (i - colors)], line[2 * (i - colors) + 1]]);
                    let cur = u16::from_be_bytes([line[2 * i], line[2 * i + 1]]).wrapping_add(prev);
                    line[2 * i..2 * i + 2].copy_from_slice(&cur.to_be_bytes());
                }
            }
            _ => {
                let samples = line.len() * 8 / bits;
                let mask = (1u16 << bits) - 1;
                let get = |line: &[u8], i: usize| -> u16 {
                    let bit = i * bits;
                    ((line[bit / 8] as u16) >> (8 - bits - bit % 8)) & mask
                };
                for i in colors..samples {
                    let value = (get(line, i) + get(line, i - colors)) & mask;
                    let bit = i * bits;
                    let shift = 8 - bits - bit % 8;
                    line[bit / 8] = (line[bit / 8] & !((mask as u8) << shift)) | ((value as u8) << shift);
                }
            }
        }
    }
    Ok(data)
}

fn lzw(raw: &[u8], parms: Option<&Dictionary>, limit: usize) -> Decoded {
    use weezl::{BitOrder, LzwStatus, decode::Decoder};
    let mut decoder =
        if parm(parms, b"EarlyChange", 1) != 0 { Decoder::with_tiff_size_switch(BitOrder::Msb, 8) } else { Decoder::new(BitOrder::Msb, 8) };
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    let mut input = raw;
    loop {
        let result = decoder.decode_bytes(input, &mut chunk);
        input = &input[result.consumed_in..];
        out.extend_from_slice(&chunk[..result.consumed_out]);
        if out.len() > limit {
            return Err(DecodeError::Limit);
        }
        match result.status {
            Ok(LzwStatus::Done) => return Ok(out),
            Ok(LzwStatus::NoProgress) => return Ok(out),
            Ok(LzwStatus::Ok) => {
                if input.is_empty() && result.consumed_out == 0 {
                    return Ok(out);
                }
            }
            Err(e) => return Err(invalid(format!("lzw: {e}"))),
        }
    }
}

fn ascii_hex(raw: &[u8], limit: usize) -> Decoded {
    let mut out = Vec::with_capacity(raw.len() / 2);
    let mut high: Option<u8> = None;
    for &b in raw {
        let value = match b {
            b'>' => break,
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            b if b.is_ascii_whitespace() || b == 0 => continue,
            _ => return Err(invalid("invalid ASCIIHex data")),
        };
        match high.take() {
            Some(h) => out.push(h << 4 | value),
            None => high = Some(value),
        }
        if out.len() > limit {
            return Err(DecodeError::Limit);
        }
    }
    if let Some(h) = high {
        out.push(h << 4);
    }
    Ok(out)
}

fn ascii85(raw: &[u8], limit: usize) -> Decoded {
    let mut out = Vec::with_capacity(raw.len() * 4 / 5);
    let mut group = [0u8; 5];
    let mut count = 0;
    let body = raw.strip_prefix(b"<~").unwrap_or(raw);
    for &b in body {
        match b {
            b'~' => break,
            b'z' if count == 0 => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group[count] = b - b'!';
                count += 1;
                if count == 5 {
                    let value = group.iter().try_fold(0u32, |acc, &d| acc.checked_mul(85).and_then(|a| a.checked_add(d as u32)));
                    out.extend_from_slice(&value.ok_or_else(|| invalid("ASCII85 group overflows"))?.to_be_bytes());
                    count = 0;
                }
            }
            b if b.is_ascii_whitespace() || b == 0 => continue,
            _ => return Err(invalid("invalid ASCII85 data")),
        }
        if out.len() > limit {
            return Err(DecodeError::Limit);
        }
    }
    match count {
        0 => {}
        1 => return Err(invalid("ASCII85 data ends with a single character")),
        n => {
            for slot in group.iter_mut().skip(n) {
                *slot = 84;
            }
            let value = group.iter().try_fold(0u32, |acc, &d| acc.checked_mul(85).and_then(|a| a.checked_add(d as u32)));
            out.extend_from_slice(&value.ok_or_else(|| invalid("ASCII85 group overflows"))?.to_be_bytes()[..n - 1]);
        }
    }
    Ok(out)
}

fn run_length(raw: &[u8], limit: usize) -> Decoded {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let n = raw[i] as usize;
        i += 1;
        match n {
            128 => break,
            0..=127 => {
                let end = (i + n + 1).min(raw.len());
                out.extend_from_slice(&raw[i..end]);
                i = end;
            }
            _ => {
                if i < raw.len() {
                    out.extend(std::iter::repeat_n(raw[i], 257 - n));
                    i += 1;
                }
            }
        }
        if out.len() > limit {
            return Err(DecodeError::Limit);
        }
    }
    Ok(out)
}

struct Rows {
    columns: usize,
    row_bytes: usize,
    black_is_one: bool,
    out: Vec<u8>,
    limit: usize,
    over: bool,
}

impl hayro_ccitt::Decoder for Rows {
    fn push_row(&mut self, changes: &[u32]) {
        if self.over {
            return;
        }
        let (white, black) = if self.black_is_one { (0x00u8, 0xffu8) } else { (0xffu8, 0x00u8) };
        let start = self.out.len();
        self.out.resize(start + self.row_bytes, white);
        let row = &mut self.out[start..];
        let mut runs: Vec<(usize, usize)> = changes.chunks(2).map(|c| (c[0] as usize, c.get(1).map_or(self.columns, |&e| e as usize))).collect();
        runs.retain(|(a, b)| a < b);
        for (from, to) in runs {
            for x in from..to.min(self.columns) {
                let mask = 0x80u8 >> (x % 8);
                row[x / 8] = (row[x / 8] & !mask) | (black & mask);
            }
        }
        if !self.columns.is_multiple_of(8) {
            // Padding bits keep the white value, as other decoders write them.
            let last = self.row_bytes - 1;
            let keep = 0xffu8 << (8 - self.columns % 8);
            row[last] = (row[last] & keep) | (white & !keep);
        }
        if self.out.len() > self.limit {
            self.over = true;
        }
    }
}

fn ccitt(raw: &[u8], parms: Option<&Dictionary>, limit: usize) -> Decoded {
    use hayro_ccitt::{DecodeSettings, DecoderContext, EncodingMode};
    let k = parm(parms, b"K", 0);
    let columns = parm(parms, b"Columns", 1728);
    let rows = parm(parms, b"Rows", 0);
    if !(1..=1 << 20).contains(&columns) || !(0..=1 << 24).contains(&rows) {
        return Err(invalid("invalid CCITT parameters"));
    }
    let settings = DecodeSettings {
        columns: columns as u32,
        rows: rows as u32,
        end_of_block: bool_parm(parms, b"EndOfBlock", true),
        end_of_line: bool_parm(parms, b"EndOfLine", false),
        rows_are_byte_aligned: bool_parm(parms, b"EncodedByteAlign", false),
        encoding: match k {
            k if k < 0 => EncodingMode::Group4,
            0 => EncodingMode::Group3_1D,
            _ => EncodingMode::Group3_2D,
        },
        strict: false,
    };
    let mut rows_out = Rows {
        columns: columns as usize,
        row_bytes: (columns as usize).div_ceil(8),
        black_is_one: bool_parm(parms, b"BlackIs1", false),
        out: Vec::new(),
        limit,
        over: false,
    };
    let mut context = DecoderContext::new(settings);
    hayro_ccitt::decode(raw, &mut rows_out, &mut context).map_err(|e| invalid(format!("ccitt: {e:?}")))?;
    if rows_out.over {
        return Err(DecodeError::Limit);
    }
    Ok(rows_out.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn inflate_strictness() {
        let ctx = Ctx::background();
        let data: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let good = deflate(&data);
        assert_eq!(inflate(&ctx, &good, 1 << 20).unwrap(), data);
        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(inflate(&ctx, &bad, 1 << 20).is_err());
        assert_eq!(inflate(&ctx, &good[..good.len() - 2], 1 << 20).unwrap(), data);
        assert_eq!(inflate(&ctx, &good, 100), Err(DecodeError::Limit));
        assert!(inflate(&ctx, &[], 10).is_err());
    }

    #[test]
    fn text_filters() {
        assert_eq!(ascii_hex(b"48 65 6c6C6f>", 100).unwrap(), b"Hello");
        assert_eq!(ascii_hex(b"414>", 100).unwrap(), b"A@");
        assert!(ascii_hex(b"4g>", 100).is_err());
        assert_eq!(ascii85(b"87cURDZ~>", 100).unwrap(), b"Hello");
        assert_eq!(ascii85(b"z~>", 100).unwrap(), [0, 0, 0, 0]);
        assert!(ascii85(b"uuuuu~>", 100).is_err());
        assert_eq!(run_length(&[2, b'a', b'b', b'c', 254, b'x', 128], 100).unwrap(), b"abcxxx");
    }

    #[test]
    fn png_and_tiff_predictors() {
        let mut parms = Dictionary::new();
        parms.set("Predictor", 12);
        parms.set("Columns", 3);
        // Two Up-filtered rows: the second adds the first.
        let data = vec![2, 1, 2, 3, 2, 1, 1, 1];
        assert_eq!(unpredict(data, Some(&parms), 100).unwrap(), vec![1, 2, 3, 2, 3, 4]);
        parms.set("Predictor", 2);
        assert_eq!(unpredict(vec![1, 1, 1, 5, 1, 1], Some(&parms), 100).unwrap(), vec![1, 2, 3, 5, 6, 7]);
    }

    #[test]
    fn lzw_round_trip() {
        let data = b"-----A---B";
        let encoded = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap();
        assert_eq!(lzw(&encoded, None, 100).unwrap(), data);
    }
}
