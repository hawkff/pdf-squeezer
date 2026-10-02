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
	opts, inputs, version, err := parseOptions(ctx, args, stderr)
	if err != nil {
		return err
	}
	if version {
		v := "unknown"
		if info, ok := debug.ReadBuildInfo(); ok {
			v = info.Main.Version
		}
		fmt.Fprintln(stdout, v)
		return nil
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	if len(inputs) == 0 {
		return nil
	} // --save-profile without a document
	files, err := expandInputs(ctx, inputs, opts.recursive)
	if err != nil {
		return err
	}
	if len(files) == 0 {
		return errors.New("no input PDFs found")
	}
	outputDir := false
	if opts.output != "" {
		info, err := os.Stat(opts.output)
		outputDir = err == nil && info.IsDir()
		if len(files) > 1 && !outputDir {
			return errors.New("batch --output must be an existing directory")
		}
	}
	if len(files) == 1 && opts.extract == "images" {
		outputDir = false
	}
	// Resolve the complete batch before writing any output. Exclusive creation still
	// protects against another process creating a destination after this preflight.
	outputs := make([]string, len(files))
	reserved := map[string]bool{}
	for i, file := range files {
		base := strings.TrimSuffix(filepath.Base(file.path), filepath.Ext(file.path))
		name := base + ".squeezed.pdf"
		if opts.extract == "images" {
			name = base + ".images"
		}
		if opts.extract == "text" {
			name = base + ".txt"
		}
		output := opts.output
		if output == "" {
			output = filepath.Join(filepath.Dir(file.path), name)
		}
		if outputDir {
			output = filepath.Join(opts.output, filepath.Dir(file.relative), name)
		}
		output, err = availableOutput(output, opts.collision, reserved)
		if err != nil {
			return err
		}
		outputs[i] = output
		reserved[output] = true
	}
	var failures []error
	for i, file := range files {
		if err := ctx.Err(); err != nil {
			return errors.Join(append(failures, err)...)
		}
		output := outputs[i]
		if outputDir {
			if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
				failures = append(failures, err)
				continue
			}
		}
		if opts.extract != "" {
			if err := extract(ctx, file.path, output, opts, stderr); err != nil {
				failures = append(failures, fmt.Errorf("%s: %w", file.path, err))
				continue
			}
			fmt.Fprintln(stdout, output)
			continue
		}
		before, after, err := squeeze(ctx, file.path, output, opts, stderr)
		if err != nil {
			failures = append(failures, fmt.Errorf("%s: %w", file.path, err))
			continue
		}
		switch {
		case before == after && !opts.requiredOutput():
			hint := "try --engine ghostscript"
			if opts.engine == "ghostscript" {
				hint = "try --quality screen or a lower --dpi"
			} else if !opts.images {
				hint = "try --images or --engine ghostscript"
			}
			fmt.Fprintf(stdout, "%s: no reduction; copied the original (%d bytes); %s\n", output, before, hint)
		case after > before:
			fmt.Fprintf(stdout, "%s: %d -> %d bytes (%.1f%% larger, %s)\n", output, before, after, 100*(float64(after)/float64(before)-1), opts.engine)
		default:
			fmt.Fprintf(stdout, "%s: %d -> %d bytes (%.1f%% smaller, %s)\n", output, before, after, 100*(1-float64(after)/float64(before)), opts.engine)
		}
	}
	return errors.Join(failures...)
}

func availableOutput(path, policy string, reserved map[string]bool) (string, error) {
	base, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	for n := 0; n < 10000; n++ {
		candidate := base
		if n > 0 {
			ext := filepath.Ext(base)
			candidate = fmt.Sprintf("%s.%d%s", strings.TrimSuffix(base, ext), n, ext)
		}
		_, err := os.Lstat(candidate)
		if errors.Is(err, os.ErrNotExist) && !reserved[candidate] {
			return candidate, nil
		}
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return "", err
		}
		if policy != "number" {
			return "", fmt.Errorf("output already exists or has a batch name collision: %s", candidate)
		}
	}
	return "", errors.New("too many numbered output collisions")
}

func configuration(password string) *model.Configuration {
	conf := model.NewStatelessConfiguration()
	conf.Offline = true
	conf.PreserveInfoDict = true
	conf.Optimize = false // Validation must not re-optimize output.
	conf.OptimizeDuplicateContentStreams = true
	conf.UserPW, conf.OwnerPW = password, password
	conf.Cmd = model.OPTIMIZE
	return conf
}

