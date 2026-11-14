use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::bail;
use crate::error::{Ctx, Error, Result, path_error};
use crate::files::{abs_path, go_ext, write_new};
use crate::process::{Out, write_out};
use crate::profiles::{MAX_PROFILE_BYTES, Profile, ProfileBundle, compression_flags, load_profiles, select_profile};

pub const USAGE: &str = "Usage: pdf-squeezer [flags] input.pdf [more.pdf | directory ...]

Compress PDFs without overwriting inputs or existing outputs. Directory inputs
include PDFs only; --recursive includes subdirectories. Batch outputs retain
relative directories. Advanced operations require optional tools; see README.

General:
  --engine NAME           native (default) or ghostscript
  -o, --output PATH        output file/directory; repeat once per input for separate files
  --compression LEVEL     light, balanced, medium, strong, heavy; explicit flags override
  --recursive             recurse into directory inputs; skip *.squeezed.pdf
  --jobs N                process N files at once (default 1); each has its own image budget
  --collision POLICY      error (default) or number
  --profile FILE          load JSON or .pdfscp profiles; repeat to combine bundles
  --profile-entry NAME|N  select a profile by name or 1-based index (default first)
  --save-profile FILE     save resolved settings; retain other entries in a JSON bundle
  --timestamps POLICY     preserve (default), modified, or now (both document dates)
  -V, --verbose           report stages, image progress, and preserved images
  -v, --version           print version
  -h, --help              print help

Images:
  --images                re-encode supported images; may use lossy JPEG
  --lossless              only lossless image transformations; implies --images
  --image-quality N       JPEG quality, 1..100 (default 75)
  --image-codecs LIST     flate,jpeg (default), flate, or jpeg
  --mono-codecs LIST      flate (default), ccitt, jbig2, or a comma-separated list
  --reduce-bit-depth      allow 16-bit to 8-bit conversion; never for soft masks
  --color-reduction MODE  exact (default) or preserve; independent of JPEG/bit depth
  --force-recompression   keep requested re-encoding even without size savings
  --image-memory N        native image working-buffer budget in MiB (default 512)
  --dpi N                 color/gray target DPI; 0 keeps resolution
  --gray-dpi N            override the gray target DPI
  --mono-dpi N            monochrome target DPI; 0 keeps resolution
  --dpi-threshold N       downsample above this multiple of target DPI (default 1.5)
  --clip-images           crop unused image margins; conservative shared-use analysis
  --inline-images         externalize inline images before optimization
  --extended-images       also decode JPEG 2000 and re-encode CMYK images
  --gray                  convert all page colors to grayscale
  --quality PRESET        Ghostscript: screen, ebook (default), printer, prepress

Document:
  --privacy               remove document info, XMP, piece info, web capture data
  --metadata KEY=VALUE    set Title, Author, Subject, Keywords, Creator, Producer,
                          CreationDate or ModDate; repeatable; empty removes a key
  --strip LIST            thumbnails,alternates,threads,tags,output-intents,piece-info,
                          metadata,links,annotations,forms,images,actions,multimedia,
                          hidden,xfa,attachments,web-capture; opt-in removals
  --flatten LIST          forms,annotations,links or all; freezes appearances
  --subset-fonts          subset eligible embedded fonts
  --merge-fonts           merge compatible embedded TrueType subsets
  --convert-fonts-cff     rewrite through Ghostscript with compact font programs
  --remove-standard-fonts remove embedded copies of the standard PDF fonts

Security and conversion:
  --password-file FILE    input password, first line; never passed in command arguments
  --decrypt               explicitly remove input encryption
  --encrypt-user-file FILE  output user password, first line
  --encrypt-owner-file FILE output owner password; AES-256; required with user password
  --permissions POLICY    output encryption permissions: all (default), print, none
  --extract MODE          images or text; output is a new directory or text file
  --bitmap                render pages to an image-only PDF; loses text/interactive data
  --mrc                   image-only layered PDF for scans; preserves fine dark text
  --render-dpi N          bitmap/MRC rendering resolution (default 200)
  --background-dpi N      MRC background resolution (default 72)
  --pdfa LEVEL            convert to PDF/A 2b, 3b, or 4 (4f with attachments); veraPDF
                          must confirm. Inputs that declare PDF/A keep their level.
  --output-intent FILE    ICC profile for the PDF/A output intent (default: bundled sRGB)
  --font-file NAME=PATH   font program for a non-embedded font; repeatable
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    Float,
    Str,
    List,
}

const FLAGS: &[(&str, Kind)] = &[
    ("background-dpi", Kind::Int),
    ("bitmap", Kind::Bool),
    ("clip-images", Kind::Bool),
    ("collision", Kind::Str),
    ("color-reduction", Kind::Str),
    ("compression", Kind::Str),
    ("convert-fonts-cff", Kind::Bool),
    ("decrypt", Kind::Bool),
    ("dpi", Kind::Int),
    ("dpi-threshold", Kind::Float),
    ("encrypt-owner-file", Kind::Str),
    ("encrypt-user-file", Kind::Str),
    ("engine", Kind::Str),
    ("extended-images", Kind::Bool),
    ("extract", Kind::Str),
    ("flatten", Kind::Str),
    ("font-file", Kind::List),
    ("force-recompression", Kind::Bool),
    ("gray", Kind::Bool),
    ("gray-dpi", Kind::Int),
    ("image-codecs", Kind::Str),
    ("image-memory", Kind::Int),
    ("image-quality", Kind::Int),
    ("images", Kind::Bool),
    ("inline-images", Kind::Bool),
    ("jobs", Kind::Int),
    ("lossless", Kind::Bool),
    ("merge-fonts", Kind::Bool),
    ("metadata", Kind::List),
    ("mono-codecs", Kind::Str),
    ("mono-dpi", Kind::Int),
    ("mrc", Kind::Bool),
    ("o", Kind::List),
    ("output", Kind::List),
    ("output-intent", Kind::Str),
    ("password-file", Kind::Str),
    ("pdfa", Kind::Str),
    ("permissions", Kind::Str),
    ("privacy", Kind::Bool),
    ("profile", Kind::List),
    ("profile-entry", Kind::Str),
    ("quality", Kind::Str),
    ("recursive", Kind::Bool),
    ("reduce-bit-depth", Kind::Bool),
    ("remove-standard-fonts", Kind::Bool),
    ("render-dpi", Kind::Int),
    ("save-profile", Kind::Str),
    ("strip", Kind::Str),
    ("subset-fonts", Kind::Bool),
    ("timestamps", Kind::Str),
    ("v", Kind::Bool),
    ("V", Kind::Bool),
    ("verbose", Kind::Bool),
    ("version", Kind::Bool),
];

