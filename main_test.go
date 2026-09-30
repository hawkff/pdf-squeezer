package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"testing/iotest"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
)

func TestEngines(t *testing.T) {
	for _, engine := range []string{"pdfcpu", "ghostscript"} {
		t.Run(engine, func(t *testing.T) {
			if engine == "ghostscript" {
				requireTool(t, "gs")
				// Inherited options must not disable rendering or override safety flags.
				t.Setenv("GS_OPTIONS", "-dNODISPLAY")
			}
			dir := t.TempDir()
			input := filepath.Join(dir, "@scan 100%.pdf")
			original := testPDF(64 * 1024)
			writeFile(t, input, original)
			var stdout, stderr bytes.Buffer
			args := []string{"--engine", engine}
			if engine == "ghostscript" {
				args = append(args, "--quality", "screen")
			}
			if err := run(t.Context(), append(args, input), &stdout, &stderr); err != nil {
				t.Fatalf("%v\n%s", err, &stderr)
			}
			output := filepath.Join(dir, "@scan 100%.squeezed.pdf")
			result := readFile(t, output)
			if len(result) >= len(original) {
				t.Fatalf("did not shrink: %d -> %d bytes", len(original), len(result))
			}
			if !bytes.Equal(readFile(t, input), original) {
				t.Fatal("input changed")
			}
			conf := model.NewStatelessConfiguration()
			if err := api.Validate(t.Context(), bytes.NewReader(result), conf, nil); err != nil {
				t.Fatalf("invalid output: %v", err)
			}
			if pages, err := api.PageCount(t.Context(), bytes.NewReader(result), conf); err != nil || pages != 1 {
				t.Fatalf("page count = %d, err = %v", pages, err)
			}
			if !strings.Contains(stdout.String(), "smaller, "+engine) {
				t.Fatalf("missing size summary: %s", &stdout)
			}
			assertNoTemps(t, dir)
			if path, err := exec.LookPath("pdftotext"); err == nil {
				text, err := exec.Command(path, output, "-").CombinedOutput()
				if err != nil || !bytes.Contains(text, []byte("Squeeze this PDF.")) {
					t.Fatalf("text changed: %q, err = %v", text, err)
				}
			} else if os.Getenv("PDF_SQUEEZER_INTEGRATION") == "1" {
				t.Fatal("pdftotext is required for integration tests")
			}
		})
	}
}

func TestNoReduction(t *testing.T) {
	dir := t.TempDir()
	input, output := filepath.Join(dir, "tiny.pdf"), filepath.Join(dir, "copy.pdf")
	original := testPDF(0)
	writeFile(t, input, original)
	var stdout bytes.Buffer
	if err := run(t.Context(), []string{"-o", output, input}, &stdout, io.Discard); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(readFile(t, output), original) || !strings.Contains(stdout.String(), "no reduction") {
		t.Fatalf("expected original bytes and no-reduction summary: %s", &stdout)
	}
	assertNoTemps(t, dir)
}

func TestRejectsInvalidArguments(t *testing.T) {
	for _, args := range [][]string{
		{}, {"one.pdf", "two.pdf"}, {"--engine", "other", "input.pdf"},
		{"--quality", "screen", "input.pdf"},
		{"--engine", "ghostscript", "--quality", "other", "input.pdf"},
		{"input.pdf", "--engine", "ghostscript"}, {"--unknown"},
	} {
		if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
			t.Errorf("accepted %q", args)
		}
	}
	if err := run(t.Context(), []string{"--help"}, io.Discard, io.Discard); !errors.Is(err, flag.ErrHelp) {
		t.Fatalf("help: %v", err)
	}
}

func TestRejectsInvalidInput(t *testing.T) {
	for _, data := range [][]byte{nil, []byte("not a PDF"), []byte("%PDF-1.7\nbroken")} {
		t.Run(fmt.Sprint(len(data)), func(t *testing.T) {
			dir := t.TempDir()
			input, output := filepath.Join(dir, "bad.pdf"), filepath.Join(dir, "out.pdf")
			writeFile(t, input, data)
			if err := run(t.Context(), []string{"-o", output, input}, io.Discard, io.Discard); err == nil {
				t.Fatal("accepted invalid PDF")
			}
			assertMissing(t, output)
			assertNoTemps(t, dir)
		})
	}
	dir := t.TempDir()
	for _, input := range []string{dir, filepath.Join(dir, "missing.pdf")} {
		if err := run(t.Context(), []string{input}, io.Discard, io.Discard); err == nil {
			t.Errorf("accepted %s", input)
		}
	}
}

func TestNoOverwrite(t *testing.T) {
	dir := t.TempDir()
	input := filepath.Join(dir, "in.pdf")
	original := testPDF(1000)
	writeFile(t, input, original)
	for _, kind := range []string{"same", "existing", "symlink", "hardlink"} {
		t.Run(kind, func(t *testing.T) {
			output := filepath.Join(dir, kind+".pdf")
			switch kind {
			case "same":
				output = input
			case "existing":
				writeFile(t, output, original)
			case "symlink":
				if err := os.Symlink(input, output); err != nil {
					t.Skip(err)
				}
			case "hardlink":
				if err := os.Link(input, output); err != nil {
					t.Skip(err)
				}
			}
			if err := run(t.Context(), []string{"-o", output, input}, io.Discard, io.Discard); err == nil {
				t.Fatal("overwrote an existing file")
			}
			if !bytes.Equal(readFile(t, output), original) || !bytes.Equal(readFile(t, input), original) {
				t.Fatal("existing data changed")
			}
		})
	}
	assertNoTemps(t, dir)
}

