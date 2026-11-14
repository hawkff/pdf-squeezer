use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::bail;
use crate::error::{Ctx, Error, Result, path_error};
use crate::files::go_ext;
use crate::options::list_contains;
use crate::process::{self, Out, Sink, Tool, look_path, shared};
use crate::tools::{python_command, tool_error};

pub const MAX_PROFILE_BYTES: usize = 1 << 20;

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub version: i64,
    #[serde(default, deserialize_with = "null_default", skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub flags: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "null_default", skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBundle {
    pub version: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub profiles: Vec<Profile>,
}

/// Tier settings are defaults, below explicit profile values and command-line flags.
pub fn compression_flags(name: &str) -> Result<Vec<(&'static str, String)>> {
    let (quality, dpi) = match name {
        "light" => (90, 0),
        "balanced" => (85, 0),
        "medium" => (65, 150),
        "strong" => (45, 120),
        "heavy" => (30, 96),
        _ => bail!("--compression must be light, balanced, medium, strong, or heavy"),
    };
    Ok(vec![
        ("images", "true".into()),
        ("image-quality", quality.to_string()),
        ("reduce-bit-depth", (name != "light").to_string()),
        ("dpi", dpi.to_string()),
        ("clip-images", (dpi > 0).to_string()),
        ("inline-images", (dpi > 0).to_string()),
        ("merge-fonts", (name == "heavy").to_string()),
    ])
}

fn strict<'a, T: Deserialize<'a>>(data: &'a [u8]) -> std::result::Result<T, String> {
    let mut de = serde_json::Deserializer::from_slice(data);
    let value = T::deserialize(&mut de).map_err(|e| e.to_string())?;
    de.end().map_err(|_| "profile has trailing data".to_string())?;
    Ok(value)
}

pub fn load_profiles(ctx: &Ctx, path: &Path, stderr: &Out) -> Result<Vec<Profile>> {
    let file = fs::File::open(path).map_err(|e| path_error("open", path, &e))?;
    let mut data = Vec::new();
    file.take(MAX_PROFILE_BYTES as u64 + 1).read_to_end(&mut data)?;
    if data.len() > MAX_PROFILE_BYTES {
        bail!("profile exceeds 1 MiB");
    }
    let trimmed = data.trim_ascii();
    if go_ext(path.as_os_str()).eq_ignore_ascii_case(b".pdfscp") || data.starts_with(b"bplist") || trimmed.starts_with(b"<") {
        return load_legacy_profiles(ctx, &data, stderr);
    }
    let header: Value = serde_json::from_slice(&data).map_err(|e| Error::new(format!("profile: {e}")))?;
    let version = match header.get("version") {
        None => 0,
        Some(v) => v.as_i64().ok_or_else(|| Error::new("profile: version must be an integer"))?,
    };
    if !header.is_object() {
        bail!("profile: expected a JSON object");
    }
    let profiles = match version {
        1 => vec![strict::<Profile>(&data).map_err(|e| Error::new(format!("profile: {e}")))?],
        2 => strict::<ProfileBundle>(&data).map_err(|e| Error::new(format!("profile bundle: {e}")))?.profiles,
        _ => bail!("unsupported profile version; expected 1 or bundle version 2"),
    };
    if profiles.is_empty() {
        bail!("profile bundle is empty");
    }
    if profiles.iter().any(|p| p.version != 1) {
        bail!("bundle entries must be version 1 profiles");
    }
    Ok(profiles)
}

pub fn select_profile(profiles: &[Profile], selector: &str) -> Result<usize> {
    if selector.is_empty() {
        return Ok(0);
    }
    if profiles.is_empty() {
        bail!("--profile-entry requires --profile");
    }
    let mut selected = None;
    for (i, p) in profiles.iter().enumerate() {
        if p.name == selector {
            if selected.is_some() {
                bail!("profile name is ambiguous; select a 1-based index");
            }
            selected = Some(i);
        }
    }
    if let Some(i) = selected {
        return Ok(i);
    }
    match selector.parse::<usize>() {
        Ok(index) if (1..=profiles.len()).contains(&index) => Ok(index - 1),
        _ => bail!("profile entry {selector:?} not found; choose a name or index 1..{}", profiles.len()),
    }
}

const PLIST_SCRIPT: &str = "import json, plistlib, sys\ntry:\n json.dump(plistlib.loads(sys.stdin.buffer.read()), sys.stdout, allow_nan=False)\nexcept Exception:\n sys.exit(1)\n";