fn flag_kind(name: &str) -> Option<Kind> {
    FLAGS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

/// Settings a profile may carry: no paths, secrets, or single-run options.
pub fn profile_flag(name: &str) -> bool {
    !matches!(
        name,
        "o" | "output"
            | "profile"
            | "profile-entry"
            | "save-profile"
            | "password-file"
            | "encrypt-user-file"
            | "encrypt-owner-file"
            | "metadata"
            | "v"
            | "version"
            | "V"
            | "verbose"
            | "recursive"
            | "jobs"
            | "collision"
            | "extract"
            | "output-intent"
            | "font-file"
    )
}

#[derive(Clone, Debug)]
pub struct Options {
    pub engine: String,
    pub quality: String,
    pub output: OsString,
    pub collision: String,
    pub timestamps: String,
    pub compression: String,
    pub color_reduction: String,
    pub outputs: Vec<OsString>,
    pub dpi: i64,
    pub gray_dpi: i64,
    pub mono_dpi: i64,
    pub image_quality: i64,
    pub image_memory: i64,
    pub jobs: i64,
    pub dpi_threshold: f64,
    pub gray: bool,
    pub images: bool,
    pub privacy: bool,
    pub verbose: bool,
    pub recursive: bool,
    pub lossless: bool,
    pub reduce_bits: bool,
    pub force: bool,
    pub clip: bool,
    pub inline: bool,
    pub extended: bool,
    pub image_codecs: String,
    pub mono_codecs: String,
    pub strip: String,
    pub flatten: String,
    pub subset_fonts: bool,
    pub merge_fonts: bool,
    pub cff_fonts: bool,
    pub remove_standard_fonts: bool,
    pub password_file: OsString,
    pub encrypt_user_file: OsString,
    pub encrypt_owner_file: OsString,
    pub permissions: String,
    /// The input password as text for the PDF reader and the Python tools. Bytes that
    /// are not UTF-8 read as PDFDocEncoding, the encoding of older passwords.
    pub password: String,
    /// The input password exactly as the file holds it, for qpdf.
    pub password_bytes: Vec<u8>,
    pub encrypt_user: String,
    pub encrypt_owner: String,
    pub decrypt: bool,
    pub bitmap: bool,
    pub mrc: bool,
    pub extract: String,
    pub output_intent: OsString,
    pub pdfa: String,
    pub render_dpi: i64,
    pub background_dpi: i64,
    pub metadata: Vec<String>,
    pub font_files: Vec<OsString>,
    /// Set per input that declares PDF/A-1, a PDF 1.4 profile without object streams.
    pub classic_xref: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            engine: "native".into(),
            quality: String::new(),
            output: OsString::new(),
            collision: "error".into(),
            timestamps: "preserve".into(),
            compression: String::new(),
            color_reduction: "exact".into(),
            outputs: Vec::new(),
            dpi: 0,
            gray_dpi: 0,
            mono_dpi: 0,
            image_quality: 75,
            image_memory: 512,
            jobs: 1,
            dpi_threshold: 1.5,
            gray: false,
            images: false,
            privacy: false,
            verbose: false,
            recursive: false,
            lossless: false,
            reduce_bits: false,
            force: false,
            clip: false,
            inline: false,
            extended: false,
            image_codecs: "flate,jpeg".into(),
            mono_codecs: "flate".into(),
            strip: String::new(),
            flatten: String::new(),
            subset_fonts: false,
            merge_fonts: false,
            cff_fonts: false,
            remove_standard_fonts: false,
            password_file: OsString::new(),
            encrypt_user_file: OsString::new(),
            encrypt_owner_file: OsString::new(),
            permissions: "all".into(),
            password: String::new(),
            password_bytes: Vec::new(),
            encrypt_user: String::new(),
            encrypt_owner: String::new(),
            decrypt: false,
            bitmap: false,
            mrc: false,
            extract: String::new(),
            output_intent: OsString::new(),
            pdfa: String::new(),
            render_dpi: 200,
            background_dpi: 72,
            metadata: Vec::new(),
            font_files: Vec::new(),
            classic_xref: false,
        }
    }
}

/// Flag values that are not settings of the document run.
#[derive(Default)]
struct Extra {
    profiles: Vec<OsString>,
    profile_entry: String,
    save_profile: OsString,
    version: bool,
}

fn lossy(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn parse_bool(value: &str) -> std::result::Result<bool, &'static str> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err("parse error"),
    }
}

/// Parses an integer with Go literal syntax: sign, 0x/0o/0b prefixes, a leading 0
/// for octal, and underscores between digits.
fn parse_int(value: &str) -> std::result::Result<i64, &'static str> {
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'+') => (false, &value[1..]),
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    let lower = digits.to_ascii_lowercase();
    let (radix, body, prefixed) = if let Some(rest) = lower.strip_prefix("0x") {
        (16, rest.to_string(), true)
    } else if let Some(rest) = lower.strip_prefix("0o") {
        (8, rest.to_string(), true)
    } else if let Some(rest) = lower.strip_prefix("0b") {
        (2, rest.to_string(), true)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, lower[1..].to_string(), true)
    } else {
        (10, lower.clone(), false)
    };
    if body.contains('_') {
        // Underscores separate digits; a base prefix counts as a digit.
        let check = if prefixed { format!("0{body}") } else { body.clone() };
        if check.starts_with('_') || check.ends_with('_') || check.contains("__") {
            return Err("parse error");
        }
    }
    let body: String = body.chars().filter(|&c| c != '_').collect();
    if body.is_empty() {
        return if radix == 8 && !lower.starts_with("0o") && lower == "0" { Ok(0) } else { Err("parse error") };
    }
    if !body.chars().all(|c| c.is_digit(radix)) {
        return Err("parse error");
    }
    let magnitude = u64::from_str_radix(&body, radix).map_err(|_| "value out of range")?;
    if negative {
        if magnitude > i64::MAX as u64 + 1 {
            return Err("value out of range");
        }
        Ok((magnitude as i64).wrapping_neg())
    } else {
        i64::try_from(magnitude).map_err(|_| "value out of range")
    }
}

