# pdf-squeezer

A PDF compressor and PDF/A-4 converter written in Go. Default compression needs no external software. Optional local tools add image cropping, scan codecs, font optimization, and archival conversion.

## Install

Download a binary for Linux, macOS, or Windows from the [releases page](https://github.com/hawkff/pdf-squeezer/releases), or install with Go 1.26 or newer:

```sh
go install github.com/hawkff/pdf-squeezer@latest
```

The container image carries every optional tool (Python helpers, qpdf, Ghostscript, veraPDF with a Java runtime, fontconfig with URW and Liberation fonts), so PDF/A conversion works without any setup:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" ghcr.io/hawkff/pdf-squeezer:latest --pdfa 4 report.pdf
```

The image runs as an unprivileged user; `--user` makes the output files yours.

## Use

```sh
# Structural optimization; writes document.squeezed.pdf next to the input
pdf-squeezer document.pdf

# Choose an output file or existing directory
pdf-squeezer document.pdf -o smaller.pdf
pdf-squeezer document.pdf -o out/

# Choose a compression tier without flattening the document
pdf-squeezer --compression medium scan.pdf
pdf-squeezer --compression heavy scan.pdf

# Set image encoding options directly
pdf-squeezer --images --image-quality 75 scan.pdf

# Re-encode without discarding sample precision or recompressing JPEG
pdf-squeezer --lossless document.pdf

# Multiple files, or PDFs in a directory tree
pdf-squeezer first.pdf second.pdf -o out/
pdf-squeezer --recursive documents/ -o out/

# Strip identifying document metadata
pdf-squeezer --privacy document.pdf

# Convert to PDF/A-4, verified by veraPDF
pdf-squeezer --pdfa 4 report.pdf

pdf-squeezer --help
pdf-squeezer --version
```

Flags may come before or after filenames. Use `--` before a filename that starts with a dash. Short flags are `-o` output, `-V` verbose, `-v` version, and `-h` help.

The CLI leaves inputs unchanged and refuses to overwrite outputs, including symlinks. `--collision number` chooses a numbered name instead. Directory scans skip hidden files, symlinks, and generated `.squeezed.pdf` or `.squeezed.N.pdf` outputs. Batch output retains relative subdirectories beneath an existing output directory. A batch reports individual failures and returns a failing exit status if any input fails. For separate destinations, repeat `-o` once per distinct input file. Inputs keep their command-line order; directory inputs cannot use this form. Every destination is checked before processing begins.

```sh
pdf-squeezer first.pdf second.pdf -o out/first.pdf -o elsewhere/second.pdf
```

Compression keeps the original bytes when the result would be no smaller. Explicit document changes, including privacy, metadata edits, grayscale, encryption, removals, flattening, raster conversion, and PDF/A-4 conversion, keep their result even when it is larger. `--force-recompression` also disables the size fallback. Unsupported or unsafe images remain untouched even with that flag.

## Compression tiers

`--compression light|balanced|medium|strong|heavy` sets image-compression defaults. Without this flag, default structural optimization stays unchanged.

| Tier | JPEG quality | Allow 16-bit to 8-bit images | Target color/gray DPI | Other work |
| --- | ---: | --- | ---: | --- |
| `light` | 90 | No | Unchanged | Compare JPEG and lossless Flate |
| `balanced` | 85 | Yes | Unchanged | Same codec comparison |
| `medium` | 65 | Yes | 150 | Crop unused margins; include inline images |
| `strong` | 45 | Yes | 120 | Same placement-aware work |
| `heavy` | 30 | Yes | 96 | Also merge compatible fonts; try maximum-effort Flate with and without PNG predictors |

Light and balanced need no external tools. Medium and strong need the Python tools and qpdf. Heavy also uses the fontTools package included in the Python requirements. Missing tools cause an error, not a fallback to another engine.

Tiers never enable rasterization, flattening, grayscale conversion, metadata stripping, or removal of fonts, tags, links, forms, or attachments. They retain soft-mask precision. Higher tiers permit more image loss; heavy can introduce visible JPEG artifacts around small text and colored edges. They do not guarantee a reduction percentage or a smaller result on every document. The no-size-gain fallback still applies.

DPI is a ceiling, not a request to upscale. An image below the selected target stays at its existing resolution. Tier settings are defaults: profile values override them, and explicit flags override both. For example, `--compression strong --image-quality 70 --dpi 0` retains resolution and uses JPEG quality 70. `--lossless` rejects tier settings that would discard precision or resolution; use it without a tier for preservation.

## Image compression

`--images` handles supported 1-, 8-, and 16-bit image samples. It compares Flate compression with PNG predictors against JPEG, with configurable `--image-quality 1..100` and `--image-codecs flate,jpeg`. A native re-encode normally needs to save at least 2% and 1 KiB.

`--color-reduction exact`, the default, lets the lossy path reduce exactly gray DeviceRGB samples to DeviceGray and exact black-and-white gray samples to 1-bit. `--color-reduction preserve` disables both classifications while still allowing JPEG. It does not disable separately requested `--reduce-bit-depth` or `--gray` conversion. It preserves ICC color spaces and avoids grayscale reclassification when the document overrides default color spaces. `--lossless` forbids JPEG re-encoding, color-space changes, downsampling, and bit-depth reduction. Existing JPEG data stays encoded in that mode.

16-bit samples retain their precision unless `--reduce-bit-depth` permits an 8-bit conversion. Soft masks retain their precision regardless of that flag. Matte-backed images, explicit masks, indexed images, unsupported transfer functions, and unknown filters stay unchanged. The native pass handles identity and inversion `/Decode` arrays and lossless CMYK streams. Use `--extended-images` for supported JPEG 2000 and CMYK JPEG decoding through the optional tools.

`--image-memory 512` sets the native image working-buffer budget in MiB. Workers share this budget and release decoded buffers after each job. It is an estimate, not a process-wide memory ceiling: the parsed document and encoded output also occupy memory. Images above the sample limit or working budget stay unchanged. `-V` reports progress and preservation reasons. Interrupting the CLI cancels processing and terminates optional tool process groups where supported.

After image transformations, the CLI deduplicates equivalent streams. It compares decoded bytes for generalized lossless filters, retains encoding parameters for specialized image codecs, and keeps the smallest equivalent encoding. It also enables pdfcpu's duplicate-content-stream optimization.

## Optional tools

Nothing is installed automatically. A requested tool that is missing or fails stops that operation; the CLI does not silently select another engine.

For the Python operations, create an environment and install `tools/requirements.txt` from this repository, or the release's `pdf-tools-requirements.txt`:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r tools/requirements.txt
export PDF_SQUEEZER_PYTHON="$PWD/.venv/bin/python"
```

On Windows, set `PDF_SQUEEZER_PYTHON` to the environment's `Scripts\python.exe`. Otherwise the CLI looks for `python3` on `PATH`. The processing script is embedded in the Go binary; users do not need to copy it beside the executable.

The optional dependencies have their own licenses. In particular, Ghostscript and PyMuPDF/MuPDF use AGPL or commercial licensing, and veraPDF is GPL/MPL. They are separate installations, not bundled third-party binaries. The binary does embed the ICC sRGB profile `sRGB2014.icc`, which the International Color Consortium distributes without restriction.

### Placement-aware images and scan codecs

```sh
# Downsample according to displayed size, without Ghostscript
pdf-squeezer --dpi 120 --gray-dpi 150 scan.pdf

# Crop unused margins and include inline images
pdf-squeezer --images --clip-images --inline-images document.pdf

# Compare lossless monochrome codecs
pdf-squeezer --images --mono-codecs flate,ccitt,jbig2 scan.pdf

# Optional monochrome downsampling
pdf-squeezer --mono-dpi 300 --mono-codecs flate,ccitt,jbig2 scan.pdf
```

These operations use pikepdf and Pillow. Inline-image externalization uses `qpdf`. CCITT Group 4 requires Pillow's libtiff support. JBIG2 requires [jbig2enc](https://github.com/agl/jbig2enc)'s `jbig2` executable. Its generic-region encoding is lossless; the CLI never enables lossy symbol substitution. Install `jbig2dec` as well to decode existing JBIG2 images through pikepdf.

`--dpi` targets color and gray images; `--gray-dpi` overrides gray resolution. Monochrome resolution stays unchanged unless `--mono-dpi` is supplied. Images must exceed the target times `--dpi-threshold`, which defaults to 1.5, before downsampling. Each image axis is judged on its own, so an image squeezed in one direction loses pixels only along that direction. A shared image uses the most demanding placement across pages and nested forms, including `/UserUnit` and affine transforms.

Cropping unions visible bounds across shared uses and adjusts drawing transforms. It retains a border for interpolation and only crops axis-aligned placements. Lossless mode additionally preserves images on fractional or downsampled display grids to avoid changing a viewer's sampling. It preserves images used in masks, annotation appearances, patterns, or unanalysed forms rather than guessing their placement. Complex clipping paths use conservative bounding boxes. Downsampling remains lossy; inspect fine text before sharing scans.

For monochrome images, the codec comparison may retain full resolution when a smaller lossless encoding costs no more than 3.5 times the downsampled candidate. This favors small-text legibility over minimum byte count.

### Ghostscript

Install a current, security-patched Ghostscript and put `gs` on `PATH`. Windows also supports `gswin64c` and `gswin32c`.

```sh
pdf-squeezer --engine ghostscript --quality ebook scan.pdf
pdf-squeezer --engine ghostscript --dpi 100 --gray scan.pdf
```

Presets are `screen`, `ebook`, `printer`, and `prepress`. The default is `ebook`. They change more than image resolution. Colors retain their original color spaces unless `--gray` requests conversion. With the pdfcpu engine, `--gray` uses the optional Python tools and also converts text and vectors.

If Ghostscript reports an image-decoding failure that would leave a blank page, the CLI rejects its output. Try the pdfcpu engine for that file.

### Fonts and flattening

```sh
pdf-squeezer --subset-fonts --merge-fonts document.pdf
pdf-squeezer --flatten forms,annotations document.pdf
pdf-squeezer --flatten links document.pdf
pdf-squeezer --convert-fonts-cff document.pdf
```

Font subsetting uses MuPDF. Font merging uses fontTools and combines compatible retained-glyph-ID TrueType subsets without rewriting text. It preserves each font's character mapping and excludes incompatible programs and unsupported consumers. It is not a general merger for every font format.

`--convert-fonts-cff` rewrites through Ghostscript's PDF writer with compact font programs. This is a document rewrite, not a byte-preserving font patch. `--remove-standard-fonts` is a separate opt-in option for eligible embedded copies of the standard PDF fonts; it makes rendering depend on viewer-provided substitutes.

Flattening freezes selected form fields, annotations, or links into page content. `--flatten all` selects every category. A visible link border without a usable appearance stream causes an error rather than disappearing. Comments, attached annotation data, and editability may be lost. Flattening is not redaction and does not securely remove underlying page content.

## Profiles and document controls

```sh
pdf-squeezer --images --image-quality 70 --save-profile web.profile.json
pdf-squeezer --profile web.profile.json document.pdf

# Explicit flags override profile values
pdf-squeezer --profile web.profile.json --image-quality 85 document.pdf

pdf-squeezer --metadata 'Title=Public report' --metadata 'Author=' document.pdf
pdf-squeezer --strip thumbnails,alternates,piece-info document.pdf
pdf-squeezer --strip web-capture document.pdf
pdf-squeezer --timestamps modified document.pdf
pdf-squeezer --timestamps now document.pdf
```

Native profiles use versioned JSON and contain settings and optional metadata, never input/output paths, password files, or password values. Existing version-1 files still work. Version-2 bundles contain an ordered `profiles` array of version-1 entries, each with an optional `name`.

`--profile-entry NAME|N` selects an entry by name or 1-based index. A matching name takes precedence over an index; ambiguous names require an index. Without it, the first entry is used. Repeat `--profile` to combine files in argument order. `--save-profile` saves the selected entry's resolved settings and retains the other entries when saving a bundle. Export uses native JSON only.

```sh
pdf-squeezer --compression light --save-profile light.profile.json
pdf-squeezer --compression strong --save-profile strong.profile.json
pdf-squeezer --profile light.profile.json --profile strong.profile.json \
  --profile-entry strong --save-profile presets.profile.json
pdf-squeezer --profile presets.profile.json --profile-entry 2 document.pdf
pdf-squeezer --profile exported.pdfscp --profile-entry 1 document.pdf
```

`.pdfscp` import accepts XML or binary property lists containing an array of settings dictionaries. It requires Python 3's standard-library `plistlib`, not the optional PDF packages. It maps supported image, font, removal, metadata and date fields to CLI settings. Normalized `imageQuality` values become JPEG quality 1..100. Fields absent from the file use CLI defaults; the importer does not reproduce another engine's internal compression rules.

Unknown fields, unverified nonzero `colorConversion` values, disabled structural optimization, and automatic Producer replacement cause an error rather than a guessed conversion. Use a native profile for those settings. Timestamp import preserves the CLI's portable filesystem behavior, not macOS birth-time changes. A profile can request destructive transformations independently of a compression tier, so inspect imported profiles before using them.

`--metadata KEY=VALUE` is repeatable. Supported keys are `Title`, `Author`, `Subject`, `Keywords`, `Creator`, `Producer`, `CreationDate`, and `ModDate`. An empty value removes that key. Dates use `D:YYYYMMDDhhmmssZ`. Editing these fields removes XMP copies that could retain contradictory values.

`--timestamps preserve` retains existing document dates and the input's filesystem modification time. It is the default. `--timestamps modified` updates only the document modification date, preserving its creation date. `--timestamps now` updates both document dates. Both update modes leave the output with its new filesystem timestamp and remove stale XMP copies. The CLI does not promise portable filesystem birth-time preservation.

`--privacy` empties the document information dictionary, removes XMP metadata, application piece information, and web-capture information, and creates a fresh document identifier. Page content, comments, form values, and attachments can still contain personal information. Check those separately.

`--strip` accepts a comma-separated list:

- `thumbnails`, `alternates`, `threads`, `piece-info`, or `metadata`. Here `metadata` means XMP; use `--privacy` to clear document information too.
- `web-capture` removes the catalog's SpiderInfo web-capture data without clearing document information or XMP.
- `tags` removes accessibility structure and reading-order information.
- `output-intents` removes document color/output-intent information.
- `links`, `annotations`, or `forms` removes those objects rather than flattening their appearances.
- `images` removes image drawing objects, including externalized inline images.
- `actions` removes actions that PDF/A forbids (Launch, Sound, Movie, ResetForm, ImportData, Hide, Rendition, Trans, SetOCGState, GoTo3DView, non-navigation named actions), actions on form fields, and page or document event actions. GoTo, URI, SubmitForm, and JavaScript actions stay.
- `multimedia` removes Sound, Screen, Movie, 3D, and RichMedia annotations.
- `hidden` removes hidden and no-view annotations.
- `xfa` removes XFA form data; the AcroForm fields stay.
- `attachments` removes embedded files and file attachment annotations.

The last five categories use the optional Python tools. None of these removals is enabled by default. Do not remove accessibility tags or color information merely to save a few bytes.

## Passwords and encryption

```sh
pdf-squeezer --password-file reader-password.txt protected.pdf
pdf-squeezer --password-file owner-password.txt --decrypt protected.pdf
pdf-squeezer --encrypt-user-file reader-password.txt \
  --encrypt-owner-file owner-password.txt --permissions print document.pdf
```

Password files contain one line. Password values never enter command arguments or saved profiles. New encryption uses AES-256 and requires a nonempty owner password different from the user password. Permissions are `all`, `print`, or `none`; PDF viewers determine how they enforce restrictions.

Encrypted inputs require qpdf. The CLI uses private decrypted working copies, then preserves the input's encryption unless `--decrypt` or new output encryption was requested. For a fresh identifier, `--privacy` on encrypted input requires explicit decryption or new encryption. Protect the working filesystem as well as the final document.

## PDF/A

```sh
pdf-squeezer --pdfa 4 report.pdf
pdf-squeezer --pdfa 3b invoice.pdf
pdf-squeezer --pdfa 4 --output-intent ISOcoated_v2.icc brochure.pdf
pdf-squeezer --pdfa 4 --font-file 'Verdana=/path/to/verdana.ttf' slides.pdf
pdf-squeezer --pdfa 2b --strip actions,hidden form.pdf
pdf-squeezer --pdfa 4 --images --dpi 150 --image-quality 80 scan.pdf
```

`--pdfa LEVEL` converts the compressed document to PDF/A-2b, PDF/A-3b, or PDF/A-4 (ISO 19005) and publishes it only after [veraPDF](https://verapdf.org/) confirms compliance. PDF/A-4 documents with embedded files become PDF/A-4f; PDF/A-3b associates every embedded file with the document; PDF/A-2b refuses attachments, since it only permits ones that are PDF/A themselves. PDF/A-2 and 3 also forbid JavaScript and event actions, and keep only predefined XMP schemas, so custom XMP properties without an extension schema are dropped. Ghostscript cannot produce PDF/A-4, so the conversion is this project's own code on top of pikepdf, fontTools, and MuPDF.

A batch runs veraPDF once per group of files rather than once per file.

It requires the Python tools, qpdf, veraPDF with a Java runtime on `PATH`, and fontconfig with metric-compatible fonts for the standard 14 fonts: the URW Base35 family (`fonts-urw-base35`) or Liberation (`fonts-liberation`).

The converter repairs what it can without changing page content:

- PDF 2.0 header, file identifier, XMP identification, document information moved into XMP, and PDF 2.0 deprecations such as LZW streams, transfer functions, halftone settings, image alternates, OPI, reference XObjects, and interpolation flags.
- Resources inherited by form XObjects, annotation appearances, patterns, and Type 3 glyphs become explicit.
- Missing appearance streams for annotations and form fields are generated with MuPDF.
- Soft masks follow their images when `--dpi` or `--clip-images` downsample or crop them, as long as the mask has the image's size and no matte.
- Non-embedded Helvetica, Times, Courier, Symbol, ZapfDingbats, and their Arial, Times New Roman, and Courier New aliases are embedded from a metric-compatible font. The document's glyph widths must match; otherwise supply a font with `--font-file NAME=PATH`. A supplied font keeps the document's widths: its glyph advances are set to them, so text does not move. Composite (Type 0) fonts need the original TrueType file through `--font-file`, because their glyph identifiers only match that file.
- Embedded fonts get consistent widths (font programs are patched, so layout does not change), TrueType cmap subtables, symbolic encoding rules, CIDToGIDMap entries, and valid ToUnicode mappings. Fonts and glyphs count as used only when drawn, including through forms, patterns, Type 3 glyphs, and appearance streams; text in rendering mode 3 does not require embedding.
- An output intent is added. The bundled sRGB profile covers DeviceGray and DeviceRGB content. DeviceCMYK content needs a CMYK ICC profile through `--output-intent`; the converter then adds a DefaultRGB color space for any RGB content.
- Optional content configurations, permissions, and embedded file specifications (MIME type, names, `AFRelationship`) are normalized.

It refuses to guess. Forbidden features stop the conversion with the object number and the `--strip` category that removes them: forbidden actions, multimedia annotations, hidden annotations, XFA data, or attachments for a plain PDF/A-4. Unavailable fonts, mismatched widths, and ambiguous color conversions produce errors that name the font or the profile to supply. When veraPDF still rejects the result, the diagnostics list each failed rule with its clause, the affected objects, and the option that resolves it. Nothing is flattened, rasterized, or discarded to pass validation; `-V` reports every repair.

PDF/A forbids encryption and requires metadata, color information, and embedded fonts, so `--pdfa` rejects output encryption, `--privacy`, `--strip metadata`, `--strip output-intents`, and `--remove-standard-fonts`. Encrypted input needs `--decrypt`.

### Inputs that already are PDF/A

Without `--pdfa`, an input that declares PDF/A keeps its level: a PDF/A-1 document is written with a classic cross-reference table and no object streams, and when veraPDF is on `PATH` the output is verified against the declared level before it is published. If compression broke conformance the file is not published; if the input did not conform in the first place, a warning says so and the output is published unverified. Options that remove what PDF/A requires (`--privacy`, `--metadata`, `--timestamps now`, `--strip metadata|output-intents`, raster output, a Ghostscript rewrite) print a warning and skip verification.

### Checking a corpus

`tools/corpus.py` converts every PDF under a directory and tabulates the outcomes: converted, refused (grouped by the option the converter asks for), rejected by veraPDF (grouped by rule), crashed, or timed out.

```sh
python3 tools/corpus.py --pdfa 4 --binary ./pdf-squeezer documents/ results/
```

Known limits: non-Identity CMaps and CFF-based CIDFonts without embedded programs, glyphs missing from embedded fonts, and JPEG 2000 streams that violate PDF/A constraints are reported, not repaired.

## Extraction and raster output

```sh
pdf-squeezer --extract images -o extracted-images document.pdf
pdf-squeezer --extract images --inline-images -o extracted-images document.pdf
pdf-squeezer --extract text -o document.txt document.pdf

pdf-squeezer --bitmap --render-dpi 200 document.pdf
pdf-squeezer --mrc --render-dpi 300 --background-dpi 72 scan.pdf
```

Image extraction creates a new directory with object-number-based filenames. For a single input, its `-o` directory must not exist. Native extraction handles supported image XObjects; `--inline-images` includes inline images through qpdf. Text extraction uses MuPDF and creates UTF-8 text with page separators.

Bitmap output replaces pages with rendered images. MRC output separates a high-resolution dark-text mask from a lower-resolution JPEG background. Its luminance threshold suits document scans, not photographs or complex artwork. Both modes discard searchable text, vector objects, interactive fields, links, tags, and attachments. They are opt-in and are not redaction tools.

## File safety

- All output is staged and validated before exclusive destination creation. Inputs and existing outputs are never replaced or moved to Trash.
- A crash during the final copy can leave an incomplete new destination. Remove it before retrying. A reported write failure removes partial output.
- Temporary working files and output use private permissions where the filesystem supports them. Keep enough space for intermediates and the final copy.
- Compression invalidates digital signatures. Ghostscript, font rewriting, flattening, rasterization, and PDF/A-4 conversion can change document features. Keep originals and inspect results.
- Default compression and the open-source processing tools run locally. Keep PDF parsers and optional tools patched before processing untrusted documents.

## Development

CI runs on Namespace. It runs Go tests with the race detector, `go vet`, formatting checks, Python lint checks, and synthetic render-comparison tests. It cross-compiles standalone binaries for the supported targets.

The tests cover shared-image cropping and DPI, masks and scan codecs, CMYK/JPEG 2000, font merging/subsetting, flattening and raster output, and PDF/A conversion of fonts, color, attachments, annotations, and forbidden features at every level, each verified with veraPDF. CI also builds the container image and pushes it on release tags. A missing optional dependency or a noncompliant validator report must fail without publishing an output.

## License

[MIT](LICENSE) for this repository's code. Separately installed tools retain their own licenses.