/// plistlib reads binary and XML property lists without executing archived objects.
/// Imported files hold an ordered array of settings dictionaries.
fn load_legacy_profiles(ctx: &Ctx, data: &[u8], stderr: &Out) -> Result<Vec<Profile>> {
    let Some(python) = look_path(&python_command()) else {
        bail!(".pdfscp import requires Python 3 with its standard-library plistlib; set PDF_SQUEEZER_PYTHON or add python3 to PATH");
    };
    let ctx = ctx.with_timeout(Duration::from_secs(15));
    let (output, messages) = (shared(), shared());
    let tool = Tool {
        stdin: Some(data.to_vec()),
        stdout: Sink::Buffer(output.clone()),
        stderr: Sink::Buffer(messages.clone()),
        ..Tool::new(python).arg("-I").arg("-c").arg(PLIST_SCRIPT)
    };
    let result = process::run(&ctx, tool);
    tool_error(&ctx, ".pdfscp property list", result.err(), &messages.lock().unwrap(), stderr, &[])?;
    let output = output.lock().unwrap();
    if output.truncated {
        bail!("decoded profile exceeds 1 MiB");
    }
    let dictionaries: Vec<Map<String, Value>> = strict(output.bytes()).unwrap_or_default();
    if dictionaries.is_empty() {
        bail!(".pdfscp must contain a nonempty property-list array of settings dictionaries");
    }
    dictionaries.iter().enumerate().map(|(i, settings)| legacy_profile(settings).map_err(|e| e.context(format!(".pdfscp entry {}", i + 1)))).collect()
}

const LEGACY_BOOLS: &[(&str, &str)] = &[
    ("optimizeImages", "images"),
    ("clipImages", "clip-images"),
    ("forceRecompression", "force-recompression"),
    ("convertToCFF", "convert-fonts-cff"),
    ("mergeEmbeddedFonts", "merge-fonts"),
    ("removeStandardFonts", "remove-standard-fonts"),
    ("subsetFonts", "subset-fonts"),
    ("createPDFA4", "pdfa"),
];

const LEGACY_STRIPS: &[(&str, &str)] = &[
    ("stripMetadata", "metadata"),
    ("stripPieceInfo", "piece-info"),
    ("stripThreads", "threads"),
    ("stripThumbnails", "thumbnails"),
    ("stripSpiderInfo", "web-capture"),
    ("stripAlternateImages", "alternates"),
    ("stripDocumentStructureTree", "tags"),
    ("stripTheDocumentOutputIntents", "output-intents"),
    ("removeAllImages", "images"),
];

const LEGACY_FLATTEN: &[(&str, &str)] = &[
    ("stripAndFlattenFormFields", "forms"),
    ("stripAndFlattenLinkAnnotations", "links"),
    ("stripAndFlattenAnnotationsExceptFormFieldsAndLinks", "annotations"),
    ("flattenRemainingAnnotations", "annotations"),
];

const LEGACY_OTHER: &str = "reduceColorComplexity,downsampleMonochromeScans,optimizeResources,removeRedundantObjects,removeTitle,removeAuthor,removeSubject,removeKeywords,removeCreator,removeProducer,updateModificationDate,updateModificationAndCreationDate,hasBeenCreatedByApp";