fn parse_float(value: &str) -> std::result::Result<f64, &'static str> {
    let lower = value.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return value.parse::<f64>().map_err(|_| "parse error");
    }
    if lower.is_empty() || !lower.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'+' | b'-')) {
        return Err("parse error");
    }
    let parsed = value.parse::<f64>().map_err(|_| "parse error")?;
    if parsed.is_infinite() { Err("value out of range") } else { Ok(parsed) }
}

/// Formats a float like Go's strconv 'g' with the shortest exact digits.
fn format_float(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let exponent = value.abs().log10().floor() as i32;
    let digits = format!("{:e}", value);
    let mantissa_digits = digits.split('e').next().unwrap_or("").replace(['-', '.'], "").len() as i32;
    if exponent < -4 || exponent >= mantissa_digits.max(21) {
        let (mantissa, exp) = digits.split_once('e').unwrap_or((&digits, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        format!("{value}")
    }
}

impl Options {
    fn set(&mut self, extra: &mut Extra, name: &str, value: &OsStr) -> std::result::Result<(), &'static str> {
        let text = || lossy(value);
        match name {
            "engine" => self.engine = text(),
            "quality" => self.quality = text(),
            "o" | "output" => self.outputs.push(value.to_os_string()),
            "compression" => self.compression = text(),
            "color-reduction" => self.color_reduction = text(),
            "collision" => self.collision = text(),
            "timestamps" => self.timestamps = text(),
            "recursive" => self.recursive = parse_bool(&text())?,
            "jobs" => self.jobs = parse_int(&text())?,
            "verbose" | "V" => self.verbose = parse_bool(&text())?,
            "images" => self.images = parse_bool(&text())?,
            "lossless" => self.lossless = parse_bool(&text())?,
            "reduce-bit-depth" => self.reduce_bits = parse_bool(&text())?,
            "force-recompression" => self.force = parse_bool(&text())?,
            "gray" => self.gray = parse_bool(&text())?,
            "clip-images" => self.clip = parse_bool(&text())?,
            "inline-images" => self.inline = parse_bool(&text())?,
            "extended-images" => self.extended = parse_bool(&text())?,
            "image-quality" => self.image_quality = parse_int(&text())?,
            "image-memory" => self.image_memory = parse_int(&text())?,
            "image-codecs" => self.image_codecs = text(),
            "mono-codecs" => self.mono_codecs = text(),
            "dpi" => self.dpi = parse_int(&text())?,
            "gray-dpi" => self.gray_dpi = parse_int(&text())?,
            "mono-dpi" => self.mono_dpi = parse_int(&text())?,
            "dpi-threshold" => self.dpi_threshold = parse_float(&text())?,
            "privacy" => self.privacy = parse_bool(&text())?,
            "metadata" => self.metadata.push(text()),
            "strip" => self.strip = text(),
            "flatten" => self.flatten = text(),
            "subset-fonts" => self.subset_fonts = parse_bool(&text())?,
            "merge-fonts" => self.merge_fonts = parse_bool(&text())?,
            "convert-fonts-cff" => self.cff_fonts = parse_bool(&text())?,
            "remove-standard-fonts" => self.remove_standard_fonts = parse_bool(&text())?,
            "password-file" => self.password_file = value.to_os_string(),
            "decrypt" => self.decrypt = parse_bool(&text())?,
            "encrypt-user-file" => self.encrypt_user_file = value.to_os_string(),
            "encrypt-owner-file" => self.encrypt_owner_file = value.to_os_string(),
            "permissions" => self.permissions = text(),
            "extract" => self.extract = text(),
            "bitmap" => self.bitmap = parse_bool(&text())?,
            "mrc" => self.mrc = parse_bool(&text())?,
            "render-dpi" => self.render_dpi = parse_int(&text())?,
            "background-dpi" => self.background_dpi = parse_int(&text())?,
            "pdfa" => self.pdfa = text(),
            "output-intent" => self.output_intent = value.to_os_string(),
            "font-file" => self.font_files.push(value.to_os_string()),
            "profile" => extra.profiles.push(value.to_os_string()),
            "profile-entry" => extra.profile_entry = text(),
            "save-profile" => extra.save_profile = value.to_os_string(),
            "version" | "v" => extra.version = parse_bool(&text())?,
            _ => return Err("flag provided but not defined"),
        }
        Ok(())
    }

    /// The value a saved profile records for a setting.
    fn flag_value(&self, name: &str) -> String {
        let flag = |b: bool| b.to_string();
        match name {
            "engine" => self.engine.clone(),
            "quality" => self.quality.clone(),
            "compression" => self.compression.clone(),
            "color-reduction" => self.color_reduction.clone(),
            "timestamps" => self.timestamps.clone(),
            "images" => flag(self.images),
            "lossless" => flag(self.lossless),
            "reduce-bit-depth" => flag(self.reduce_bits),
            "force-recompression" => flag(self.force),
            "gray" => flag(self.gray),
            "clip-images" => flag(self.clip),
            "inline-images" => flag(self.inline),
            "extended-images" => flag(self.extended),
            "image-quality" => self.image_quality.to_string(),
            "image-memory" => self.image_memory.to_string(),
            "image-codecs" => self.image_codecs.clone(),
            "mono-codecs" => self.mono_codecs.clone(),
            "dpi" => self.dpi.to_string(),
            "gray-dpi" => self.gray_dpi.to_string(),
            "mono-dpi" => self.mono_dpi.to_string(),
            "dpi-threshold" => format_float(self.dpi_threshold),
            "privacy" => flag(self.privacy),
            "strip" => self.strip.clone(),
            "flatten" => self.flatten.clone(),
            "subset-fonts" => flag(self.subset_fonts),
            "merge-fonts" => flag(self.merge_fonts),
            "convert-fonts-cff" => flag(self.cff_fonts),
            "remove-standard-fonts" => flag(self.remove_standard_fonts),
            "decrypt" => flag(self.decrypt),
            "permissions" => self.permissions.clone(),
            "bitmap" => flag(self.bitmap),
            "mrc" => flag(self.mrc),
            "render-dpi" => self.render_dpi.to_string(),
            "background-dpi" => self.background_dpi.to_string(),
            "pdfa" => self.pdfa.clone(),
            _ => String::new(),
        }
    }
}

/// Go flag-package semantics: one or two dashes, `-name=value`, a separate value
/// for non-boolean flags, and `--` to end the flags.
struct Parser<'a> {
    opts: Options,
    extra: Extra,
    visited: HashSet<String>,
    stderr: &'a Out,
}

