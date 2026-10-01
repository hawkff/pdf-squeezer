package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	stdlog "log"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"runtime/debug"
	"strings"
	"syscall"
	"time"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/log"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

const usage = `Usage: pdf-squeezer [flags] input.pdf

Optimize with pdfcpu, or choose Ghostscript for lossy image compression.
The input stays unchanged. Existing output files are never overwritten.
Without -o, the result is input.squeezed.pdf next to the input. The output
directory must already exist; it also holds a temporary file during
compression.

Flags:
  --dpi N            Ghostscript: resample color and gray images above N dpi
  --engine NAME      pdfcpu (default) or ghostscript
  --gray             Ghostscript: convert all colors to grayscale
  -h, --help         print this help and exit
  --images           pdfcpu: re-encode images (see README for what changes)
  -o, --output PATH  output file, or a directory for input.squeezed.pdf
  --privacy          drop document info, XMP metadata, and piece info
  --quality PRESET   Ghostscript preset: screen, ebook (default), printer, prepress
  -V, --verbose      report each step on stderr
  -v, --version      print the version and exit
`

type options struct {
	engine, quality                string
	dpi                            int
	gray, images, privacy, verbose bool
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	err := run(ctx, os.Args[1:], os.Stdout, os.Stderr)
	stop()
	if err != nil && !errors.Is(err, flag.ErrHelp) {
		fmt.Fprintln(os.Stderr, "pdf-squeezer:", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, args []string, stdout, stderr io.Writer) error {
	flags := flag.NewFlagSet("pdf-squeezer", flag.ContinueOnError)
	flags.SetOutput(stderr)
	// Help text lives in usage: PrintDefaults would list short and long forms separately.
	var opts options
	flags.StringVar(&opts.engine, "engine", "pdfcpu", "")
	flags.StringVar(&opts.quality, "quality", "", "")
	flags.IntVar(&opts.dpi, "dpi", 0, "")
	flags.BoolVar(&opts.gray, "gray", false, "")
	flags.BoolVar(&opts.images, "images", false, "")
	var output string
	flags.StringVar(&output, "output", "", "")
	flags.StringVar(&output, "o", "", "")
	var showVersion bool
	flags.BoolVar(&opts.privacy, "privacy", false, "")
	flags.BoolVar(&opts.verbose, "verbose", false, "")
	flags.BoolVar(&opts.verbose, "V", false, "")
	flags.BoolVar(&showVersion, "version", false, "")
	flags.BoolVar(&showVersion, "v", false, "")
	flags.Usage = func() { fmt.Fprint(stderr, usage) }
	// Accept flags after the filename: Parse stops at the first positional argument.
	var inputs []string
	for {
		if err := flags.Parse(args); err != nil {
			return err
		}
		rest := flags.Args()
		consumed := len(args) - len(rest)
		if len(rest) == 0 || (consumed > 0 && args[consumed-1] == "--") {
			inputs = append(inputs, rest...)
			break
		}
		inputs = append(inputs, rest[0])
		args = rest[1:]
	}
	if showVersion {
		v := "unknown"
		if info, ok := debug.ReadBuildInfo(); ok {
			v = info.Main.Version
		}
		fmt.Fprintln(stdout, v)
		return nil
	}
	if len(inputs) != 1 {
		flags.Usage()
		return errors.New("provide exactly one input PDF")
	}
	if opts.dpi < 0 {
		return errors.New("--dpi must be positive")
	}
	switch opts.engine {
	case "pdfcpu":
		switch {
		case opts.quality != "":
			return errors.New("--quality requires --engine ghostscript")
		case opts.dpi != 0:
			return errors.New("--dpi requires --engine ghostscript")
		case opts.gray:
			return errors.New("--gray requires --engine ghostscript")
		}
	case "ghostscript":
		if opts.images {
			return errors.New("--images requires --engine pdfcpu")
		}
		if opts.quality == "" {
			opts.quality = "ebook"
		}
		switch opts.quality {
		case "screen", "ebook", "printer", "prepress":
		default:
			return fmt.Errorf("unknown quality %q: use screen, ebook, printer, or prepress", opts.quality)
		}
	default:
		return fmt.Errorf("unknown engine %q: use pdfcpu or ghostscript", opts.engine)
	}
	input := inputs[0]
	name := strings.TrimSuffix(filepath.Base(input), filepath.Ext(input)) + ".squeezed.pdf"
	if output == "" {
		output = filepath.Join(filepath.Dir(input), name)
	} else if info, err := os.Stat(output); err == nil && info.IsDir() {
		output = filepath.Join(output, name)
	}
	before, after, err := squeeze(ctx, input, output, opts, stderr)
	if err != nil {
		return err
	}
	switch {
	case before == after && !opts.privacy:
		hint := "try --engine ghostscript"
		if opts.engine == "ghostscript" {
			hint = "try --quality screen or a lower --dpi"
		} else if !opts.images {
			hint = "try --images or --engine ghostscript"
		}
		fmt.Fprintf(stdout, "%s: no reduction; copied the original (%d bytes); %s\n", output, before, hint)
	case after > before:
		fmt.Fprintf(stdout, "%s: %d -> %d bytes (%.1f%% larger, %s)\n",
			output, before, after, 100*(float64(after)/float64(before)-1), opts.engine)
	default:
		fmt.Fprintf(stdout, "%s: %d -> %d bytes (%.1f%% smaller, %s)\n",
			output, before, after, 100*(1-float64(after)/float64(before)), opts.engine)
	}
	return nil
}

func squeeze(ctx context.Context, input, output string, opts options, stderr io.Writer) (int64, int64, error) {
	logf := func(format string, args ...any) {
		if opts.verbose {
			fmt.Fprintf(stderr, format+"\n", args...)
		}
	}
	if err := ctx.Err(); err != nil {
		return 0, 0, err
	}
	info, err := os.Stat(input)
	if err != nil {
		return 0, 0, fmt.Errorf("input: %w", err)
	}
	if !info.Mode().IsRegular() {
		return 0, 0, errors.New("input must be a regular PDF file")
	}
	logf("input: %s (%d bytes)", input, info.Size())
	if _, err := os.Lstat(output); err == nil {
		return 0, 0, fmt.Errorf("output already exists: %s", output)
	} else if !errors.Is(err, os.ErrNotExist) {
		return 0, 0, fmt.Errorf("output: %w", err)
	}
	in, err := os.Open(input)
	if err != nil {
		return 0, 0, err
	}
	defer in.Close()
	if err := checkPDF(in); err != nil {
		return 0, 0, fmt.Errorf("input: %w", err)
	}
	// Stage beside the destination so a missing or unwritable directory fails before compression.
	tmp, err := os.CreateTemp(filepath.Dir(output), ".pdf-squeezer-*.pdf")
	if err != nil {
		return 0, 0, err
	}
	defer func() {
		tmp.Close()
		os.Remove(tmp.Name())
	}()
	logf("staging in %s", tmp.Name())

	conf := model.NewStatelessConfiguration()
	conf.Offline = true
	conf.PreserveInfoDict = true
	// api.Optimize always optimizes; this only stops api.Validate from re-optimizing the output.
	conf.Optimize = false
	if opts.verbose {
		// Surface pdfcpu's repair and skipped-object notices.
		log.SetCLILogger(stdlog.New(stderr, "", 0))
		defer log.DisableLoggers()
	}
	start := time.Now()
	switch opts.engine {
	case "pdfcpu":
		err = optimize(ctx, in, tmp, conf, opts, logf)
	case "ghostscript":
		if err = ghostscript(ctx, in, tmp, opts, stderr); err == nil && opts.privacy {
			opts.images = false
			err = optimize(ctx, tmp, tmp, conf, opts, logf)
		}
	default:
		err = fmt.Errorf("unknown engine %q", opts.engine)
	}
	if err != nil {
		return 0, 0, fmt.Errorf("%s: %w", opts.engine, err)
	}
	compressed, err := tmp.Stat()
	if err != nil {
		return 0, 0, err
	}
	logf("%s: %d bytes in %s", opts.engine, compressed.Size(), time.Since(start).Round(time.Millisecond))
	if _, err := tmp.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
	start = time.Now()
	if err := api.Validate(ctx, tmp, conf, nil); err != nil {
		return 0, 0, fmt.Errorf("%s output: %w", opts.engine, err)
	}
	logf("validated in %s", time.Since(start).Round(time.Millisecond))
	source := tmp
	if compressed.Size() >= info.Size() && !opts.privacy {
		source = in
	}
	if _, err := source.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
	logf("writing %s", output)
	written, err := writeNew(ctx, output, source)
	return info.Size(), written, err
}

// optimize is api.Optimize with optional image re-encoding and metadata stripping before writing.
// pdfcpu holds the whole document in memory, so in and out may be the same file.
func optimize(ctx context.Context, in io.ReadSeeker, out *os.File, conf *model.Configuration, opts options, logf func(string, ...any)) error {
	conf.Cmd = model.OPTIMIZE
	pdf, err := api.ReadValidateAndOptimize(ctx, in, conf, nil)
	if err != nil {
		return err
	}
	if opts.images {
		stats, err := optimizeImages(pdf)
		if err != nil {
			return err
		}
		logf("%s", stats)
	}
	if opts.privacy {
		if err := stripMetadata(pdf); err != nil {
			return err
		}
	}
	if err := out.Truncate(0); err != nil {
		return err
	}
	if _, err := out.Seek(0, io.SeekStart); err != nil {
		return err
	}
	return api.WriteContext(ctx, pdf, out)
}

// stripMetadata empties the document information dictionary and drops XMP metadata and
// application piece info from every object. Unreferenced objects are not written.
func stripMetadata(pdf *model.Context) error {
	// pdfcpu fills a missing Info dict with its own producer and dates; an empty one stays empty.
	info, err := pdf.IndRefForNewObject(types.NewDict())
	if err != nil {
		return err
	}
	pdf.Info = info
	pdf.ID = nil
	for _, entry := range pdf.Table {
		if entry == nil || entry.Free {
			continue
		}
		var d types.Dict
		switch o := entry.Object.(type) {
		case types.Dict:
			d = o
		case types.StreamDict:
			d = o.Dict
		default:
			continue
		}
		for _, key := range []string{"Metadata", "PieceInfo", "LastModified", "SpiderInfo"} {
			delete(d, key)
		}
	}
	return nil
}

func checkPDF(file *os.File) error {
	var header [5]byte
	if _, err := file.ReadAt(header[:], 0); err != nil {
		return fmt.Errorf("read PDF header: %w", err)
	}
	if string(header[:]) != "%PDF-" {
		return errors.New("missing %PDF- header")
	}
	return nil
}

func ghostscript(ctx context.Context, input, output *os.File, opts options, stderr io.Writer) error {
	names := []string{"gs"}
	if runtime.GOOS == "windows" {
		names = []string{"gswin64c", "gswin32c", "gs"}
	}
	var executable string
	for _, name := range names {
		if path, err := exec.LookPath(name); err == nil {
			executable = path
			break
		}
	}
	if executable == "" {
		return errors.New("Ghostscript not found in PATH; install it or use --engine pdfcpu")
	}
	// The presets convert colors to sRGB, which drops 16-bit ICC images with soft masks (iOS Photos exports).
	colors := "-sColorConversionStrategy=LeaveColorUnchanged"
	if opts.gray {
		colors = "-sColorConversionStrategy=Gray"
	}
	args := []string{"-dSAFER", "-dBATCH", "-dNOPAUSE", "-dPDFSTOPONERROR",
		"-sDEVICE=pdfwrite", "-dPDFSETTINGS=/" + opts.quality, colors}
	if opts.dpi > 0 {
		// Monochrome scans keep the preset's resolution: downsampling them costs legibility.
		args = append(args, fmt.Sprintf("-dColorImageResolution=%d", opts.dpi), fmt.Sprintf("-dGrayImageResolution=%d", opts.dpi))
	}
	cmd := exec.CommandContext(ctx, executable, append(args, "-sOutputFile=-", "-sstdout=%stderr", "-f", "-")...)
	// Stream file contents, not user paths, to avoid Ghostscript filename syntax.
	cmd.Stdin, cmd.Stdout = input, output
	// Not -q: it would also hide the warning summary checked below.
	var messages bytes.Buffer
	cmd.Stderr = &messages
	if opts.verbose {
		cmd.Stderr = io.MultiWriter(stderr, &messages)
		fmt.Fprintln(stderr, "running", strings.Join(cmd.Args, " "))
	}
	cmd.Env = append(os.Environ(), "GS_OPTIONS=")
	if err := cmd.Run(); err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if !opts.verbose {
			stderr.Write(messages.Bytes())
		}
		return err
	}
	// Ghostscript 10 skips an image it cannot decode, leaves the page blank, and exits 0.
	if bytes.Contains(messages.Bytes(), []byte("recoverable image error")) {
		return errors.New("Ghostscript skipped an image it could not decode, which leaves a blank page; try --engine pdfcpu")
	}
	return nil
}

// ponytail: a crash during the exclusive copy can leave partial output; use a no-replace rename for atomic publication.
func writeNew(ctx context.Context, path string, source io.Reader) (written int64, err error) {
	if err := ctx.Err(); err != nil {
		return 0, err
	}
	out, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return 0, fmt.Errorf("create output: %w", err)
	}
	defer func() {
		err = errors.Join(err, out.Close())
		if err != nil {
			err = errors.Join(err, os.Remove(path))
		}
	}()
	if written, err = io.Copy(out, source); err != nil {
		return written, err
	}
	if err = ctx.Err(); err != nil {
		return written, err
	}
	return written, out.Sync()
}
