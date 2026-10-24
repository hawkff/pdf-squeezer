# pdf-squeezer

pdf-squeezer compresses PDF files and converts them to PDF/A. It is a single Go binary for Linux and macOS, and its default compression needs no other software. Optional tools add downsampling, cropping, scan codecs, font work, and PDF/A conversion.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Optional tools](#optional-tools)
- [Compression levels](#compression-levels)
- [Images](#images)
- [Output files and batches](#output-files-and-batches)
- [Document changes](#document-changes)
- [Profiles](#profiles)
- [Passwords and encryption](#passwords-and-encryption)
- [PDF/A](#pdfa)
- [Extraction and raster output](#extraction-and-raster-output)
- [Troubleshooting](#troubleshooting)
- [Safety notes](#safety-notes)
- [Development](#development)
- [License](#license)

## Install

Download a binary from the [releases page](https://github.com/hawkff/pdf-squeezer/releases). Builds exist for `linux_amd64`, `linux_arm64`, `darwin_arm64` for Apple silicon, and `darwin_amd64` for Intel Macs. Check the file against `checksums.txt` and put it on your `PATH`:

```sh
asset=pdf-squeezer_linux_amd64
curl -fsSLO "https://github.com/hawkff/pdf-squeezer/releases/latest/download/$asset"
curl -fsSLO https://github.com/hawkff/pdf-squeezer/releases/latest/download/checksums.txt
sha256sum --ignore-missing -c checksums.txt   # on macOS: shasum -a 256 --ignore-missing -c checksums.txt
mkdir -p ~/.local/bin && install -m 755 "$asset" ~/.local/bin/pdf-squeezer
```

On macOS, Gatekeeper blocks this binary if you download it with a browser, because it is not notarized. After you install it, run `xattr -d com.apple.quarantine ~/.local/bin/pdf-squeezer`.

With Go 1.26 or newer, `go install github.com/hawkff/pdf-squeezer@latest` builds and installs it.

The container image includes every optional tool. `--user` lets it write to the current directory as you:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" ghcr.io/hawkff/pdf-squeezer:latest --pdfa 4 report.pdf
```

## Quick start

```sh
pdf-squeezer document.pdf                     # writes document.squeezed.pdf next to the input
pdf-squeezer document.pdf -o smaller.pdf      # picks the output file
pdf-squeezer --compression balanced scan.pdf  # recompresses images with the built-in encoder
pdf-squeezer --compression medium scan.pdf    # also downsamples and crops images; needs the optional tools
pdf-squeezer --recursive documents/ -o out/   # every PDF in a directory tree
pdf-squeezer --privacy document.pdf           # removes identifying metadata
pdf-squeezer --pdfa 4 report.pdf              # converts to PDF/A-4, checked by veraPDF
```

Each run prints the old and new size. If a file does not shrink, the output is a copy of the original, and the CLI suggests an option to try. Flags can go before or after file names. `-V` reports progress, and `pdf-squeezer --help` lists every flag.

## Optional tools

The CLI installs nothing itself. If an option needs a missing tool, that file fails with an error. The CLI never switches to another method on its own.

| Tool | Needed for |
| --- | --- |
| Python packages from `tools/requirements.txt` | `--compression medium` and higher, `--clip-images`, `--extended-images`, `--mono-codecs`, `--flatten`, `--subset-fonts`, `--merge-fonts`, `--bitmap`, `--mrc`, `--extract text`, five `--strip` categories, `--pdfa`, and, with the default engine, DPI limits and `--gray` |
| qpdf | Encrypted input, `--inline-images`, `--strip images`, `--compression medium` and higher, and `--pdfa` |
| `jbig2` from [jbig2enc](https://github.com/agl/jbig2enc) | `--mono-codecs jbig2` |
| jbig2dec | Reading JBIG2 images already in a PDF |
| Ghostscript | `--engine ghostscript` and `--convert-fonts-cff` |
| veraPDF 1.30.3 or newer, with Java | `--pdfa`, and checking files that declare PDF/A |
| fontconfig with URW base 35 or Liberation fonts | `--pdfa` on files whose standard fonts are not embedded |
| Python 3 | Importing `.pdfscp` profiles |

The rest needs nothing extra. That includes `--images`, `--compression light` and `balanced`, metadata and privacy options, output encryption, and image extraction.

### Python packages

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r tools/requirements.txt   # or the release asset pdf-tools-requirements.txt
export PDF_SQUEEZER_PYTHON="$PWD/.venv/bin/python"
```

The binary carries its own Python scripts, so you install only the packages. Without `PDF_SQUEEZER_PYTHON`, the CLI uses `python3` from your `PATH`. It runs Python in isolated mode, which ignores `PYTHONPATH` and packages from `pip install --user`.

### Other tools

Debian and Ubuntu package everything except veraPDF and jbig2enc's `jbig2`:

```sh
sudo apt install qpdf ghostscript jbig2dec default-jre-headless fontconfig fonts-urw-base35 fonts-liberation python3-venv
```

On macOS, these Homebrew commands install everything except veraPDF and the Java runtime it needs. On Intel Macs, pip builds pikepdf and PyMuPDF from source, which needs the Xcode command line tools.

```sh
brew install qpdf ghostscript jbig2dec jbig2enc fontconfig
brew install --cask font-urw-base35 font-liberation
```

Use [veraPDF](https://verapdf.org/) 1.30.3 or newer, because older releases leave a log file in `/tmp` on every run. From a clone of this repository, `sh tools/install-verapdf.sh "$HOME/.local/verapdf"` installs a checksummed 1.30.3. Add that directory to your `PATH` afterwards. The script needs Java, `curl`, and `unzip`.

Ghostscript and PyMuPDF/MuPDF use AGPL or commercial licenses, and veraPDF uses GPL and MPL. The release binaries include none of them. The binary embeds the freely distributable ICC sRGB profile `sRGB2014.icc`.

## Compression levels

`--compression` picks a set of image settings. Without it, the CLI optimizes the PDF structure and leaves images alone.

| Level | JPEG quality | 16-bit images to 8-bit | Color and gray DPI limit | Extra work |
| --- | ---: | --- | ---: | --- |
| `light` | 90 | No | None | |
| `balanced` | 85 | Yes | None | |
| `medium` | 65 | Yes | 150 | Crops unused image margins and includes inline images |
| `strong` | 45 | Yes | 120 | Same as `medium` |
| `heavy` | 30 | Yes | 96 | Same as `medium`, plus font merging and maximum-effort Flate |

Every level compares JPEG with lossless Flate for each image and keeps the smaller one. `medium` and higher need the Python packages and qpdf. Levels never rasterize, flatten, convert to grayscale, or remove content. `heavy` can leave JPEG artifacts around small text and colored edges, so check fine print before you share the result.

The DPI limit is a ceiling, and images below it keep their resolution. Flags override profile values, and profile values override the level. `--compression strong --dpi 0` therefore keeps every image's resolution. `--lossless` rejects levels that lower precision or resolution.

### Ghostscript engine

```sh
pdf-squeezer --engine ghostscript --quality ebook scan.pdf
pdf-squeezer --engine ghostscript --dpi 100 --gray scan.pdf
```

`--engine ghostscript` rewrites the whole file with Ghostscript instead of the default `pdfcpu` engine. The `--quality` presets are `screen`, `ebook`, `printer`, and `prepress`, and `ebook` is the default. They change more than image resolution. If Ghostscript skips an image it cannot decode, the CLI rejects the output, because that page would come out blank.

## Images

`--images` recompresses images with the built-in encoder. For each image, it compares lossless Flate with JPEG and keeps the smaller encoding if it saves at least 2% and 1 KiB.

```sh
pdf-squeezer --images --image-quality 75 scan.pdf     # JPEG quality from 1 to 100; 75 is the default
pdf-squeezer --images --image-codecs flate scan.pdf   # never uses JPEG
pdf-squeezer --lossless document.pdf                  # recompresses images without loss
```

- `--lossless` turns on `--images` and forbids JPEG, color-space changes, downsampling, and bit-depth reduction.
- RGB images whose pixels are all gray become grayscale, and grayscale images with only black and white become 1-bit. `--color-reduction preserve` turns this off.
- 16-bit images stay 16-bit unless you add `--reduce-bit-depth`. Soft masks always keep their precision.
- `--gray` converts all page colors to grayscale.
- `--force-recompression` keeps the new encoding even when it saves nothing.
- `--extended-images` adds JPEG 2000 and CMYK JPEG images.
- Images with a matte, explicit masks, indexed colors, unsupported transfer functions, or unknown filters stay unchanged.
- `--image-memory` sets the memory budget for image buffers in MiB, 512 by default. Images that need more stay unchanged. `-V` prints each image the CLI kept and why.

### Downsampling and scan codecs

These options need the Python packages.

```sh
pdf-squeezer --dpi 120 --gray-dpi 150 scan.pdf                    # downsamples to the displayed size
pdf-squeezer --images --clip-images --inline-images document.pdf  # crops unused margins, includes inline images
pdf-squeezer --images --mono-codecs flate,ccitt,jbig2 scan.pdf    # picks the best lossless black-and-white codec
```

- `--dpi` limits color and gray images, and `--gray-dpi` overrides it for gray ones. `--mono-dpi` sets a limit for black-and-white images, which otherwise keep their resolution.
- The CLI measures each image at the size the page draws it. It downsamples an image only when it exceeds the limit times `--dpi-threshold`, which defaults to 1.5. A shared image keeps enough pixels for its largest use.
- Cropping applies only to axis-aligned images. Images in masks, patterns, annotation appearances, or forms the CLI cannot analyze stay whole.
- CCITT Group 4 and JBIG2 encoding is lossless. The CLI never enables the lossy symbol matching in JBIG2.

Downsampling loses detail, so check fine text before you share a scan.

## Output files and batches

- Without `-o`, each output goes next to its input as `NAME.squeezed.pdf`.
- `-o` takes a file name for one input, or an existing directory for any number. Outputs inside that directory keep the subdirectories of their inputs.
- To give each input its own destination, repeat `-o` once per input file, in order: `pdf-squeezer a.pdf b.pdf -o out/a.pdf -o elsewhere/b.pdf`. This form does not accept directory inputs.
- The CLI checks every destination before it starts and never overwrites an existing file or symlink. `--collision number` writes `NAME.squeezed.1.pdf` and so on instead of stopping.
- A directory input covers its top-level `.pdf` files, and `--recursive` adds subdirectories. Scans skip hidden files and directories, symlinks inside the tree, and earlier `.squeezed.pdf` outputs.
- `--jobs N` processes up to N files at once, from 1 to 64, and keeps reports and outputs in input order. Each job has its own `--image-memory` budget.
- A batch reports each failed file, continues with the rest, and exits with a nonzero status if any file failed.

If compression does not make a file smaller, the output is a copy of the original. Options that change the document on purpose, such as `--privacy`, `--strip`, `--flatten`, `--gray`, or `--pdfa`, keep their result even when it grows. So does `--force-recompression`.

## Document changes

```sh
pdf-squeezer --privacy document.pdf
pdf-squeezer --metadata 'Title=Public report' --metadata 'Author=' document.pdf
pdf-squeezer --timestamps modified document.pdf
pdf-squeezer --strip thumbnails,piece-info document.pdf
pdf-squeezer --subset-fonts --merge-fonts document.pdf
pdf-squeezer --flatten forms,annotations document.pdf
```

`--privacy` empties the document information, removes XMP metadata, application data, and web-capture data, and gives the file a new identifier. Page content, comments, form values, and attachments can still hold personal data, so check them yourself.

`--metadata KEY=VALUE` sets `Title`, `Author`, `Subject`, `Keywords`, `Creator`, `Producer`, `CreationDate`, or `ModDate`. Dates use the form `D:YYYYMMDDhhmmssZ`. Repeat the flag for more keys, and leave the value empty to remove one. It also removes XMP metadata, which could contradict the new values. It cannot be combined with `--privacy`.

`--timestamps preserve`, the default, keeps the document dates and the input file's modification time. `modified` sets the document's modification date to now, and `now` sets both document dates. Both also remove XMP metadata.

### Removing content

`--strip` removes the categories you list, separated by commas. Removing tags or output intents to save a few bytes costs accessibility and color accuracy.

<details>
<summary>All <code>--strip</code> categories</summary>

| Category | Removes |
| --- | --- |
| `thumbnails`, `alternates`, `threads` | Page thumbnails, alternate images, and article threads |
| `piece-info`, `web-capture` | Application data and web-capture data |
| `metadata` | XMP metadata. Add `--privacy` to clear the document information too |
| `tags` | Accessibility structure and reading order |
| `output-intents` | Color output intents |
| `links`, `annotations`, `forms` | Those objects, without drawing them into the page |
| `images` | All images, inline images included. Needs qpdf |
| `actions`\* | Actions that PDF/A forbids, form-field actions, and page and document event actions. GoTo, URI, SubmitForm, and JavaScript actions stay |
| `multimedia`\* | Sound, Screen, Movie, 3D, and RichMedia annotations |
| `hidden`\* | Hidden and no-view annotations |
| `xfa`\* | XFA form data. The regular form fields stay |
| `attachments`\* | Embedded files and file-attachment annotations |

\* Needs the Python packages.

</details>

### Fonts and flattening

- `--subset-fonts` removes unused glyphs from embedded fonts with MuPDF.
- `--merge-fonts` combines compatible subsets of the same TrueType font with fontTools and leaves the text as it is.
- `--convert-fonts-cff` rewrites the file with Ghostscript to store fonts in the compact CFF format.
- `--remove-standard-fonts` drops embedded copies of the 14 standard PDF fonts, so viewers use their own.
- `--flatten` draws `forms`, `annotations`, `links`, or `all` into the page. Comments, attached data, and editing can be lost.

Flattening is not redaction. The content underneath stays in the file.

## Profiles

```sh
pdf-squeezer --images --image-quality 70 --save-profile web.profile.json
pdf-squeezer --profile web.profile.json document.pdf
pdf-squeezer --profile web.profile.json --image-quality 85 document.pdf   # flags override the profile
```

A profile is a JSON file with settings and optional metadata. It never holds file paths, passwords, or single-run options such as `-o`, `--recursive`, and `--jobs`. Read a profile from someone else before you use it, because it can turn on destructive options.

<details>
<summary>Bundles and <code>.pdfscp</code> import</summary>

Repeat `--profile` to load several profiles as one list, and pick one with `--profile-entry` and a name or a 1-based number. Without it, the first entry applies. Saving with several profiles loaded writes a bundle that keeps the others. A profile saved with `--compression` takes that level as its name:

```sh
pdf-squeezer --compression light --save-profile light.profile.json
pdf-squeezer --compression strong --save-profile strong.profile.json
pdf-squeezer --profile light.profile.json --profile strong.profile.json \
  --profile-entry strong --save-profile presets.profile.json
pdf-squeezer --profile presets.profile.json --profile-entry 2 document.pdf
```

`--profile` also imports `.pdfscp` property lists, which need only Python 3. The importer maps image, font, removal, metadata, and date settings. It stops on settings it cannot map exactly, such as unknown fields, unverified color conversion, or automatic Producer replacement. The error suggests a native option where one exists.

</details>

## Passwords and encryption

```sh
pdf-squeezer --password-file reader-password.txt protected.pdf
pdf-squeezer --password-file owner-password.txt --decrypt protected.pdf
pdf-squeezer --encrypt-user-file reader-password.txt \
  --encrypt-owner-file owner-password.txt --permissions print document.pdf
```

- Passwords come from one-line files, so they never appear in the process list or in profiles.
- New encryption uses AES-256 and needs an owner password that differs from the user password. `--permissions` takes `all`, `print`, or `none`, and each viewer decides how to enforce them.
- Encrypted input needs qpdf. The output keeps the input's encryption unless you add `--decrypt` or new encryption. On encrypted input, `--privacy` needs one of the two, because the file gets a new identifier.
- The CLI decrypts into a private directory next to the output, so protect that filesystem too.

## PDF/A

```sh
pdf-squeezer --pdfa 4 report.pdf
pdf-squeezer --pdfa 3b invoice.pdf
pdf-squeezer --pdfa 4 --output-intent ISOcoated_v2.icc brochure.pdf
pdf-squeezer --pdfa 4 --font-file 'Verdana=/path/to/verdana.ttf' slides.pdf
pdf-squeezer --pdfa 2b --strip actions,hidden form.pdf
```

`--pdfa` converts the compressed document to PDF/A and writes it only after [veraPDF](https://verapdf.org/) confirms that it complies. It needs the Python packages, qpdf, veraPDF, and fontconfig, which the container image already has. In a batch, one veraPDF run checks up to 32 files.

| Level | PDF version | Attachments |
| --- | --- | --- |
| `2b` | 1.7 | Refused, because PDF/A-2 accepts only attachments that are PDF/A files |
| `3b` | 1.7 | Kept and associated with the document |
| `4` | 2.0 | Kept, and the file becomes PDF/A-4f |

PDF/A-2b and 3b forbid JavaScript and event actions. For those levels, the converter drops custom XMP properties that lack an extension schema.

The converter repairs what it can without changing how pages look. When it cannot, it stops and names the object and the option that fixes it, such as `--strip actions`, `--font-file`, or `--output-intent`. It never flattens, rasterizes, or deletes content to pass validation. If veraPDF still rejects the file, the error lists each failed rule, the objects involved, and an option to try. `-V` prints every repair.

<details>
<summary>What the converter repairs</summary>

- It sets the PDF header, the file identifier, and the XMP identification, and it moves the document information into XMP.
- It removes features that PDF 2.0 deprecates or PDF/A forbids, such as LZW streams, transfer functions, halftones, image alternates, OPI, reference XObjects, and interpolation flags.
- It makes inherited resources explicit, and MuPDF draws missing appearance streams for annotations and form fields.
- Soft masks follow their images through `--dpi` and `--clip-images` when a mask has the size of its image and no matte.
- When the document does not embed Helvetica, Times, Courier, Symbol, ZapfDingbats, or their Arial, Times New Roman, and Courier New aliases, the converter embeds a metric-compatible font. The document's glyph widths must match that font. Otherwise, pass the original with `--font-file NAME=PATH`. The converter sets its glyph advances to the document's widths, so text does not move. A composite font that is not embedded always needs its original TrueType file through `--font-file`.
- It makes embedded fonts consistent in their widths, TrueType cmap subtables, symbolic encoding rules, CIDToGIDMap entries, and ToUnicode maps. Invisible text needs no embedded outline.
- It adds an output intent. The bundled sRGB profile covers gray and RGB content. CMYK content needs a CMYK ICC profile through `--output-intent`, and the converter then adds a DefaultRGB color space for any RGB content.
- It makes optional-content configurations, permissions, and embedded-file specifications consistent.

</details>

PDF/A forbids encryption and requires metadata, color information, and embedded fonts. `--pdfa` therefore rejects output encryption, `--privacy`, `--strip metadata`, `--strip output-intents`, and `--remove-standard-fonts`. Encrypted input needs `--decrypt`.

The converter does not repair non-Identity CMaps, CFF-based CIDFonts without embedded programs, glyphs missing from embedded fonts, or JPEG 2000 streams that break PDF/A rules. It reports them instead.

### Files that already declare PDF/A

Without `--pdfa`, a file that declares PDF/A keeps its level. If veraPDF is installed, the CLI checks the output against that level and refuses to write it when compression broke conformance. If the input did not conform to begin with, or veraPDF is missing, the CLI warns and writes the output unchecked. Options that remove something PDF/A needs, such as `--privacy`, `--metadata`, raster output, or a Ghostscript rewrite, print a warning and skip the check.

## Extraction and raster output

```sh
pdf-squeezer --extract images -o extracted-images document.pdf
pdf-squeezer --extract text -o document.txt document.pdf
pdf-squeezer --bitmap --render-dpi 200 document.pdf
pdf-squeezer --mrc --render-dpi 300 --background-dpi 72 scan.pdf
```

- `--extract images` saves the images of each input in a new `NAME.images` directory. With a single input, `-o` names that directory, which must not exist yet. `--inline-images` adds inline images and needs qpdf.
- `--extract text` writes UTF-8 text with a form feed between pages. Neither mode can be combined with options that change the document.
- `--bitmap` replaces every page with a rendered image. `--mrc` splits a scan into a sharp black-and-white text layer and a low-resolution JPEG background. It suits document scans, not photos.
- Both raster modes discard searchable text, vector graphics, form fields, links, tags, and attachments. They do not redact anything.

## Troubleshooting

| Message | What to do |
| --- | --- |
| `no reduction; copied the original` | Follow the hint in the message, or try a stronger `--compression` level. |
| `output already exists` | Choose another `-o`, delete the old output, or add `--collision number`. |
| `Python 3 is required` or `Missing optional module` | Install the [Python packages](#python-packages) and set `PDF_SQUEEZER_PYTHON`. |
| `qpdf is required` or `Ghostscript not found` | Install the tool. See [Optional tools](#optional-tools). |
| `veraPDF did not confirm PDF/A-4 compliance` | Each failed rule listed below the message names an option to try. |

## Safety notes

- The CLI never changes, moves, or replaces an existing file.
- It builds each output in a private directory next to the destination and hard-links it into place, so a crash cannot leave a half-written file. Filesystems without hard links, such as FAT and exFAT, get a copy instead. If a crash interrupts that copy, delete the partial file.
- The private directory needs free space for a few copies of the document. Where the filesystem allows it, only you can read the outputs and working files.
- If the output directory's path is not valid UTF-8, the Python and Java tools reach the private directory through a symlink in the system temporary directory.
- Ctrl-C, `kill`, or closing the terminal stops the run and its tools, and the CLI deletes its working files.
- Any rewrite invalidates digital signatures, and the CLI warns about signed files. Keep your originals.
- Everything runs locally. Keep the PDF tools patched before you process untrusted files.

## Development

CI runs the Go tests with the race detector and the rendering-comparison tests on Debian 13 and macOS, with every optional tool installed. It also runs `gofmt`, Ruff, and `go vet` and cross-compiles the release binaries. It builds the container image on every run and publishes it for release tags.

`tools/corpus.py` converts every PDF in a directory and counts how many converted, were refused, failed veraPDF, crashed, or timed out:

```sh
python3 tools/corpus.py --pdfa 4 --binary ./pdf-squeezer documents/ results/
```

## License

[MIT](LICENSE) for this repository's code. The optional tools keep their own licenses.