fn lookup(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

pub fn legacy_profile(settings: &Map<String, Value>) -> Result<Profile> {
    let mut p = Profile { version: 1, name: String::new(), flags: BTreeMap::new(), metadata: Vec::new() };
    let mut values: BTreeMap<String, bool> = BTreeMap::new();
    let mut text: BTreeMap<String, String> = BTreeMap::new();
    let mut numbers: BTreeMap<String, f64> = BTreeMap::new();
    for (key, raw) in settings {
        match key.as_str() {
            "title" | "uuid" | "customTitle" | "customAuthor" | "customSubject" | "customKeywords" | "customCreator" | "customProducer" => {
                let Some(value) = raw.as_str() else { bail!("{key} must be a string") };
                text.insert(key.clone(), value.to_string());
            }
            "imageQuality" | "imageResolution" | "colorConversion" | "rank" => {
                let Some(value) = raw.as_f64().filter(|v| v.is_finite()) else { bail!("{key} must be a finite number") };
                numbers.insert(key.clone(), value);
            }
            _ => {
                let known = lookup(LEGACY_BOOLS, key).is_some()
                    || lookup(LEGACY_STRIPS, key).is_some()
                    || lookup(LEGACY_FLATTEN, key).is_some()
                    || list_contains(LEGACY_OTHER, key);
                if !known {
                    bail!("unsupported profile field {key:?}");
                }
                let value = match raw {
                    Value::Null => bail!("{key} must be a boolean"),
                    Value::Bool(b) => *b,
                    other => match other.as_i64() {
                        Some(0) => false,
                        Some(1) => true,
                        _ => bail!("{key} must be a boolean or 0/1"),
                    },
                };
                values.insert(key.clone(), value);
            }
        }
    }
    p.name = text.get("title").cloned().unwrap_or_default();
    for (key, flag) in LEGACY_BOOLS {
        if let Some(&value) = values.get(*key) {
            if *key == "createPDFA4" {
                if value {
                    p.flags.insert((*flag).into(), "4".into());
                }
            } else {
                p.flags.insert((*flag).into(), value.to_string());
            }
        }
    }
    let quality = numbers.get("imageQuality").copied();
    let dpi = numbers.get("imageResolution").copied();
    if quality.is_some_and(|q| !(0.0..=1.0).contains(&q)) || dpi.is_some_and(|d| !(0.0..=9600.0).contains(&d) || d.trunc() != d) {
        bail!("imageQuality must be in 0..1 and imageResolution must be an integer in 0..9600");
    }
    if values.get("optimizeImages") == Some(&false) {
        // Stored image settings are inactive when image optimization is disabled.
        p.flags.insert("clip-images".into(), "false".into());
        p.flags.insert("force-recompression".into(), "false".into());
    } else {
        if let Some(q) = quality {
            // Keep truncation while correcting one-ULP loss at whole quality steps.
            p.flags.insert("image-quality".into(), ((q * 100.0).next_up() as i64).max(1).to_string());
        }
        if let Some(d) = dpi {
            let d = (d as i64).to_string();
            if values.get("downsampleMonochromeScans") == Some(&true) {
                p.flags.insert("mono-dpi".into(), d.clone());
            }
            p.flags.insert("dpi".into(), d);
        }
    }
    if let Some(&value) = values.get("reduceColorComplexity") {
        p.flags.insert("color-reduction".into(), if value { "exact" } else { "preserve" }.into());
    }
    let conversion = numbers.get("colorConversion").copied().unwrap_or(0.0);
    if conversion != 0.0 {
        bail!("colorConversion {conversion} has no verified mapping; export with color conversion disabled, or use a native profile with --gray");
    }
    for key in ["optimizeResources", "removeRedundantObjects"] {
        if values.get(key) == Some(&false) {
            bail!("{key}=false is unsupported: structural optimization is always enabled");
        }
    }
    let mut append = |flag: &str, item: &str| {
        let entry = p.flags.entry(flag.into()).or_default();
        if !list_contains(entry, item) {
            if !entry.is_empty() {
                entry.push(',');
            }
            entry.push_str(item);
        }
    };
    // Fixed ordering keeps profile exports deterministic.
    for (key, item) in LEGACY_STRIPS {
        if values.get(*key) == Some(&true) {
            append("strip", item);
        }
    }
    for (key, item) in LEGACY_FLATTEN {
        if values.get(*key) == Some(&true) {
            append("flatten", item);
        }
    }
    for key in ["Title", "Author", "Subject", "Keywords", "Creator", "Producer"] {
        if values.get(&format!("remove{key}")) == Some(&true) {
            let custom = text.get(&format!("custom{key}")).cloned().unwrap_or_default();
            if key == "Producer" && custom.is_empty() {
                bail!("automatic Producer replacement is unsupported; supply a customProducer or use native metadata editing");
            }
            p.metadata.push(format!("{key}={custom}"));
        }
    }
    let timestamps = if values.get("updateModificationAndCreationDate") == Some(&true) {
        "now"
    } else if values.get("updateModificationDate") == Some(&true) {
        "modified"
    } else {
        "preserve"
    };
    p.flags.insert("timestamps".into(), timestamps.into());
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> Map<String, Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn legacy_quality_truncation() {
        for (input, want) in [("0", "1"), ("0.57", "57"), ("0.58", "58"), ("0.5799", "57"), ("1", "100")] {
            let p = legacy_profile(&settings(&format!(r#"{{"imageQuality":{input},"imageResolution":144}}"#))).unwrap();
            assert_eq!(p.flags["image-quality"], want, "{input}");
        }
    }

    #[test]
    fn legacy_rejects_ambiguous_mappings() {
        for data in [
            r#"{"imageQuality":1.1}"#,
            r#"{"imageResolution":144.5}"#,
            r#"{"optimizeImages":false,"colorConversion":1}"#,
            r#"{"optimizeImages":false,"optimizeResources":false}"#,
            r#"{"optimizeImages":false,"removeProducer":true}"#,
            r#"{"optimizeImages":false,"unknown":true}"#,
            r#"{"optimizeImages":null}"#,
            r#"{"optimizeImages":false,"clipImages":2}"#,
        ] {
            assert!(legacy_profile(&settings(data)).is_err(), "{data}");
        }
    }

    #[test]
    fn selection() {
        let p = |name: &str| Profile { version: 1, name: name.into(), flags: BTreeMap::new(), metadata: vec![] };
        assert!(select_profile(&[p("same"), p("same")], "same").is_err());
        assert!(select_profile(&[], "2").is_err());
        assert_eq!(select_profile(&[p("a"), p("b")], "b").unwrap(), 1);
        assert_eq!(select_profile(&[p("a"), p("b")], "2").unwrap(), 1);
        assert!(select_profile(&[p("a")], "0").is_err());
        assert_eq!(select_profile(&[p("a"), p("b")], "+2").unwrap(), 1);
    }
}