impl Parser<'_> {
    fn fail(&self, message: String) -> Error {
        write_out(self.stderr, format!("{message}\n").as_bytes());
        write_out(self.stderr, USAGE.as_bytes());
        Error::new(message)
    }

    /// Parses flags until the first non-flag argument; returns how many were consumed.
    fn parse(&mut self, args: &[OsString]) -> Result<usize> {
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_bytes();
            if arg.len() < 2 || arg[0] != b'-' {
                return Ok(i);
            }
            let mut minuses = 1;
            if arg[1] == b'-' {
                minuses = 2;
                if arg.len() == 2 {
                    return Ok(i + 1);
                }
            }
            let mut name = &arg[minuses..];
            if name.is_empty() || name[0] == b'-' || name[0] == b'=' {
                return Err(self.fail(format!("bad flag syntax: {}", String::from_utf8_lossy(arg))));
            }
            i += 1;
            let mut value: Option<&[u8]> = None;
            if let Some(at) = name[1..].iter().position(|&b| b == b'=') {
                value = Some(&name[at + 2..]);
                name = &name[..at + 1];
            }
            let name = String::from_utf8_lossy(name).into_owned();
            let Some(kind) = flag_kind(&name) else {
                if name == "help" || name == "h" {
                    write_out(self.stderr, USAGE.as_bytes());
                    return Err(Error::help());
                }
                return Err(self.fail(format!("flag provided but not defined: -{name}")));
            };
            if kind == Kind::Bool {
                let raw = value.unwrap_or(b"true");
                if self.opts.set(&mut self.extra, &name, OsStr::from_bytes(raw)).is_err() {
                    let shown = String::from_utf8_lossy(raw);
                    return Err(self.fail(format!("invalid boolean value {shown:?} for -{name}: parse error")));
                }
            } else {
                let raw = match value {
                    Some(v) => v.to_vec(),
                    None if i < args.len() => {
                        i += 1;
                        args[i - 1].as_bytes().to_vec()
                    }
                    None => return Err(self.fail(format!("flag needs an argument: -{name}"))),
                };
                if let Err(problem) = self.opts.set(&mut self.extra, &name, OsStr::from_bytes(&raw)) {
                    let shown = String::from_utf8_lossy(&raw);
                    return Err(self.fail(format!("invalid value {shown:?} for flag -{name}: {problem}")));
                }
            }
            self.visited.insert(name);
        }
        Ok(i)
    }

    /// Flags may follow file names; everything after `--` is a file name.
    fn parse_all(&mut self, mut args: &[OsString]) -> Result<Vec<OsString>> {
        let mut inputs = Vec::new();
        loop {
            let consumed = self.parse(args)?;
            let rest = &args[consumed..];
            if rest.is_empty() || consumed > 0 && args[consumed - 1].as_bytes() == b"--" {
                inputs.extend_from_slice(rest);
                return Ok(inputs);
            }
            inputs.push(rest[0].clone());
            args = &rest[1..];
        }
    }

    /// Applies a value as if typed on the command line, for profiles and tiers.
    fn set(&mut self, name: &str, value: &str) -> std::result::Result<(), &'static str> {
        self.opts.set(&mut self.extra, name, OsStr::new(value))
    }
}

pub struct Parsed {
    pub opts: Options,
    pub inputs: Vec<OsString>,
    pub version: bool,
}

