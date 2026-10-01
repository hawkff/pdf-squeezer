# pdf-squeezer

A PDF compressor written in Go.

## Install

Download a binary for Linux, macOS, or Windows from the [releases page](https://github.com/hawkff/pdf-squeezer/releases), or build from source with Go 1.26 or newer:

```sh
go install github.com/hawkff/pdf-squeezer@latest
```

The default engine, [pdfcpu](https://pdfcpu.io), needs no other software. For the Ghostscript engine, install a current, security-patched version and put `gs` on your `PATH`. On Windows, the CLI also looks for `gswin64c` and `gswin32c`.

```sh
# macOS
brew install ghostscript

# Debian / Ubuntu
sudo apt-get install ghostscript
```

## Use

```sh
# Lossless optimization, writes document.squeezed.pdf next to the input
pdf-squeezer document.pdf

# Choose an output path
pdf-squeezer -o smaller.pdf document.pdf

# Write document.squeezed.pdf into the current directory
pdf-squeezer document.pdf -o .

# Reduce image quality with Ghostscript
pdf-squeezer --engine ghostscript --quality ebook -o smaller.pdf scan.pdf

# Report each step on stderr, including Ghostscript's own progress
pdf-squeezer -V document.pdf

pdf-squeezer --version
pdf-squeezer --help
```

Short flags: `-o` output file or directory, `-V` verbose, `-v` version, `-h` help. Flags may come before or after the filename. Use `--` before a filename that starts with a dash.

`--engine pdfcpu` is the default. It removes redundant PDF objects and compresses document structure without downsampling images. Already optimized PDFs may not shrink.

`--engine ghostscript` rewrites the PDF and can downsample images. Choose `--quality screen` for low-resolution output, `ebook` for medium resolution, or `printer` / `prepress` for print-oriented output. The default is `ebook`. These presets change more than resolution and can reduce quality. `--quality` only applies to Ghostscript.

The CLI prints the input and output sizes. If compression would not make the PDF smaller, it copies the original bytes to the output instead.

## File safety and limitations

- The CLI leaves the input unchanged and refuses to overwrite an existing output, including a symlink.
- The output directory must already exist. The CLI stages compression in a temporary file there, then creates the destination with exclusive access. It removes partial output after a reported write failure. A crash or power loss during the final copy can leave an incomplete destination. Remove that file before retrying.
- Keep enough free disk space for the temporary PDF and the final copy. New files use owner-only permissions where the filesystem supports them.
- Compression can invalidate digital signatures. Ghostscript can also change or discard forms, annotations, accessibility tags, attachments, and encryption. Keep originals and inspect the result before sharing it. Use another tool for signed or password-protected documents.
- Both engines process files on your machine. The CLI does not upload PDFs. Keep Ghostscript updated before processing untrusted files.

## License

[MIT](LICENSE).