func squeeze(ctx context.Context, input, output string, opts options, stderr io.Writer) (int64, int64, error) {
	stderr = &lockedWriter{writer: stderr}
	logf := func(format string, args ...any) {
		if opts.verbose {
			fmt.Fprintf(stderr, format+"\n", args...)
		}
	}
	if err := ctx.Err(); err != nil {
		return 0, 0, err
	}
	in, err := os.Open(input)
	if err != nil {
		return 0, 0, err
	}
	defer in.Close()
	info, err := in.Stat()
	if err != nil {
		return 0, 0, err
	}
	if !info.Mode().IsRegular() {
		return 0, 0, errors.New("input must be a regular PDF file")
	}
	if err := checkPDF(in); err != nil {
		return 0, 0, fmt.Errorf("input: %w", err)
	}
	if _, err := os.Lstat(output); err == nil {
		return 0, 0, fmt.Errorf("output already exists: %s", output)
	} else if !errors.Is(err, os.ErrNotExist) {
		return 0, 0, err
	}
	work, err := os.MkdirTemp(filepath.Dir(output), ".pdf-squeezer-*")
	if err != nil {
		return 0, 0, err
	}
	defer os.RemoveAll(work)
	logf("input: %s (%d bytes)", input, info.Size())
	logf("staging in %s", work)
	if opts.verbose {
		log.SetCLILogger(stdlog.New(stderr, "", 0))
		defer log.DisableLoggers()
	}
	conf := configuration(opts.password)
	pdf, err := api.ReadAndValidate(ctx, in, conf)
	if err != nil {
		return 0, 0, fmt.Errorf("input: %w", err)
	}
	originalDates, err := documentDates(pdf)
	if err != nil {
		return 0, 0, err
	}
	encrypted := pdf.Encrypt != nil
	if encrypted && opts.privacy && !opts.decrypt && opts.encryptOwnerFile == "" {
		return 0, 0, errors.New("--privacy on encrypted input requires --decrypt or new output encryption to replace its identifier")
	}
	if len(pdf.Signatures) > 0 {
		fmt.Fprintln(stderr, "warning: rewriting this signed PDF invalidates its digital signatures")
	}
	start := time.Now()
	current, err := filepath.Abs(input)
	if err != nil {
		return 0, 0, err
	}
	// Every tool works on a private staged copy, never the original path.
	stage, err := stagePath(work)
	if err != nil {
		return 0, 0, err
	}
	if encrypted {
		logf("decrypting working copy")
		if err := qpdf(ctx, []string{"--decrypt", current, stage}, opts.password, stderr); err != nil {
			return 0, 0, err
		}
		pdf = nil
	} else {
		if _, err := in.Seek(0, io.SeekStart); err != nil {
			return 0, 0, err
		}
		if err := copyStage(ctx, stage, in); err != nil {
			return 0, 0, err
		}
	}
	current = stage
	apply := func(label string, process func(string, string) error) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		next, err := stagePath(work)
		if err != nil {
			return err
		}
		logf("%s", label)
		if err := process(current, next); err != nil {
			return fmt.Errorf("%s: %w", label, err)
		}
		current, pdf = next, nil
		return nil
	}
	if opts.inline || listContains(opts.strip, "images") {
		err = apply("externalizing inline images", func(in, out string) error {
			return qpdf(ctx, []string{"--externalize-inline-images", "--ii-min-bytes=0", in, out}, "", stderr)
		})
		if err != nil {
			return 0, 0, err
		}
	}
	if opts.engine == "ghostscript" || opts.cffFonts {
		err = apply("rewriting with Ghostscript", func(src, dst string) error {
			input, err := os.Open(src)
			if err != nil {
				return err
			}
			defer input.Close()
			output, err := os.OpenFile(dst, os.O_WRONLY|os.O_TRUNC, 0600)
			if err != nil {
				return err
			}
			err = ghostscript(ctx, input, output, opts, stderr)
			return errors.Join(err, output.Close())
		})
		if err != nil {
			return 0, 0, err
		}
	}
	if opts.advancedImages() || opts.flatten != "" || opts.mergeFonts || opts.subsetFonts || opts.bitmap || opts.mrc || opts.gray && opts.engine == "pdfcpu" {
		err = apply("processing advanced document features", func(in, out string) error { return pythonPDF(ctx, "transform", in, out, opts, stderr) })
		if err != nil {
			return 0, 0, err
		}
	}
	if pdf == nil {
		f, err := os.Open(current)
		if err != nil {
			return 0, 0, err
		}
		pdf, err = api.ReadAndValidate(ctx, f, configuration(""))
		err = errors.Join(err, f.Close())
		if err != nil {
			return 0, 0, fmt.Errorf("transformed input: %w", err)
		}
	}
	// Optimize structure first to avoid re-encoding duplicate image objects.
	if err := api.OptimizeContext(ctx, pdf); err != nil {
		return 0, 0, err
	}
	if opts.images {
		stats, err := optimizeImages(ctx, pdf, opts, logf)
		if err != nil {
			return 0, 0, err
		}
		logf("%s", stats)
	}
	if opts.timestamps == "preserve" && !opts.privacy {
		if err := restoreDocumentDates(pdf, originalDates); err != nil {
			return 0, 0, err
		}
	}
	if err := transformDocument(ctx, pdf, opts); err != nil {
		return 0, 0, err
	}
	if err := deduplicateStreams(ctx, pdf); err != nil {
		return 0, 0, err
	}
	next, err := stagePath(work)
	if err != nil {
		return 0, 0, err
	}
	out, err := os.OpenFile(next, os.O_WRONLY|os.O_TRUNC, 0600)
	if err != nil {
		return 0, 0, err
	}
	err = api.WriteContext(ctx, pdf, out)
	err = errors.Join(err, out.Close())
	if err != nil {
		return 0, 0, err
	}
	current, pdf = next, nil
	if opts.monoCodecs != "flate" {
		err = apply("encoding monochrome images", func(in, out string) error { return pythonPDF(ctx, "monochrome", in, out, opts, stderr) })
		if err != nil {
			return 0, 0, err
		}
		err = apply("deduplicating final streams", func(src, dst string) error {
			in, err := os.Open(src)
			if err != nil {
				return err
			}
			defer in.Close()
			final, err := api.ReadAndValidate(ctx, in, configuration(""))
			if err != nil {
				return err
			}
			if err := deduplicateStreams(ctx, final); err != nil {
				return err
			}
			out, err := os.OpenFile(dst, os.O_WRONLY|os.O_TRUNC, 0600)
			if err != nil {
				return err
			}
			err = api.WriteContext(ctx, final, out)
			return errors.Join(err, out.Close())
		})
		if err != nil {
			return 0, 0, err
		}
	}
	outputPassword := ""
	if opts.encryptOwnerFile != "" {
		err = apply("encrypting output", func(in, out string) error { return encryptPDF(ctx, in, out, opts) })
		outputPassword = opts.encryptOwner
	} else if encrypted && !opts.decrypt {
		original, absErr := filepath.Abs(input)
		if absErr != nil {
			return 0, 0, absErr
		}
		err = apply("preserving encryption", func(in, out string) error {
			return qpdf(ctx, []string{"--copy-encryption=" + original, "--encryption-file-password=" + opts.password, in, out}, "", stderr)
		})
		outputPassword = opts.password
	}
	if err != nil {
		return 0, 0, err
	}
	compressed, err := os.Open(current)
	if err != nil {
		return 0, 0, err
	}
	defer compressed.Close()
	ci, err := compressed.Stat()
	if err != nil {
		return 0, 0, err
	}
	if err := api.Validate(ctx, compressed, configuration(outputPassword), nil); err != nil {
		return 0, 0, fmt.Errorf("output validation: %w", err)
	}
	logf("%s: %d bytes in %s; validated", opts.engine, ci.Size(), time.Since(start).Round(time.Millisecond))
	source := compressed
	if ci.Size() >= info.Size() && !opts.requiredOutput() {
		source = in
	}
	if _, err := source.Seek(0, io.SeekStart); err != nil {
		return 0, 0, err
	}
	logf("writing %s", output)
	written, err := writeNew(ctx, output, source)
	if err == nil && opts.timestamps == "preserve" {
		if err = os.Chtimes(output, info.ModTime(), info.ModTime()); err != nil {
			err = errors.Join(err, os.Remove(output))
		}
	}
	return info.Size(), written, err
}