pub fn parse_options(ctx: &Ctx, args: &[OsString], stderr: &Out) -> Result<Parsed> {
    let mut p = Parser { opts: Options::default(), extra: Extra::default(), visited: HashSet::new(), stderr };
    let inputs = p.parse_all(args)?;
    if p.extra.version {
        return Ok(Parsed { opts: p.opts, inputs, version: true });
    }
    let visited = p.visited.clone();
    if p.opts.outputs.len() == 1 {
        p.opts.output = p.opts.outputs[0].clone();
    }
    let mut profiles: Vec<Profile> = Vec::new();
    for path in p.extra.profiles.clone() {
        profiles.extend(load_profiles(ctx, Path::new(&path), stderr)?);
    }
    let selected = select_profile(&profiles, &p.extra.profile_entry)?;
    for profile in &profiles {
        for name in profile.flags.keys() {
            if !profile_flag(name) || flag_kind(name).is_none() {
                bail!("profile flag {name:?} is not permitted");
            }
        }
    }
    let mut configured = HashSet::new();
    if let Some(profile) = profiles.get(selected).cloned() {
        for (name, value) in &profile.flags {
            configured.insert(name.clone());
            if !visited.contains(name) {
                p.set(name, value).map_err(|e| Error::new(format!("profile flag {name}: {e}")))?;
            }
        }
        // Explicit metadata wins per key, applied after the profile's metadata.
        let explicit = std::mem::take(&mut p.opts.metadata);
        p.opts.metadata = profile.metadata.clone();
        p.opts.metadata.extend(explicit);
    }
    let images_off = (visited.contains("images") || configured.contains("images")) && !p.opts.images;
    if !p.opts.compression.is_empty() {
        let defaults = compression_flags(&p.opts.compression)?;
        for (name, value) in defaults {
            if images_off && name != "merge-fonts" {
                continue;
            }
            if !visited.contains(name) && !configured.contains(name) {
                p.set(name, &value).map_err(Error::new)?;
            }
        }
    }
    if p.opts.lossless && !visited.contains("image-codecs") && p.opts.image_codecs == "flate,jpeg" {
        p.opts.image_codecs = "flate".into();
    }
    p.opts.validate()?;
    if images_off && p.opts.images {
        bail!("--images=false conflicts with an enabled image transformation");
    }
    let save = p.extra.save_profile.clone();
    if !save.is_empty() {
        let save_path = Path::new(&save);
        if go_ext(save_path.as_os_str()).eq_ignore_ascii_case(b".pdfscp") {
            bail!("--save-profile writes native JSON; use a .json filename, not .pdfscp");
        }
        let mut profile = Profile { version: 1, name: String::new(), flags: BTreeMap::new(), metadata: p.opts.metadata.clone() };
        for (name, _) in FLAGS {
            if profile_flag(name) {
                profile.flags.insert((*name).to_string(), p.opts.flag_value(name));
            }
        }
        profile.name = match profiles.get(selected) {
            Some(existing) => existing.name.clone(),
            None => p.opts.compression.clone(),
        };
        let data = if profiles.len() > 1 {
            profiles[selected] = profile;
            serde_json::to_vec_pretty(&ProfileBundle { version: 2, profiles: profiles.clone() })
        } else {
            serde_json::to_vec_pretty(&profile)
        }
        .map_err(|e| Error::new(e.to_string()))?;
        if data.len() + 1 > MAX_PROFILE_BYTES {
            bail!("saved profile exceeds 1 MiB; export fewer entries or less metadata");
        }
        let mut bytes = data;
        bytes.push(b'\n');
        write_new(ctx, save_path, &mut bytes.as_slice())?;
    }
    if inputs.is_empty() && save.is_empty() {
        bail!("provide at least one input PDF or directory");
    }
    let opts = &mut p.opts;
    if !opts.password_file.is_empty() {
        let bytes = read_password(Path::new(&opts.password_file))?;
        opts.password = String::from_utf8(bytes.clone()).unwrap_or_else(|_| legacy_password(&bytes));
        opts.password_bytes = bytes;
    }
    // AES-256 passwords are UTF-8 text by definition.
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).map_err(|_| Error::new("output password files must contain UTF-8 text"));
    if !opts.encrypt_user_file.is_empty() {
        opts.encrypt_user = text(read_password(Path::new(&opts.encrypt_user_file))?)?;
    }
    if !opts.encrypt_owner_file.is_empty() {
        opts.encrypt_owner = text(read_password(Path::new(&opts.encrypt_owner_file))?)?;
    }
    if !opts.encrypt_owner_file.is_empty() && (opts.encrypt_owner.is_empty() || opts.encrypt_owner == opts.encrypt_user) {
        bail!("output owner password must be nonempty and different from the user password");
    }
    Ok(Parsed { opts: p.opts, inputs, version: false })
}

/// Older encryption takes passwords in PDFDocEncoding, and the PDF reader encodes text
/// passwords that way, so decoding them from it keeps every defined byte.
fn legacy_password(bytes: &[u8]) -> String {
    let bom = bytes.starts_with(b"\xFE\xFF") || bytes.starts_with(b"\xEF\xBB\xBF");
    let text = (!bom).then(|| lopdf::decode_text_string(&lopdf::Object::String(bytes.to_vec(), lopdf::StringFormat::Literal)).ok()).flatten();
    text.unwrap_or_else(|| bytes.iter().map(|&b| char::from(b)).collect())
}

pub fn read_password(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).map_err(|e| path_error("open", path, &e).context("password file"))?;
    let mut data = Vec::new();
    file.take(4097).read_to_end(&mut data)?;
    if data.len() > 4096 {
        bail!("password file exceeds 4096 bytes");
    }
    let mut data = data.as_slice();
    data = data.strip_suffix(b"\n").unwrap_or(data);
    data = data.strip_suffix(b"\r").unwrap_or(data);
    if data.iter().any(|&b| matches!(b, b'\r' | b'\n' | 0)) {
        bail!("password file must contain one line without NUL");
    }
    Ok(data.to_vec())
}

pub fn list_contains(list: &str, item: &str) -> bool {
    list.split(',').any(|s| s == item)
}

fn valid_list(value: &str, allowed: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let mut seen = HashSet::new();
    value.split(',').all(|s| !s.is_empty() && list_contains(allowed, s) && seen.insert(s))
}

