//! PNG-predicted and plain Flate encodings of image samples, plus bounded decoding.

use std::fmt;
use std::io::{self, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::error::Ctx;
use crate::filters::{self, DecodeError, Filter};

pub const MAX_IMAGE_BYTES: usize = 256 << 20;

#[derive(Debug)]
pub struct Larger;

impl fmt::Display for Larger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("encoded candidate exceeds the current winner")
    }
}

impl std::error::Error for Larger {}

#[derive(Debug)]
pub enum EncodeError {
    /// The candidate grew past the size it had to beat.
    Larger,
    Canceled,
    Failed(String),
}

impl From<io::Error> for EncodeError {
    fn from(e: io::Error) -> Self {
        if e.get_ref().is_some_and(|inner| inner.is::<Larger>()) {
            EncodeError::Larger
        } else if e.get_ref().is_some_and(|inner| inner.is::<Canceled>()) {
            EncodeError::Canceled
        } else {
            EncodeError::Failed(e.to_string())
        }
    }
}

#[derive(Debug)]
struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("context canceled")
    }
}

impl std::error::Error for Canceled {}

/// Collects compressed bytes up to limit; zero means no competing candidate.
struct Bounded {
    data: Vec<u8>,
    limit: usize,
}

impl Write for Bounded {
    fn write(&mut self, p: &[u8]) -> io::Result<usize> {
        if self.limit > 0 && p.len() > self.limit - self.data.len() {
            return Err(io::Error::other(Larger));
        }
        self.data.extend_from_slice(p);
        Ok(p.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn level(best: bool) -> Compression {
    if best { Compression::best() } else { Compression::default() }
}

/// Feeds an encoder in large chunks and checks for cancellation between them.
struct Chunked<W: Write> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: Write> Chunked<W> {
    fn new(inner: W) -> Self {
        Chunked { inner, pending: Vec::with_capacity(CHUNK) }
    }

    fn push(&mut self, ctx: &Ctx, bytes: &[u8]) -> Result<(), EncodeError> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= CHUNK {
            self.drain(ctx)?;
        }
        Ok(())
    }

    fn drain(&mut self, ctx: &Ctx) -> Result<(), EncodeError> {
        if ctx.done() {
            return Err(EncodeError::Canceled);
        }
        self.inner.write_all(&self.pending)?;
        self.pending.clear();
        Ok(())
    }
}

const CHUNK: usize = 256 << 10;

/// Compresses raw samples with zlib; output past limit fails with Larger.
pub fn deflate_samples(ctx: &Ctx, samples: &[u8], best: bool, limit: usize) -> Result<Vec<u8>, EncodeError> {
    let encoder = ZlibEncoder::new(Bounded { data: Vec::new(), limit }, level(best));
    let mut chunked = Chunked::new(encoder);
    for part in samples.chunks(CHUNK) {
        chunked.push(ctx, part)?;
    }
    chunked.drain(ctx)?;
    Ok(chunked.inner.finish()?.data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PngKind {
    Gray8,
    Gray16,
    Rgb8,
    Rgb16,
    /// 8-bit samples that are all 0 or 255, packed to one bit per pixel.
    Bilevel,
}

fn abs8(d: u8) -> u32 {
    if d < 128 { d as u32 } else { 256 - d as u32 }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let pc = c as i32;
    let pa = b as i32 - pc;
    let pb = a as i32 - pc;
    let pc = (pa + pb).abs();
    let (pa, pb) = (pa.abs(), pb.abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Picks the PNG filter with the smallest sum of absolute differences, trying them
/// in the order of the Go encoder (Up, Paeth, None, Sub, Average). Fills rows[f]
/// with the filtered bytes of the chosen filter f.
fn choose_filter(current: &[u8], prior: &[u8], bpp: usize, rows: &mut [Vec<u8>; 5]) -> usize {
    let n = current.len();
    let [_, sub, up, average, paeth_row] = rows;
    let mut sum = 0u32;
    for i in 0..n {
        up[i] = current[i].wrapping_sub(prior[i]);
        sum += abs8(up[i]);
    }
    let mut best = sum;
    let mut filter = 2;

    sum = 0;
    let lead = bpp.min(n);
    for i in 0..lead {
        paeth_row[i] = current[i].wrapping_sub(prior[i]);
        sum += abs8(paeth_row[i]);
    }
    for i in bpp..n {
        paeth_row[i] = current[i].wrapping_sub(paeth(current[i - bpp], prior[i], prior[i - bpp]));
        sum += abs8(paeth_row[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = 4;
    }

    sum = 0;
    for &b in current {
        sum += abs8(b);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = 0;
    }

    sum = 0;
    for i in 0..lead {
        sub[i] = current[i];
        sum += abs8(sub[i]);
    }
    for i in bpp..n {
        sub[i] = current[i].wrapping_sub(current[i - bpp]);
        sum += abs8(sub[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        best = sum;
        filter = 1;
    }

    sum = 0;
    for i in 0..lead {
        average[i] = current[i].wrapping_sub(prior[i] / 2);
        sum += abs8(average[i]);
    }
    for i in bpp..n {
        average[i] = current[i].wrapping_sub(((current[i - bpp] as u32 + prior[i] as u32) / 2) as u8);
        sum += abs8(average[i]);
        if sum >= best {
            break;
        }
    }
    if sum < best {
        filter = 3;
    }
    filter
}

/// The zlib stream of a PNG image's IDAT chunks, which is exactly what
/// FlateDecode with /Predictor 15 expects. Output past limit fails with Larger.
pub fn png_idat(ctx: &Ctx, samples: &[u8], width: usize, height: usize, kind: PngKind, best: bool, limit: usize) -> Result<Vec<u8>, EncodeError> {
    let (row_len, bpp) = match kind {
        PngKind::Gray8 => (width, 1),
        PngKind::Gray16 => (2 * width, 2),
        PngKind::Rgb8 => (3 * width, 3),
        PngKind::Rgb16 => (6 * width, 6),
        PngKind::Bilevel => (width.div_ceil(8), 1),
    };
    let encoder = ZlibEncoder::new(Bounded { data: Vec::new(), limit }, level(best));
    let mut out = Chunked::new(encoder);
    let mut prior = vec![0u8; row_len];
    let mut packed = vec![0u8; if kind == PngKind::Bilevel { row_len } else { 0 }];
    let mut rows: [Vec<u8>; 5] = std::array::from_fn(|_| vec![0u8; row_len]);
    for y in 0..height {
        let current: &[u8] = if kind == PngKind::Bilevel {
            packed.fill(0);
            for (x, &v) in samples[y * width..(y + 1) * width].iter().enumerate() {
                if v != 0 {
                    packed[x / 8] |= 0x80 >> (x % 8);
                }
            }
            &packed
        } else {
            &samples[y * row_len..(y + 1) * row_len]
        };
        // Paletted images keep the None filter, as the Go encoder writes them.
        if kind == PngKind::Bilevel {
            out.push(ctx, &[0])?;
            out.push(ctx, current)?;
        } else {
            let filter = choose_filter(current, &prior, bpp, &mut rows);
            out.push(ctx, &[filter as u8])?;
            if filter == 0 {
                out.push(ctx, current)?;
            } else {
                out.push(ctx, &rows[filter])?;
            }
            prior.copy_from_slice(current);
        }
    }
    out.drain(ctx)?;
    Ok(out.inner.finish()?.data)
}

/// Decodes an image's samples. A plain Flate stream is inflated into a bounded
/// buffer; other pipelines decode stage by stage with a limit on each stage.
pub fn image_samples(ctx: &Ctx, raw: &[u8], pipeline: &[Filter], expected: usize, rows: usize) -> Result<Vec<u8>, DecodeError> {
    if ctx.done() {
        return Err(DecodeError::Canceled);
    }
    if expected == 0 || expected > MAX_IMAGE_BYTES {
        return Err(DecodeError::Invalid("image sample size exceeds limit".into()));
    }
    if let [filter] = pipeline
        && filter.name == b"FlateDecode"
        && filter.parms.as_ref().and_then(|p| p.get(b"Predictor").ok()).is_none_or(|p| matches!(p, lopdf::Object::Integer(1)))
    {
        return filters::inflate(ctx, raw, expected).map_err(|e| match e {
            DecodeError::Limit => DecodeError::Invalid("image has excess samples".into()),
            other => other,
        });
    }
    filters::decode(ctx, raw, pipeline, MAX_IMAGE_BYTES.min(2 * (expected + rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inflate(data: &[u8]) -> Vec<u8> {
        filters::inflate(&Ctx::background(), data, 1 << 26).unwrap()
    }

    /// Reverses the PNG filters to check that each row decodes to its samples.
    fn unfilter(data: &[u8], row_len: usize, bpp: usize) -> Vec<u8> {
        let mut parms = lopdf::Dictionary::new();
        parms.set("Predictor", 15);
        parms.set("Colors", bpp as i64);
        parms.set("Columns", (row_len / bpp) as i64);
        filters::unpredict(data.to_vec(), Some(&parms), 1 << 26).unwrap()
    }

    #[test]
    fn png_rows_round_trip_and_bounds() {
        let ctx = Ctx::background();
        let (w, h) = (97, 31);
        for (kind, bpp, bytes) in [(PngKind::Gray8, 1, 1), (PngKind::Gray16, 2, 2), (PngKind::Rgb8, 3, 3), (PngKind::Rgb16, 6, 6)] {
            let samples: Vec<u8> = (0..w * h * bytes).map(|i| ((i as u32).wrapping_mul(2654435761) >> 17) as u8 / 4 * 4).collect();
            for best in [false, true] {
                let idat = png_idat(&ctx, &samples, w, h, kind, best, 0).unwrap();
                assert_eq!(unfilter(&inflate(&idat), w * bytes, bpp), samples, "{kind:?}");
                for limit in [idat.len(), idat.len() + 1] {
                    assert_eq!(png_idat(&ctx, &samples, w, h, kind, best, limit).unwrap(), idat);
                }
                assert!(matches!(png_idat(&ctx, &samples, w, h, kind, best, idat.len() - 1), Err(EncodeError::Larger)));
            }
        }
        let canceled = Ctx::background();
        canceled.cancel();
        assert!(matches!(png_idat(&canceled, &[0; 64], 8, 8, PngKind::Gray8, false, 1), Err(EncodeError::Canceled)));
    }

    #[test]
    fn bilevel_rows_pack_bits() {
        let ctx = Ctx::background();
        let samples: Vec<u8> = (0..20 * 3).map(|i| if i % 3 == 0 { 255 } else { 0 }).collect();
        let idat = inflate(&png_idat(&ctx, &samples, 20, 3, PngKind::Bilevel, false, 0).unwrap());
        assert_eq!(idat.len(), 3 * (1 + 3));
        assert_eq!(idat[0], 0);
        assert_eq!(idat[1], 0b1001_0010);
    }

    #[test]
    fn deflate_bounds() {
        let ctx = Ctx::background();
        let data: Vec<u8> = (0..256 * 128).map(|i| [0u8, 19, 84, 255][(i * 7 / 13) % 4]).collect();
        for best in [false, true] {
            let want = deflate_samples(&ctx, &data, best, 0).unwrap();
            assert!(matches!(deflate_samples(&ctx, &data, best, want.len() - 1), Err(EncodeError::Larger)));
            assert_eq!(deflate_samples(&ctx, &data, best, want.len()).unwrap(), want);
            assert_eq!(inflate(&want), data);
        }
        let canceled = Ctx::background();
        canceled.cancel();
        assert!(deflate_samples(&canceled, b"sample", true, 0).is_err());
    }

    #[test]
    fn bounded_sample_decoding() {
        let ctx = Ctx::background();
        let data: Vec<u8> = (0..64 * 64).map(|i| [0u8, 18, 200, 255][(i / 4 + i / 256) % 4]).collect();
        let flate = |d: &[u8]| deflate_samples(&ctx, d, false, 0).unwrap();
        let plain = [Filter { name: b"FlateDecode".to_vec(), parms: None }];
        let good = flate(&data);
        assert_eq!(image_samples(&ctx, &good, &plain, data.len(), 64).unwrap(), data);
        let short = image_samples(&ctx, &flate(&data[..data.len() - 1]), &plain, data.len(), 64).unwrap();
        assert_eq!(short.len(), data.len() - 1);
        let mut long = data.clone();
        long.push(0);
        assert!(image_samples(&ctx, &flate(&long), &plain, data.len(), 64).is_err());
        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(image_samples(&ctx, &bad, &plain, data.len(), 64).is_err());
        assert_eq!(image_samples(&ctx, &good[..good.len() - 2], &plain, data.len(), 64).unwrap(), data);
        let mut parms = lopdf::Dictionary::new();
        parms.set("Predictor", 15);
        parms.set("Colors", 1);
        parms.set("Columns", 64);
        parms.set("BitsPerComponent", 8);
        let predicted = [Filter { name: b"FlateDecode".to_vec(), parms: Some(parms) }];
        let idat = png_idat(&ctx, &data, 64, 64, PngKind::Gray8, false, 0).unwrap();
        assert_eq!(image_samples(&ctx, &idat, &predicted, data.len(), 64).unwrap(), data);
        let canceled = Ctx::background();
        canceled.cancel();
        assert_eq!(image_samples(&canceled, &idat, &predicted, data.len(), 64), Err(DecodeError::Canceled));
    }
}
