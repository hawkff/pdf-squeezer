package main

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
)

//go:embed tools/pdf_tools.py
var pythonSource []byte

// Bound diagnostic output without blocking a child on an unread pipe.
// imageError is tracked even when the retained diagnostic text is truncated.
type boundedBuffer struct {
	data                  bytes.Buffer
	truncated, imageError bool
	tail                  []byte
}

func (b *boundedBuffer) Bytes() []byte  { return b.data.Bytes() }
func (b *boundedBuffer) String() string { return b.data.String() }

func (b *boundedBuffer) Write(p []byte) (int, error) {
	combined := append(b.tail, p...)
	b.imageError = b.imageError || bytes.Contains(combined, []byte("recoverable image error"))
	b.tail = bytes.Clone(combined[max(0, len(combined)-64):])
	n := min(len(p), (1<<20)-b.data.Len())
	b.data.Write(p[:n])
	b.truncated = b.truncated || n != len(p)
	return len(p), nil
}

type lockedWriter struct {
	mu     sync.Mutex
	writer io.Writer
}

func (w *lockedWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.writer.Write(p)
}

func toolError(ctx context.Context, name string, err error, output *boundedBuffer, stderr io.Writer, secrets ...string) error {
	text := output.String()
	for _, secret := range secrets {
		if secret != "" {
			text = strings.ReplaceAll(text, secret, "[redacted]")
			if output.truncated {
				for n := min(len(secret)-1, len(text)); n > 0; n-- {
					if strings.HasSuffix(text, secret[:n]) {
						text = text[:len(text)-n] + "[redacted]"
						break
					}
				}
			}
		}
	}
	if text != "" {
		fmt.Fprint(stderr, text)
		if !strings.HasSuffix(text, "\n") {
			fmt.Fprintln(stderr)
		}
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	if err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	return nil
}

func qpdf(ctx context.Context, args []string, password string, stderr io.Writer) error {
	path, err := exec.LookPath("qpdf")
	if err != nil {
		return errors.New("qpdf is required for inline images and encrypted input; install it and add it to PATH")
	}
	// qpdf's @- reads one argument per line. Passwords never enter argv or logs.
	var private, public, secrets []string
	if password != "" {
		private = append(private, "--password="+password)
		secrets = append(secrets, password)
	}
	for _, arg := range args {
		if strings.HasPrefix(arg, "--encryption-file-password=") {
			secret := strings.TrimPrefix(arg, "--encryption-file-password=")
			if strings.ContainsAny(secret, "\r\n\x00") {
				return errors.New("invalid encryption password")
			}
			private = append(private, arg)
			secrets = append(secrets, secret)
		} else {
			public = append(public, arg)
		}
	}
	if len(private) > 0 {
		public = append([]string{"@-"}, public...)
	}
	cmd := exec.CommandContext(ctx, path, public...)
	cmd.Stdin = strings.NewReader(strings.Join(private, "\n") + "\n")
	var messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &messages, &messages
	configureCommand(cmd)
	// A qpdf warning is a failure: requested transformations must not silently lose data.
	return toolError(ctx, "qpdf", cmd.Run(), &messages, stderr, secrets...)
}

func pythonPDF(ctx context.Context, operation, input, output string, opts options, stderr io.Writer) error {
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	path, err := exec.LookPath(python)
	if err != nil {
		return errors.New("Python 3 is required for this operation; set PDF_SQUEEZER_PYTHON to a Python environment with the optional PDF tools")
	}
	script := filepath.Join(filepath.Dir(output), "pdf-tools.py")
	if err := os.WriteFile(script, pythonSource, 0600); err != nil {
		return err
	}
	defer os.Remove(script)
	request := map[string]any{
		"dpi": opts.dpi, "gray_dpi": opts.grayDPI, "mono_dpi": opts.monoDPI, "threshold": opts.dpiThreshold,
		"quality": opts.imageQuality, "clip": opts.clip, "extended": opts.extended, "lossless": opts.lossless,
		"reduce_bits": opts.reduceBits, "force": opts.force, "mono_codecs": opts.monoCodecs,
		"codecs": opts.imageCodecs, "memory_mib": opts.imageMemory,
		"gray": opts.gray && opts.engine == "pdfcpu", "geometry": opts.engine == "pdfcpu",
		"flatten": opts.flatten, "subset_fonts": opts.subsetFonts, "merge_fonts": opts.mergeFonts,
		"bitmap": opts.bitmap, "mrc": opts.mrc, "render_dpi": opts.renderDPI, "background_dpi": opts.backgroundDPI,
		"verbose": opts.verbose,
	}
	if operation == "text" {
		request["password"] = opts.password
	}
	data, err := json.Marshal(request)
	if err != nil {
		return err
	}
	cmd := exec.CommandContext(ctx, path, "-I", script, operation, input, output)
	cmd.Stdin = bytes.NewReader(data)
	var messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &messages, &messages
	configureCommand(cmd)
	return toolError(ctx, "PDF tools", cmd.Run(), &messages, stderr, opts.password)
}

func extract(ctx context.Context, input, output string, opts options, stderr io.Writer) (err error) {
	f, err := os.Open(input)
	if err != nil {
		return err
	}
	defer f.Close()
	if err := checkPDF(f); err != nil {
		return err
	}
	work, err := os.MkdirTemp(filepath.Dir(output), ".pdf-squeezer-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(work)
	if opts.extract == "text" {
		staged := filepath.Join(work, "text.txt")
		if err := pythonPDF(ctx, "text", input, staged, opts, stderr); err != nil {
			return err
		}
		source, err := os.Open(staged)
		if err != nil {
			return err
		}
		defer source.Close()
		_, err = writeNew(ctx, output, source)
		return err
	}
	if opts.inline {
		staged := filepath.Join(work, "inline.pdf")
		if err := qpdf(ctx, []string{"--externalize-inline-images", "--ii-min-bytes=0", input, staged}, opts.password, stderr); err != nil {
			return err
		}
		prepared, err := os.Open(staged)
		if err != nil {
			return err
		}
		defer prepared.Close()
		f = prepared
	}
	imagesDir := filepath.Join(work, "images")
	if err := os.Mkdir(imagesDir, 0700); err != nil {
		return err
	}
	seen := map[int]bool{}
	err = api.ExtractImages(ctx, f, nil, func(img model.Image, _ bool, objNr int) error {
		if seen[objNr] {
			return nil
		}
		seen[objNr] = true
		if !listContains("png,jpg,jpeg,tif,tiff,jp2,jpx,pbm", img.FileType) {
			return fmt.Errorf("unsupported extracted image format %q", img.FileType)
		}
		name := fmt.Sprintf("image-%06d.%s", objNr, img.FileType)
		_, err := writeNew(ctx, filepath.Join(imagesDir, name), img.Reader)
		return err
	}, configuration(opts.password))
	if err != nil {
		return err
	}
	entries, err := os.ReadDir(imagesDir)
	if err != nil {
		return err
	}
	if err := os.Mkdir(output, 0700); err != nil {
		return err
	}
	defer func() {
		if err != nil {
			err = errors.Join(err, os.RemoveAll(output))
		}
	}()
	for _, entry := range entries {
		source, openErr := os.Open(filepath.Join(imagesDir, entry.Name()))
		if openErr != nil {
			return openErr
		}
		_, copyErr := writeNew(ctx, filepath.Join(output, entry.Name()), source)
		if err := errors.Join(copyErr, source.Close()); err != nil {
			return err
		}
	}
	return nil
}