fn valid_date(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 17 || !value.starts_with("D:") || b[16] != b'Z' || !b[2..16].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let num = |from: usize, to: usize| value[from..to].parse::<u32>().unwrap_or(u32::MAX);
    let (year, month, day) = (num(2, 6), num(6, 8), num(8, 10));
    let (hour, minute, second) = (num(10, 12), num(12, 14), num(14, 16));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

impl Options {
    pub fn validate(&mut self) -> Result<()> {
        // Profiles saved by the Go release name the native engine after its PDF library.
        if self.engine == "pdfcpu" {
            self.engine = "native".into();
        }
        if self.engine != "native" && self.engine != "ghostscript" {
            bail!("--engine must be native or ghostscript");
        }
        if !self.quality.is_empty() && (self.engine != "ghostscript" || !list_contains("screen,ebook,printer,prepress", &self.quality)) {
            bail!("--quality requires Ghostscript and screen, ebook, printer, or prepress");
        }
        if self.engine == "ghostscript" && self.quality.is_empty() {
            self.quality = "ebook".into();
        }
        for dpi in [self.dpi, self.gray_dpi, self.mono_dpi] {
            if !(0..=9600).contains(&dpi) {
                bail!("image DPI must be between 0 and 9600");
            }
        }
        if self.dpi_threshold.is_nan() || self.dpi_threshold <= 1.0 || self.dpi_threshold > 10.0 {
            bail!("--dpi-threshold must be greater than 1 and at most 10");
        }
        if !(1..=100).contains(&self.image_quality) {
            bail!("--image-quality must be between 1 and 100");
        }
        if !(1..=64).contains(&self.jobs) {
            bail!("--jobs must be between 1 and 64");
        }
        if !(16..=8192).contains(&self.image_memory) {
            bail!("--image-memory must be between 16 and 8192 MiB");
        }
        if self.image_codecs.is_empty()
            || !valid_list(&self.image_codecs, "flate,jpeg")
            || self.mono_codecs.is_empty()
            || !valid_list(&self.mono_codecs, "flate,ccitt,jbig2")
        {
            bail!("invalid image or monochrome codec list");
        }
        if !valid_list(
            &self.strip,
            "thumbnails,alternates,threads,tags,output-intents,piece-info,metadata,links,annotations,forms,images,actions,multimedia,hidden,xfa,attachments,web-capture",
        ) {
            bail!("invalid --strip list");
        }
        if !valid_list(&self.flatten, "forms,annotations,links,all") {
            bail!("invalid --flatten list");
        }
        if list_contains(&self.flatten, "all") && self.flatten != "all" {
            bail!("--flatten all cannot be combined with other categories");
        }
        for kind in ["forms", "annotations", "links"] {
            if list_contains(&self.strip, kind) && (list_contains(&self.flatten, kind) || self.flatten == "all") {
                bail!("cannot both strip and flatten {kind}");
            }
        }
        if self.collision != "error" && self.collision != "number" {
            bail!("--collision must be error or number");
        }
        if !list_contains("preserve,modified,now", &self.timestamps) {
            bail!("--timestamps must be preserve, modified, or now");
        }
        if !list_contains("exact,preserve", &self.color_reduction) {
            bail!("--color-reduction must be exact or preserve");
        }
        if self.outputs.iter().any(|o| o.is_empty()) {
            bail!("--output must not be empty");
        }
        for setting in &self.metadata {
            let Some((key, value)) = setting.split_once('=') else {
                bail!("--metadata expects a supported KEY=VALUE");
            };
            if !list_contains("Title,Author,Subject,Keywords,Creator,Producer,CreationDate,ModDate", key) {
                bail!("--metadata expects a supported KEY=VALUE");
            }
            if value.len() > 65536 || value.contains('\0') {
                bail!("invalid metadata value");
            }
            if (key == "CreationDate" || key == "ModDate") && !value.is_empty() && !valid_date(value) {
                bail!("metadata dates must be valid UTC dates in D:YYYYMMDDhhmmssZ form");
            }
        }
        if self.privacy && !self.metadata.is_empty() {
            bail!("--privacy cannot be combined with --metadata");
        }
        if !self.encrypt_user_file.is_empty() && self.encrypt_owner_file.is_empty() {
            bail!("--encrypt-user-file requires --encrypt-owner-file");
        }
        if !list_contains("all,print,none", &self.permissions) {
            bail!("invalid --permissions policy");
        }
        if self.permissions != "all" && self.encrypt_owner_file.is_empty() {
            bail!("--permissions requires output encryption");
        }
        if self.decrypt && !self.encrypt_owner_file.is_empty() {
            bail!("cannot combine --decrypt with output encryption");
        }
        if !self.extract.is_empty() && !list_contains("images,text", &self.extract) {
            bail!("--extract must be images or text");
        }
        if self.bitmap && self.mrc {
            bail!("choose --bitmap or --mrc");
        }
        if self.render_dpi < 36 || self.render_dpi > 1200 || self.background_dpi < 10 || self.background_dpi > self.render_dpi {
            bail!("render DPI must be 36..1200; background DPI must be 10..render DPI");
        }
        self.pdfa = self.pdfa.to_lowercase();
        if !self.pdfa.is_empty() && !list_contains("2b,3b,4", &self.pdfa) {
            bail!("--pdfa must be 2b, 3b, or 4");
        }
        let conflict = self.breaks_pdfa(true);
        if !self.pdfa.is_empty() && !conflict.is_empty() {
            bail!("PDF/A requires metadata, color information, embedded fonts, and no encryption; {conflict} conflicts with --pdfa");
        }
        if self.pdfa.is_empty() && (!self.output_intent.is_empty() || !self.font_files.is_empty()) {
            bail!("--output-intent and --font-file require --pdfa");
        }
        for setting in &self.font_files {
            let bytes = setting.as_bytes();
            let ok = bytes.iter().position(|&b| b == b'=').is_some_and(|at| at > 0 && at + 1 < bytes.len());
            if !ok {
                bail!("--font-file expects NAME=PATH");
            }
        }
        if self.lossless
            && (self.engine != "native"
                || self.reduce_bits
                || self.gray
                || self.dpi > 0
                || self.gray_dpi > 0
                || self.mono_dpi > 0
                || self.bitmap
                || self.mrc
                || self.cff_fonts
                || list_contains(&self.image_codecs, "jpeg"))
        {
            bail!("--lossless conflicts with lossy transforms or JPEG encoding");
        }
        if self.lossless
            || self.reduce_bits
            || self.force
            || self.clip
            || self.extended
            || self.mono_codecs != "flate"
            || self.inline && self.extract.is_empty()
        {
            self.images = true;
        }
        if self.inline && self.extract == "text" {
            bail!("--inline-images applies to compression or image extraction");
        }
        if self.engine == "ghostscript" && self.images {
            bail!("--images and its encoding options require --engine native");
        }
        if !self.extract.is_empty()
            && (self.images
                || self.engine != "native"
                || self.gray
                || self.dpi != 0
                || self.gray_dpi != 0
                || self.mono_dpi != 0
                || self.privacy
                || !self.metadata.is_empty()
                || !self.strip.is_empty()
                || !self.flatten.is_empty()
                || self.subset_fonts
                || self.merge_fonts
                || self.cff_fonts
                || self.remove_standard_fonts
                || self.bitmap
                || self.mrc
                || !self.pdfa.is_empty()
                || self.decrypt
                || !self.encrypt_owner_file.is_empty())
        {
            bail!("--extract cannot be combined with document transformations");
        }
        Ok(())
    }

    /// Keeps a size fallback from undoing requested document changes.
    pub fn required_output(&self) -> bool {
        self.privacy
            || !self.metadata.is_empty()
            || !self.strip.is_empty()
            || !self.flatten.is_empty()
            || self.gray
            || self.force
            || self.decrypt
            || !self.encrypt_owner_file.is_empty()
            || self.bitmap
            || self.mrc
            || !self.pdfa.is_empty()
            || self.timestamps == "now"
            || self.timestamps == "modified"
            || self.remove_standard_fonts
    }

    /// Names the first option that removes something PDF/A requires, or "".
    /// With conversion the converter rebuilds metadata and color afterwards, so only
    /// options that keep taking things away conflict; without it, any option that
    /// rewrites the document or its metadata ends the input's declared conformance.
    pub fn breaks_pdfa(&self, conversion: bool) -> String {
        if !self.encrypt_owner_file.is_empty() {
            "--encrypt-owner-file".to_string()
        } else if self.privacy {
            "--privacy".into()
        } else if list_contains(&self.strip, "metadata") {
            "--strip metadata".into()
        } else if list_contains(&self.strip, "output-intents") {
            "--strip output-intents".into()
        } else if self.remove_standard_fonts {
            "--remove-standard-fonts".into()
        } else if conversion {
            String::new()
        } else if !self.metadata.is_empty() {
            "--metadata".into()
        } else if self.timestamps == "now" || self.timestamps == "modified" {
            format!("--timestamps {}", self.timestamps)
        } else if self.bitmap {
            "--bitmap".into()
        } else if self.mrc {
            "--mrc".into()
        } else if self.engine == "ghostscript" || self.cff_fonts {
            "a Ghostscript rewrite".into()
        } else {
            String::new()
        }
    }

    /// Reports --strip categories that the Python tools implement.
    pub fn python_strip(&self) -> bool {
        ["actions", "multimedia", "hidden", "xfa", "attachments"].iter().any(|c| list_contains(&self.strip, c))
    }

    pub fn advanced_images(&self) -> bool {
        self.clip
            || self.extended
            || self.mono_codecs != "flate"
            || self.engine == "native" && (self.dpi > 0 || self.gray_dpi > 0 || self.mono_dpi > 0)
    }
}

#[derive(Clone, Debug)]
pub struct InputFile {
    pub path: PathBuf,
    pub relative: PathBuf,
}

/// Earlier outputs: NAME.squeezed.pdf and the numbered collisions NAME.squeezed.N.pdf.
pub fn generated_pdf(name: &OsStr) -> bool {
    let lower = name.as_bytes().to_ascii_lowercase();
    let name = lower.strip_suffix(b".pdf").unwrap_or(&lower);
    if name.ends_with(b".squeezed") {
        return true;
    }
    let marker = b".squeezed.";
    let Some(at) = name.windows(marker.len()).rposition(|w| w == marker) else {
        return false;
    };
    let digits = &name[at + marker.len()..];
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) && std::str::from_utf8(digits).ok().and_then(|d| d.parse::<u32>().ok()).is_some()
}