func TestMissingGhostscript(t *testing.T) {
	t.Setenv("PATH", t.TempDir())
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, testPDF(1000))
	err := run(t.Context(), []string{"--engine", "ghostscript", "-o", output, input}, io.Discard, io.Discard)
	if err == nil || !strings.Contains(err.Error(), "Ghostscript not found") {
		t.Fatalf("unexpected error: %v", err)
	}
	assertMissing(t, output)
	assertNoTemps(t, dir)
}

func TestBrokenGhostscriptOutput(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("uses a POSIX shell stub")
	}
	for _, exit := range []string{"0", "1"} {
		t.Run("exit="+exit, func(t *testing.T) {
			dir := t.TempDir()
			stub := filepath.Join(dir, "gs")
			writeFile(t, stub, []byte("#!/bin/sh\nprintf '%s\\n' '%PDF-1.7' 'broken'\nexit "+exit+"\n"))
			if err := os.Chmod(stub, 0700); err != nil {
				t.Fatal(err)
			}
			t.Setenv("PATH", dir)
			input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
			writeFile(t, input, testPDF(1000))
			err := run(t.Context(), []string{"--engine", "ghostscript", "-o", output, input}, io.Discard, io.Discard)
			if err == nil {
				t.Fatal("accepted broken Ghostscript output")
			}
			assertMissing(t, output)
			assertNoTemps(t, dir)
		})
	}
}

func TestWriteNewFailure(t *testing.T) {
	path := filepath.Join(t.TempDir(), "out.pdf")
	failure := errors.New("read failed")
	reader := io.MultiReader(strings.NewReader("partial"), iotest.ErrReader(failure))
	if _, err := writeNew(t.Context(), path, reader); !errors.Is(err, failure) {
		t.Fatalf("unexpected error: %v", err)
	}
	assertMissing(t, path)
	writeFile(t, path, []byte("keep"))
	if _, err := writeNew(t.Context(), path, strings.NewReader("replace")); !errors.Is(err, os.ErrExist) {
		t.Fatalf("unexpected error: %v", err)
	}
	if string(readFile(t, path)) != "keep" {
		t.Fatal("existing output changed")
	}
}

func TestCanceled(t *testing.T) {
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if err := run(ctx, []string{"missing.pdf"}, io.Discard, io.Discard); !errors.Is(err, context.Canceled) {
		t.Fatalf("unexpected error: %v", err)
	}
	output := filepath.Join(t.TempDir(), "out.pdf")
	if _, err := writeNew(ctx, output, strings.NewReader("data")); !errors.Is(err, context.Canceled) {
		t.Fatalf("unexpected error: %v", err)
	}
	assertMissing(t, output)
}

func testPDF(padding int) []byte {
	content := "BT /F1 12 Tf 20 100 Td (Squeeze this PDF.) Tj ET\n"
	objects := []string{
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
		"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
	}
	var pdf bytes.Buffer
	pdf.WriteString("%PDF-1.4\n")
	// Serialization whitespace can shrink without changing page content.
	pdf.WriteString(strings.Repeat(" ", padding))
	offsets := make([]int, len(objects))
	for i, object := range objects {
		offsets[i] = pdf.Len()
		fmt.Fprintf(&pdf, "%d 0 obj\n%s\nendobj\n", i+1, object)
	}
	xref := pdf.Len()
	fmt.Fprintf(&pdf, "xref\n0 %d\n0000000000 65535 f \n", len(objects)+1)
	for _, offset := range offsets {
		fmt.Fprintf(&pdf, "%010d 00000 n \n", offset)
	}
	fmt.Fprintf(&pdf, "trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n", len(objects)+1, xref)
	return pdf.Bytes()
}

func requireTool(t *testing.T, name string) {
	t.Helper()
	if _, err := exec.LookPath(name); err != nil {
		if os.Getenv("PDF_SQUEEZER_INTEGRATION") == "1" {
			t.Fatal(err)
		}
		t.Skip(name + " is not installed")
	}
}

func writeFile(t *testing.T, path string, data []byte) {
	t.Helper()
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
}

func readFile(t *testing.T, path string) []byte {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func assertMissing(t *testing.T, path string) {
	t.Helper()
	if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("expected no file at %s, got %v", path, err)
	}
}

func assertNoTemps(t *testing.T, dir string) {
	t.Helper()
	paths, err := filepath.Glob(filepath.Join(dir, ".pdf-squeezer-*.pdf"))
	if err != nil || len(paths) != 0 {
		t.Fatalf("temporary files remain: %v, err = %v", paths, err)
	}
}
