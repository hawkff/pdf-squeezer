# pdf-squeezer

pdf-squeezer compresses PDF files and converts them to PDF/A.

It's a single binary for Linux and macOS. Basic compression needs no other software, and [optional tools](#optional-tools) add the rest.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Compression levels](#compression-levels)
- [Images](#images)
- [Optional tools](#optional-tools)
- [Output files and batches](#output-files-and-batches)
- [Document changes](#document-changes)
- [Profiles](#profiles)
- [Passwords and encryption](#passwords-and-encryption)
- [PDF/A](#pdfa)
- [Extraction and raster output](#extraction-and-raster-output)
- [Troubleshooting](#troubleshooting)
- [Safety](#safety)
- [License](#license)

## Install

Each release has binaries for Linux and macOS on x86-64 and ARM64.

<details>
<summary>Linux</summary>

```sh
arch=$(uname -m); case "$arch" in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; esac
asset="pdf-squeezer_linux_$arch"
curl -fsSLO "https://github.com/hawkff/pdf-squeezer/releases/latest/download/$asset"
curl -fsSLO https://github.com/hawkff/pdf-squeezer/releases/latest/download/checksums.txt
sha256sum --ignore-missing -c checksums.txt
mkdir -p ~/.local/bin && install -m 755 "$asset" ~/.local/bin/pdf-squeezer
```

If `~/.local/bin` isn't on your `PATH`, add it in your shell profile. The Linux binaries need glibc 2.17 or newer, so they don't run on Alpine. Use the container image there.

</details>

<details>
<summary>macOS</summary>

```sh
arch=$(uname -m); case "$arch" in x86_64) arch=amd64 ;; esac
asset="pdf-squeezer_darwin_$arch"
curl -fsSLO "https://github.com/hawkff/pdf-squeezer/releases/latest/download/$asset"
curl -fsSLO https://github.com/hawkff/pdf-squeezer/releases/latest/download/checksums.txt
shasum -a 256 --ignore-missing -c checksums.txt
mkdir -p ~/.local/bin && install -m 755 "$asset" ~/.local/bin/pdf-squeezer
```

If `~/.local/bin` isn't on your `PATH`, add it in your shell profile.

If you download the binary with a browser instead, macOS blocks it, because the release has no Apple notarization. Remove the quarantine flag:

```sh
xattr -rd com.apple.quarantine ~/.local/bin/pdf-squeezer
```

</details>

<details>
<summary>Container image</summary>

The container image includes every optional tool. With `--user`, the files it writes belong to you:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" ghcr.io/hawkff/pdf-squeezer:latest --pdfa 4 report.pdf
```

</details>

<details>
<summary>From source</summary>

You need Rust 1.92 or newer:

```sh
cargo install --locked --git https://github.com/hawkff/pdf-squeezer
```

</details>

<details>
<summary>With a coding agent</summary>

Paste this prompt into your coding agent:

```text
Install pdf-squeezer from https://github.com/hawkff/pdf-squeezer on this computer.

1. Detect the OS and CPU architecture, and pick the matching release asset:
   pdf-squeezer_linux_amd64, pdf-squeezer_linux_arm64, pdf-squeezer_darwin_arm64, or pdf-squeezer_darwin_amd64.
   On Alpine or another musl-based Linux, stop and tell me to use the container image ghcr.io/hawkff/pdf-squeezer instead.
2. Download the asset and checksums.txt from https://github.com/hawkff/pdf-squeezer/releases/latest/download/
   and check the asset's SHA-256 checksum. Stop if it doesn't match.
3. Install the binary as ~/.local/bin/pdf-squeezer with mode 755, and make sure ~/.local/bin is on my PATH.
4. Run pdf-squeezer --version to confirm that it works.
5. Ask me whether I want the optional tools. If I do, follow https://github.com/hawkff/pdf-squeezer#optional-tools.
   Set up the Python packages from pdf-tools-requirements.txt in the same release and set PDF_SQUEEZER_PYTHON.
   Then install qpdf, Ghostscript, jbig2dec, jbig2enc, fontconfig, the URW base 35 and Liberation fonts, Java,
   and veraPDF 1.30.3 or newer. Show me each command that needs sudo before you run it.
```

</details>

## Quick start

```sh
pdf-squeezer document.pdf                     # writes document.squeezed.pdf next to the input
pdf-squeezer document.pdf -o smaller.pdf      # chooses the output file
pdf-squeezer --compression balanced scan.pdf  # recompresses images with the built-in encoder
pdf-squeezer --compression medium scan.pdf    # also downsamples and crops images; needs the optional tools
pdf-squeezer --recursive documents/ -o out/   # every PDF in a directory tree
pdf-squeezer --privacy document.pdf           # removes identifying metadata
pdf-squeezer --pdfa 4 report.pdf              # converts to PDF/A-4 and checks the result with veraPDF
```

pdf-squeezer prints the old and new size of each file. If a file doesn't get smaller, you get a copy of the original and a hint about what to try. Flags can go before or after file names. `-V` shows progress, and `pdf-squeezer --help` lists every flag.

## Compression levels

Without `--compression`, pdf-squeezer rewrites the file's structure and leaves images alone. It removes objects that nothing uses, merges identical streams, and packs the remaining objects into compressed object streams.

`--compression` adds image settings in five levels. Each level compares JPEG with lossless Flate for every image and keeps the smaller one.

| Level | JPEG quality | 16-bit images to 8-bit | Color and gray DPI limit | Extra work |
| --- | ---: | --- | ---: | --- |
| `light` | 90 | No | None | |
| `balanced` | 85 | Yes | None | |
| `medium` | 65 | Yes | 150 | Crops unused image margins and includes inline images |
| `strong` | 45 | Yes | 120 | Same as `medium` |
| `heavy` | 30 | Yes | 96 | Same as `medium`, plus font merging and maximum-effort Flate |

Levels change how pdf-squeezer stores images. From `medium` up, they also cap image resolution and crop unused margins, and `heavy` merges subsets of the same font. Text, vector graphics, forms, and annotations stay as they are, and color pages stay in color. `medium` and higher need the [Python packages](#python-packages) and qpdf.

`heavy` can leave JPEG artifacts around small text and colored edges, so check fine print before you share the result.

The DPI limit is a ceiling, and images below it keep their resolution. Flags override profile values, and profile values override the level, so `--compression strong --dpi 0` keeps every image's resolution. `--lossless` refuses the levels that lower precision or resolution.

### Ghostscript engine

```sh
pdf-squeezer --engine ghostscript --quality ebook scan.pdf
pdf-squeezer --engine ghostscript --dpi 100 --gray scan.pdf
```

`--engine ghostscript` has Ghostscript rewrite the whole file instead of the default `native` engine. The `--quality` presets are `screen`, `ebook`, `printer`, and `prepress`, and `ebook` is the default. Each preset changes more than image resolution. If Ghostscript skips an image it can't decode, pdf-squeezer rejects the output, because that page would come out blank. Profiles that name the older `pdfcpu` engine still load.

## Images

`--images` recompresses images with the built-in encoder and works without `--compression`. For each image, pdf-squeezer tries lossless Flate and JPEG and keeps the smaller result when it saves at least 2% and 1 KiB.

```sh
pdf-squeezer --images --image-quality 75 scan.pdf     # JPEG quality from 1 to 100; 75 is the default
pdf-squeezer --images --image-codecs flate scan.pdf   # Flate only, never JPEG
pdf-squeezer --lossless document.pdf                  # recompresses images without loss
```

- `--lossless` turns on `--images` and forbids JPEG, color-space changes, downsampling, and bit-depth reduction.
- RGB images whose pixels are all gray become grayscale, and grayscale images with only black and white pixels become 1-bit. `--color-reduction preserve` keeps them as they are.
- 16-bit images stay 16-bit unless you add `--reduce-bit-depth`. Soft masks keep their precision.
- `--gray` converts every page color to grayscale.
- `--force-recompression` keeps the new encoding even when it saves nothing.
- `--extended-images` adds JPEG 2000 and CMYK JPEG images.
- pdf-squeezer leaves an image unchanged when the image has a matte, an explicit mask, indexed colors, an unsupported transfer function, or an unknown filter.
- `--image-memory` caps the memory for image buffers, in MiB, and defaults to 512. Images that need more stay unchanged. `-V` prints each image that pdf-squeezer kept and the reason.

### Downsampling and scan codecs

These options need the Python packages.

```sh
pdf-squeezer --dpi 120 --gray-dpi 150 scan.pdf                    # downsamples to the size the page shows
pdf-squeezer --images --clip-images --inline-images document.pdf  # crops unused margins, includes inline images
pdf-squeezer --images --mono-codecs flate,ccitt,jbig2 scan.pdf    # keeps the smallest lossless black-and-white codec
```

- `--dpi` limits color and gray images, and `--gray-dpi` overrides it for gray ones. `--mono-dpi` sets a limit for black-and-white images, which otherwise keep their resolution.
- pdf-squeezer measures each image at the size the page draws it. It downsamples an image when its resolution exceeds the limit times `--dpi-threshold`, which defaults to 1.5. A shared image keeps enough pixels for its largest use.
- Cropping applies to axis-aligned images. Images in masks, patterns, annotation appearances, or forms that pdf-squeezer can't analyze stay whole.
- CCITT Group 4 and JBIG2 encoding is lossless. pdf-squeezer never turns on the lossy symbol matching in JBIG2.

Downsampling loses detail, so check fine text before you share a scan.

## Optional tools

pdf-squeezer installs nothing on its own. If an option needs a tool you don't have, that file fails with an error that names the tool. pdf-squeezer never switches to another method instead.

| Tool | Needed for |
| --- | --- |
| [Python packages](#python-packages): [pikepdf](https://github.com/pikepdf/pikepdf), [PyMuPDF](https://github.com/pymupdf/PyMuPDF), [Pillow](https://github.com/python-pillow/Pillow), and [fontTools](https://github.com/fonttools/fonttools) | `--compression medium` and higher, `--clip-images`, `--extended-images`, `--mono-codecs`, `--flatten`, `--subset-fonts`, `--merge-fonts`, `--bitmap`, `--mrc`, `--extract text`, five `--strip` categories, `--pdfa`, and, with the default engine, DPI limits and `--gray` |
| [qpdf](https://github.com/qpdf/qpdf) | Encrypted input, `--inline-images`, `--strip images`, `--compression medium` and higher, and `--pdfa` |
| `jbig2` from [jbig2enc](https://github.com/agl/jbig2enc) | `--mono-codecs jbig2` |
| [jbig2dec](https://github.com/ArtifexSoftware/jbig2dec) | Reading JBIG2 images already in a PDF |
| [Ghostscript](https://www.ghostscript.com/) | `--engine ghostscript` and `--convert-fonts-cff` |
| [veraPDF](https://verapdf.org/) 1.30.3 or newer, with [Java](https://adoptium.net/) | `--pdfa`, and checking files that declare PDF/A |
| [fontconfig](https://www.freedesktop.org/wiki/Software/fontconfig/) with [URW base 35](https://github.com/ArtifexSoftware/urw-base35-fonts) or [Liberation](https://github.com/liberationfonts/liberation-fonts) fonts | `--pdfa` on files that don't embed their standard fonts |
| [Python 3](https://www.python.org/) | Importing `.pdfscp` profiles |

Everything else runs without extra software, including `--images`, `--compression light` and `balanced`, the metadata and privacy options, output encryption, and image extraction.

### Python packages

Download the package list for your release. A clone of this repository has the same list in `tools/requirements.txt`.

```sh
curl -fsSLO https://github.com/hawkff/pdf-squeezer/releases/latest/download/pdf-tools-requirements.txt
```

Install the packages in their own environment and point pdf-squeezer at its Python:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r pdf-tools-requirements.txt
export PDF_SQUEEZER_PYTHON="$PWD/.venv/bin/python"
```

<details>
<summary>With uv</summary>

[uv](https://docs.astral.sh/uv/) downloads Python 3.13 if your system lacks it:

```sh
uv venv --python 3.13 .venv
uv pip install --python .venv/bin/python -r pdf-tools-requirements.txt
export PDF_SQUEEZER_PYTHON="$PWD/.venv/bin/python"
```

</details>

<details>
<summary>With Miniconda</summary>

```sh
conda create -y -n pdf-tools python=3.13
conda activate pdf-tools
python -m pip install -r pdf-tools-requirements.txt
export PDF_SQUEEZER_PYTHON="$(command -v python)"
```

See the [Miniconda documentation](https://www.anaconda.com/docs/getting-started/miniconda/main) to install `conda`.

</details>

Add the `export` line to your shell profile to keep the setting. The binary carries its own Python scripts, so you install only the packages. Without `PDF_SQUEEZER_PYTHON`, pdf-squeezer uses `python3` from your `PATH`. It runs Python in isolated mode, which ignores `PYTHONPATH` and packages from `pip install --user`.

### Other tools

Debian and Ubuntu package everything except veraPDF and jbig2enc's `jbig2`:

```sh
sudo apt install \
  qpdf \
  ghostscript \
  jbig2dec \
  default-jre-headless \
  fontconfig \
  fonts-urw-base35 \
  fonts-liberation \
  python3-venv
```

On macOS, these Homebrew commands install everything except veraPDF and the Java runtime it needs. On Intel Macs, pip builds pikepdf and PyMuPDF from source, which needs the Xcode command line tools.

```sh
brew install \
  qpdf \
  ghostscript \
  jbig2dec \
  jbig2enc \
  fontconfig
brew install --cask \
  font-urw-base35 \
  font-liberation
```

Use [veraPDF](https://verapdf.org/) 1.30.3 or newer, because older releases leave a log file in `/tmp` on every run. From a clone of this repository, `sh tools/install-verapdf.sh "$HOME/.local/verapdf"` installs a checksummed 1.30.3. Add that directory to your `PATH` afterwards. The script needs Java, `curl`, and `unzip`.

Ghostscript and PyMuPDF/MuPDF use AGPL or commercial licenses, and veraPDF uses GPL and MPL. The release binaries include none of them. The binary embeds the ICC's sRGB profile `sRGB2014.icc`, which the ICC lets anyone redistribute.

## Output files and batches

- Without `-o`, pdf-squeezer writes each output next to its input as `NAME.squeezed.pdf`.
- `-o` takes a file name for one input, or an existing directory for any number of inputs. Outputs in that directory keep the subdirectories of their inputs.
- To give each input its own destination, repeat `-o` once per input file, in order: `pdf-squeezer a.pdf b.pdf -o out/a.pdf -o elsewhere/b.pdf`. This form refuses directory inputs.
- pdf-squeezer checks every destination before it starts and never overwrites an existing file or symlink. With `--collision number`, it writes `NAME.squeezed.1.pdf` and so on instead of stopping.
- A directory input covers its top-level `.pdf` files, and `--recursive` adds subdirectories. The scan skips hidden files and directories, symlinks inside the tree, and earlier `.squeezed.pdf` outputs.
- `--jobs N` processes up to N files at once, from 1 to 64, and keeps reports and outputs in input order. Each job gets its own `--image-memory` budget. With more than one job, every line a file prints on stderr starts with its path.
- A batch reports each failed file, continues with the rest, and exits with a nonzero status if any file failed.

If compression doesn't make a file smaller, the output is a copy of the original. Options that change the document on purpose keep their result even when it grows. Those are `--privacy`, `--strip`, `--flatten`, `--gray`, `--pdfa`, and `--force-recompression`.

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

`--metadata KEY=VALUE` sets `Title`, `Author`, `Subject`, `Keywords`, `Creator`, `Producer`, `CreationDate`, or `ModDate`. Dates use the form `D:YYYYMMDDhhmmssZ`. Repeat the flag for more keys, and leave the value empty to remove a key. The option also removes XMP metadata, which could contradict the new values, and you can't combine it with `--privacy`.

`--timestamps preserve`, the default, keeps the document dates and the input file's modification time. `modified` sets the document's modification date to now, and `now` sets both document dates. Both of those also remove XMP metadata.

### Removing content

`--strip` removes the categories you list, separated by commas. Removing tags or output intents saves a few bytes and costs accessibility and color accuracy.

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
- `--convert-fonts-cff` has Ghostscript rewrite the file to store fonts in the compact CFF format.
- `--remove-standard-fonts` drops embedded copies of the 14 standard PDF fonts, so viewers use their own.
- `--flatten` draws `forms`, `annotations`, `links`, or `all` into the page. You can lose comments, attached data, and the ability to edit.

Flattening doesn't redact anything. The content underneath stays in the file.

## Profiles

```sh
pdf-squeezer --images --image-quality 70 --save-profile web.profile.json
pdf-squeezer --profile web.profile.json document.pdf
pdf-squeezer --profile web.profile.json --image-quality 85 document.pdf   # flags override the profile
```

A profile is a JSON file with settings and optional metadata. It never holds file paths, passwords, or single-run options such as `-o`, `--recursive`, and `--jobs`. Read a profile from someone else before you use it, because it can turn on destructive options.

<details>
<summary>Bundles and <code>.pdfscp</code> import</summary>

Repeat `--profile` to load several profiles as one list, and pick one with `--profile-entry` and a name or a 1-based number. Without `--profile-entry`, the first entry applies. If you save with several profiles loaded, you get a bundle that keeps the others. A profile saved with `--compression` takes the level as its name:

```sh
pdf-squeezer --compression light --save-profile light.profile.json
pdf-squeezer --compression strong --save-profile strong.profile.json
pdf-squeezer --profile light.profile.json --profile strong.profile.json \
  --profile-entry strong --save-profile presets.profile.json
pdf-squeezer --profile presets.profile.json --profile-entry 2 document.pdf
```

`--profile` also imports `.pdfscp` property lists, which need only Python 3. The importer maps image, font, removal, metadata, and date settings. It stops on settings that have no exact equivalent, such as unknown fields, unverified color conversion, or automatic Producer replacement. The error suggests a native option where one exists.

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
- pdf-squeezer decrypts into a private directory next to the output, so protect that filesystem too.

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

The converter repairs what it can without changing how pages look. If it can't, it stops and names the object and the option that fixes it, such as `--strip actions`, `--font-file`, or `--output-intent`. It never flattens, rasterizes, or deletes content to pass validation. If veraPDF still rejects the file, the error lists each failed rule, the objects involved, and an option to try. `-V` prints every repair.

<details>
<summary>What the converter repairs</summary>

- It sets the PDF header, the file identifier, and the XMP identification, and it moves the document information into XMP.
- It removes features that PDF 2.0 deprecates or PDF/A forbids, such as LZW streams, transfer functions, halftones, image alternates, OPI, reference XObjects, and interpolation flags.
- It makes inherited resources explicit, and MuPDF draws missing appearance streams for annotations and form fields.
- Soft masks follow their images through `--dpi` and `--clip-images` when a mask has the size of its image and no matte.
- If the document doesn't embed Helvetica, Times, Courier, Symbol, ZapfDingbats, or their Arial, Times New Roman, and Courier New aliases, the converter embeds a metric-compatible font. The document's glyph widths must match that font. Otherwise, pass the original with `--font-file NAME=PATH`. The converter sets its glyph advances to the document's widths, so text doesn't move. A composite font that the document doesn't embed needs its original TrueType file through `--font-file`.
- It makes embedded fonts consistent in their widths, TrueType cmap subtables, symbolic encoding rules, CIDToGIDMap entries, and ToUnicode maps. Invisible text needs no embedded outline.
- It adds an output intent. The bundled sRGB profile covers gray and RGB content. CMYK content needs a CMYK ICC profile through `--output-intent`, and the converter then adds a DefaultRGB color space for any RGB content.
- It makes optional-content configurations, permissions, and embedded-file specifications consistent.

</details>

PDF/A forbids encryption and requires metadata, color information, and embedded fonts. `--pdfa` therefore rejects output encryption, `--privacy`, `--strip metadata`, `--strip output-intents`, and `--remove-standard-fonts`. Encrypted input needs `--decrypt`.

The converter doesn't repair non-Identity CMaps, CFF-based CIDFonts without embedded programs, glyphs missing from embedded fonts, or JPEG 2000 streams that break PDF/A rules. It reports them instead.

### Files that already declare PDF/A

Without `--pdfa`, a file that declares PDF/A keeps its level. If veraPDF is on your `PATH`, pdf-squeezer checks the output against that level and refuses to write it when compression broke conformance. If the input didn't conform in the first place, or veraPDF is missing, pdf-squeezer warns and writes the output unchecked. Options that remove something PDF/A needs, such as `--privacy`, `--metadata`, raster output, or a Ghostscript rewrite, print a warning and skip the check.

## Extraction and raster output

```sh
pdf-squeezer --extract images -o extracted-images document.pdf
pdf-squeezer --extract text -o document.txt document.pdf
pdf-squeezer --bitmap --render-dpi 200 document.pdf
pdf-squeezer --mrc --render-dpi 300 --background-dpi 72 scan.pdf
```

- `--extract images` saves the images of each input in a new `NAME.images` directory. With a single input, `-o` names that directory, which must not exist yet. `--inline-images` adds inline images and needs qpdf.
- `--extract text` writes UTF-8 text with a form feed between pages. You can't combine either mode with options that change the document.
- `--bitmap` replaces every page with a rendered image. `--mrc` splits a scan into a sharp black-and-white text layer and a low-resolution JPEG background, which suits document scans and not photos.
- Both raster modes discard searchable text, vector graphics, form fields, links, tags, and attachments. They don't redact anything.

## Troubleshooting

| Message | What to do |
| --- | --- |
| `no reduction; copied the original` | Follow the hint in the message, or try a stronger `--compression` level. |
| `output already exists` | Choose another `-o`, delete the old output, or add `--collision number`. |
| `the password is incorrect` | Put the password that opens the file in a one-line file and pass it with `--password-file`. |
| `Python 3 is required` or `Missing optional module` | Install the [Python packages](#python-packages) and set `PDF_SQUEEZER_PYTHON`. |
| `qpdf is required` or `Ghostscript not found` | Install the tool. See [Optional tools](#optional-tools). |
| `object N 0 is damaged and cannot be read` | Get an undamaged copy of the file. pdf-squeezer couldn't read or repair an object the document uses, and it stops instead of writing a file without that object. |
| `veraPDF did not confirm PDF/A-4 compliance` | Each failed rule listed below the message names an option to try. |

## Safety

- pdf-squeezer never changes, moves, or replaces an existing file.
- It builds each output in a private directory next to the destination and hard-links the finished file into place, so a crash can't leave a half-written file. Filesystems without hard links, such as FAT and exFAT, get a copy instead. If a crash interrupts that copy, delete the partial file.
- The private directory needs free space for a few copies of the document. On filesystems with permissions, only you can read the outputs and working files.
- If the output directory's path isn't valid UTF-8, the Python and Java tools reach the private directory through a symlink in the system temporary directory.
- Ctrl-C, `kill`, or closing the terminal stops the run and its tools, and pdf-squeezer deletes its working files.
- pdf-squeezer repairs damaged cross-reference tables and skips stray tokens where a dictionary key belongs, as PDF readers do. If it still can't read an object the document uses, it stops with an error.
- Any rewrite invalidates digital signatures, and pdf-squeezer warns about signed files. Keep your originals.
- Everything runs on your machine. Keep the PDF tools patched before you process untrusted files.

## License

[MIT](LICENSE) for this repository's code. The optional tools keep their own licenses.