pub fn expand_inputs(ctx: &Ctx, inputs: &[OsString], recursive: bool) -> Result<Vec<InputFile>> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: &Path, relative: PathBuf| -> Result<()> {
        let abs = abs_path(path)?;
        let resolved = fs::canonicalize(&abs).map_err(|e| path_error("lstat", &abs, &e))?;
        if seen.insert(resolved) {
            files.push(InputFile { path: abs, relative });
        }
        Ok(())
    };
    for input in inputs {
        ctx.check()?;
        let input = Path::new(input);
        let info = fs::metadata(input).map_err(|e| path_error("stat", input, &e))?;
        if !info.is_dir() {
            if !info.is_file() {
                bail!("input must be a regular PDF file: {}", input.display());
            }
            let base = input.file_name().map(PathBuf::from).unwrap_or_else(|| input.to_path_buf());
            add(input, base)?;
            continue;
        }
        // The walk follows a symlinked root but reports paths under the named directory.
        let root = fs::canonicalize(input).map_err(|e| path_error("lstat", input, &e))?;
        walk(ctx, &root, &root, recursive, &mut |rel| add(&input.join(rel), rel.to_path_buf()))?;
    }
    Ok(files)
}

fn walk(ctx: &Ctx, root: &Path, dir: &Path, recursive: bool, add: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> =
        fs::read_dir(dir).map_err(|e| path_error("open", dir, &e))?.collect::<std::io::Result<_>>().map_err(|e| path_error("readdirent", dir, &e))?;
    entries.sort_by(|a, b| a.file_name().as_bytes().cmp(b.file_name().as_bytes()));
    for entry in entries {
        ctx.check()?;
        let name = entry.file_name();
        let kind = entry.file_type().map_err(|e| path_error("lstat", &entry.path(), &e))?;
        let hidden = name.as_bytes().starts_with(b".");
        if kind.is_dir() {
            if recursive && !hidden {
                walk(ctx, root, &entry.path(), recursive, add)?;
            }
            continue;
        }
        if !kind.is_file() || hidden || !go_ext(&name).eq_ignore_ascii_case(b".pdf") || generated_pdf(&name) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        add(&rel)?;
    }
    Ok(())
}

