package main

import (
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
)

const usage = `Usage: pdf-squeezer [flags] input.pdf

Optimize with pdfcpu, or choose Ghostscript for lossy image compression.
The input stays unchanged. Existing output files are never overwritten.
Without -o, the result is input.squeezed.pdf next to the input. The output
directory must already exist; it also holds a temporary file during
compression.

Flags:
  --engine NAME      pdfcpu (default) or ghostscript
  -h, --help         print this help and exit
  -o, --output PATH  output file, or a directory for input.squeezed.pdf
  --quality PRESET   Ghostscript preset: screen, ebook (default), printer, prepress
  -V, --verbose      report each step on stderr
  -v, --version      print the version and exit
`

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
	engine := flags.String("engine", "pdfcpu", "")
	quality := flags.String("quality", "", "")
	var output string
	flags.StringVar(&output, "output", "", "")
	flags.StringVar(&output, "o", "", "")
	var verbose, showVersion bool
	flags.BoolVar(&verbose, "verbose", false, "")
	flags.BoolVar(&verbose, "V", false, "")
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
	switch *engine {
	case "pdfcpu":
		if *quality != "" {
			return errors.New("--quality requires --engine ghostscript")
		}
	case "ghostscript":
		if *quality == "" {
			*quality = "ebook"
		}
		switch *quality {
		case "screen", "ebook", "printer", "prepress":
		default:
			return fmt.Errorf("unknown quality %q: use screen, ebook, printer, or prepress", *quality)
		}
	default:
		return fmt.Errorf("unknown engine %q: use pdfcpu or ghostscript", *engine)
	}
	input := inputs[0]
	name := strings.TrimSuffix(filepath.Base(input), filepath.Ext(input)) + ".squeezed.pdf"
	if output == "" {
		output = filepath.Join(filepath.Dir(input), name)
	} else if info, err := os.Stat(output); err == nil && info.IsDir() {
		output = filepath.Join(output, name)
	}
	before, after, err := squeeze(ctx, input, output, *engine, *quality, stderr, verbose)
	if err != nil {
		return err
	}
	if before == after {
		fmt.Fprintf(stdout, "%s: no reduction; copied the original (%d bytes)\n", output, before)
	} else {
		fmt.Fprintf(stdout, "%s: %d -> %d bytes (%.1f%% smaller, %s)\n",
			output, before, after, 100*(1-float64(after)/float64(before)), *engine)
	}
	return nil
}

func squeeze(ctx context.Context, input, output, engine, quality string, stderr io.Writer, verbose bool) (int64, int64, error) {
	logf := func(format string, args ...any) {
		if verbose {
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
	if verbose {
		// Surface pdfcpu's repair and skipped-object notices.
		log.SetCLILogger(stdlog.New(stderr, "", 0))
		defer log.DisableLoggers()
	}
	start := time.Now()
	switch engine {
	case "pdfcpu":
		err = api.Optimize(ctx, in, tmp, conf, nil)
	case "ghostscript":
		err = ghostscript(ctx, in, tmp, quality, stderr, verbose)
	default:
		err = fmt.Errorf("unknown engine %q", engine)
	}
	if err != nil {
		return 0, 0, fmt.Errorf("%s: %w", engine, err)
	}
	compressed, err := tmp.Stat()
	if err != nil {
		return 0, 0, err
	}
	logf("%s: %d bytes in %s", engine, compressed.Size(), time.Since(start).Round(time.Millisecond))
	if _, err := tmp.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
	start = time.Now()
	if err := api.Validate(ctx, tmp, conf, nil); err != nil {
		return 0, 0, fmt.Errorf("%s output: %w", engine, err)
	}
	logf("validated in %s", time.Since(start).Round(time.Millisecond))
	source := tmp
	if compressed.Size() >= info.Size() {
		source = in
	}
	if _, err := source.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
	logf("writing %s", output)
	written, err := writeNew(ctx, output, source)
	return info.Size(), written, err
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

func ghostscript(ctx context.Context, input, output *os.File, quality string, stderr io.Writer, verbose bool) error {
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
	var quiet []string
	if !verbose {
		quiet = []string{"-q"}
	}
	cmd := exec.CommandContext(ctx, executable, append(quiet,
		"-dSAFER", "-dBATCH", "-dNOPAUSE", "-dPDFSTOPONERROR",
		"-sDEVICE=pdfwrite", "-dPDFSETTINGS=/"+quality,
		"-sOutputFile=-", "-sstdout=%stderr", "-f", "-")...)
	// Stream file contents, not user paths, to avoid Ghostscript filename syntax.
	cmd.Stdin, cmd.Stdout, cmd.Stderr = input, output, stderr
	cmd.Env = append(os.Environ(), "GS_OPTIONS=")
	if verbose {
		fmt.Fprintln(stderr, "running", strings.Join(cmd.Args, " "))
	}
	if err := cmd.Run(); err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return err
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