func stagePath(dir string) (string, error) {
	f, err := os.CreateTemp(dir, "stage-*.pdf")
	if err != nil {
		return "", err
	}
	return f.Name(), f.Close()
}

func copyStage(ctx context.Context, path string, in io.Reader) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_TRUNC, 0600)
	if err != nil {
		return err
	}
	_, err = io.Copy(f, contextReader{ctx, in})
	return errors.Join(err, f.Close())
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
	// With the pdfcpu engine, grayscale and DPI belong to the native/Python passes.
	colors := "-sColorConversionStrategy=LeaveColorUnchanged"
	if opts.gray && opts.engine == "ghostscript" {
		colors = "-sColorConversionStrategy=Gray"
	}
	args := []string{"-dSAFER", "-dBATCH", "-dNOPAUSE", "-dPDFSTOPONERROR", "-sDEVICE=pdfwrite"}
	if opts.engine == "ghostscript" {
		args = append(args, "-dPDFSETTINGS=/"+opts.quality)
	}
	args = append(args, colors)
	if opts.cffFonts {
		// Font conversion must not also downsample or recompress page images.
		args = append(args, "-dCompressFonts=true", "-dSubsetFonts=true", "-dEmbedAllFonts=true", "-dConvertFontsToCFF=true")
		if opts.engine != "ghostscript" {
			// pdfwrite auto-filters images to DCT by default; keep lossless data lossless.
			args = append(args, "-dDownsampleColorImages=false", "-dDownsampleGrayImages=false", "-dDownsampleMonoImages=false",
				"-dAutoFilterColorImages=false", "-dAutoFilterGrayImages=false", "-dColorImageFilter=/FlateEncode", "-dGrayImageFilter=/FlateEncode",
				"-dPassThroughJPEGImages=true", "-dPassThroughJPXImages=true")
		}
	}
	if opts.engine == "ghostscript" {
		for kind, dpi := range map[string]int{"Color": opts.dpi, "Gray": max(opts.dpi, opts.grayDPI), "Mono": opts.monoDPI} {
			if kind == "Gray" && opts.grayDPI > 0 {
				dpi = opts.grayDPI
			}
			if dpi > 0 {
				args = append(args, fmt.Sprintf("-dDownsample%sImages=true", kind), fmt.Sprintf("-d%sImageResolution=%d", kind, dpi), fmt.Sprintf("-d%sImageDownsampleThreshold=%g", kind, opts.dpiThreshold))
			}
		}
	}
	args = append(args, "-sOutputFile=-", "-sstdout=%stderr")
	if opts.cffFonts {
		args = append(args, "-c", "<< /NeverEmbed [] >> setdistillerparams")
	}
	args = append(args, "-f", "-")
	cmd := exec.CommandContext(ctx, executable, args...)
	cmd.Stdin, cmd.Stdout = contextReader{ctx, input}, output
	var messages boundedBuffer
	cmd.Stderr = &messages
	if opts.verbose {
		cmd.Stderr = io.MultiWriter(stderr, &messages)
		fmt.Fprintln(stderr, "running Ghostscript")
	}
	cmd.Env = append(os.Environ(), "GS_OPTIONS=")
	configureCommand(cmd)
	if err := cmd.Run(); err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if !opts.verbose {
			stderr.Write(messages.Bytes())
		}
		return err
	}
	if messages.imageError {
		return errors.New("Ghostscript skipped an image it could not decode; try --engine pdfcpu")
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
	if written, err = io.Copy(out, contextReader{ctx, source}); err != nil {
		return written, err
	}
	if err = ctx.Err(); err != nil {
		return written, err
	}
	return written, out.Sync()
}

type contextReader struct {
	ctx context.Context
	r   io.Reader
}

func (r contextReader) Read(p []byte) (int, error) {
	if err := r.ctx.Err(); err != nil {
		return 0, err
	}
	return r.r.Read(p)
}

type contextWriter struct {
	ctx context.Context
	w   io.Writer
}

func (w contextWriter) Write(p []byte) (int, error) {
	if err := w.ctx.Err(); err != nil {
		return 0, err
	}
	return w.w.Write(p)
}