/// Converts lossy text back into the bytes of a path-like option.
pub fn os(value: &str) -> OsString {
    OsString::from_vec(value.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn parse(args: &[&str]) -> Result<Parsed> {
        let sink: Out = Arc::new(Mutex::new(std::io::sink()));
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse_options(&Ctx::background(), &args, &sink)
    }

    #[test]
    fn go_integer_and_float_syntax() {
        assert_eq!(parse_int("42"), Ok(42));
        assert_eq!(parse_int("0x10"), Ok(16));
        assert_eq!(parse_int("010"), Ok(8));
        assert_eq!(parse_int("1_000"), Ok(1000));
        assert_eq!(parse_int("-7"), Ok(-7));
        assert!(parse_int("1__0").is_err());
        assert!(parse_int("abc").is_err());
        assert!(parse_int("").is_err());
        assert!(parse_float("NaN").unwrap().is_nan());
        assert_eq!(parse_float("1.5"), Ok(1.5));
        assert!(parse_float("x").is_err());
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(2.0), "2");
    }

    #[test]
    fn rejects_conflicts() {
        for args in [
            &["--lossless", "--reduce-bit-depth"][..],
            &["--lossless", "--dpi", "72"],
            &["--lossless", "--image-codecs", "jpeg"],
            &["--image-memory", "0"],
            &["--image-quality", "101"],
            &["--dpi-threshold", "NaN"],
            &["--strip", "forms", "--flatten", "forms"],
            &["--privacy", "--metadata", "Title=test"],
            &["--bitmap", "--mrc"],
            &["--extract", "images", "--gray"],
            &["--encrypt-user-file", "example.txt"],
            &["--flatten", "all,links"],
            &["--pdfa", "4", "--privacy"],
            &["--pdfa", "4", "--strip", "output-intents"],
            &["--output-intent", "profile.icc"],
            &["--pdfa", "4", "--font-file", "Arial"],
            &["--strip", "javascript"],
            &["--extract", "text", "--pdfa", "4"],
            &["--pdfa", "1b"],
            &["--pdfa", "4f"],
            &["--images", "--engine", "ghostscript"],
            &["--dpi", "72"],
            &["--gray"],
            &["--engine", "ghostscript", "--dpi", "-1"],
        ] {
            let mut full = args.to_vec();
            full.push("input.pdf");
            if !matches!(args[0], "--dpi" | "--gray") {
                assert!(parse(&full).is_err(), "accepted {args:?}");
            }
        }
    }

    #[test]
    fn compression_tiers() {
        for (name, quality, dpi, bits) in
            [("light", 90, 0, false), ("balanced", 85, 0, true), ("medium", 65, 150, true), ("strong", 45, 120, true), ("heavy", 30, 96, true)]
        {
            let o = parse(&["--compression", name, "in.pdf"]).unwrap().opts;
            assert!(o.images && o.image_quality == quality && o.dpi == dpi && o.reduce_bits == bits, "{name}");
            assert!(!(o.bitmap || o.mrc || o.gray || o.cff_fonts || o.remove_standard_fonts || o.privacy));
            assert!(o.flatten.is_empty() && o.strip.is_empty());
        }
        let o = parse(&[
            "--compression",
            "heavy",
            "--image-quality",
            "99",
            "--dpi",
            "0",
            "--clip-images=false",
            "--inline-images=false",
            "--merge-fonts=false",
            "--reduce-bit-depth=false",
            "--color-reduction",
            "preserve",
            "in.pdf",
        ])
        .unwrap()
        .opts;
        assert!(o.image_quality == 99 && o.dpi == 0 && !o.clip && !o.inline && !o.merge_fonts && !o.reduce_bits);
        assert_eq!(o.color_reduction, "preserve");
        for args in [
            &["--compression", "unknown"][..],
            &["--compression", "medium", "--lossless"],
            &["--color-reduction", "unknown"],
            &["--timestamps", "unknown"],
            &["--output", ""],
        ] {
            let mut full = args.to_vec();
            full.push("in.pdf");
            assert!(parse(&full).is_err(), "accepted {args:?}");
        }
        assert!(parse(&["--images=false", "--reduce-bit-depth", "in.pdf"]).is_err());
        for level in ["light", "balanced", "medium", "strong", "heavy"] {
            let o = parse(&["--compression", level, "in.pdf", "--images=false"]).unwrap().opts;
            assert!(!o.images && !o.reduce_bits && o.dpi == 0 && !o.clip && !o.inline, "{level}");
        }
    }

    #[test]
    fn engine_alias_and_flags_anywhere() {
        let parsed = parse(&["in.pdf", "--engine", "pdfcpu", "-V"]).unwrap();
        assert_eq!(parsed.opts.engine, "native");
        assert!(parsed.opts.verbose);
        assert_eq!(parsed.inputs, vec![OsString::from("in.pdf")]);
        let parsed = parse(&["--", "in.pdf", "-V"]).unwrap();
        assert_eq!(parsed.inputs.len(), 2);
        assert!(matches!(parse(&["-h"]), Err(e) if e.is_help()));
        assert!(matches!(parse(&["--help"]), Err(e) if e.is_help()));
        assert!(parse(&["--unknown"]).is_err());
        assert!(parse(&["--jobs"]).is_err());
        assert!(parse(&["-v"]).unwrap().version);
    }

    #[test]
    fn generated_names() {
        for (name, want) in [
            ("a.squeezed.pdf", true),
            ("A.SQUEEZED.PDF", true),
            ("a.squeezed.1.pdf", true),
            ("a.squeezed.x.pdf", false),
            ("a.pdf", false),
            ("squeezed.pdf", false),
        ] {
            assert_eq!(generated_pdf(OsStr::new(name)), want, "{name}");
        }
    }

    #[test]
    fn metadata_dates() {
        assert!(valid_date("D:20260101120000Z"));
        assert!(!valid_date("D:20261301120000Z"));
        assert!(!valid_date("D:20260230120000Z"));
        assert!(valid_date("D:20240229000000Z"));
    }
}
