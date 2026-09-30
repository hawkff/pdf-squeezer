package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
)

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
	engine := flags.String("engine", "pdfcpu", "compression engine: pdfcpu or ghostscript")
	quality := flags.String("quality", "", "Ghostscript preset: screen, ebook (default), printer, prepress")
	var output string
	flags.StringVar(&output, "output", "", "output path (default: INPUT.squeezed.pdf)")
	flags.StringVar(&output, "o", "", "output path (short form)")
	flags.Usage = func() {
		fmt.Fprintln(stderr, "Usage: pdf-squeezer [flags] input.pdf")
		fmt.Fprintln(stderr, "\nOptimize with pdfcpu, or choose Ghostscript for lossy image compression.")
		fmt.Fprintln(stderr, "The input stays unchanged. Existing output files are never overwritten.")
		fmt.Fprintln(stderr)
		flags.PrintDefaults()
	}
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 1 {
		flags.Usage()
		return errors.New("provide one input PDF, with flags before the filename")
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
	input := flags.Arg(0)
	if output == "" {
		output = strings.TrimSuffix(input, filepath.Ext(input)) + ".squeezed.pdf"
	}
	before, after, err := squeeze(ctx, input, output, *engine, *quality, stderr)
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

func squeeze(ctx context.Context, input, output, engine, quality string, stderr io.Writer) (int64, int64, error) {
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

	switch engine {
	case "pdfcpu":
		conf := model.NewStatelessConfiguration()
		conf.Offline = true
		conf.PostProcessValidate = true
		conf.PreserveInfoDict = true
		err = api.Optimize(ctx, in, tmp, conf, nil)
	case "ghostscript":
		err = ghostscript(ctx, in, tmp, quality, stderr)
	default:
		err = fmt.Errorf("unknown engine %q", engine)
	}
	if err != nil {
		return 0, 0, fmt.Errorf("%s: %w", engine, err)
	}
	if err := checkPDF(tmp); err != nil {
		return 0, 0, fmt.Errorf("%s output: %w", engine, err)
	}
	compressed, err := tmp.Stat()
	if err != nil {
		return 0, 0, err
	}
	source := tmp
	if compressed.Size() >= info.Size() {
		source = in
	}
	if _, err := source.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
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

func ghostscript(ctx context.Context, input, output *os.File, quality string, stderr io.Writer) error {
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
	cmd := exec.CommandContext(ctx, executable,
		"-q", "-dSAFER", "-dBATCH", "-dNOPAUSE", "-dPDFSTOPONERROR",
		"-sDEVICE=pdfwrite", "-dPDFSETTINGS=/"+quality,
		"-sOutputFile=-", "-sstdout=%stderr", "-f", "-")
	// Stream file contents, not user paths, to avoid Ghostscript filename syntax.
	cmd.Stdin, cmd.Stdout, cmd.Stderr = input, output, stderr
	cmd.Env = append(os.Environ(), "GS_OPTIONS=")
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
