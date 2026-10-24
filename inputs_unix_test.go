//go:build !windows

package main

import (
	"context"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestReplacedInputsDoNotBlock(t *testing.T) {
	for _, mode := range []string{"compress", "images", "text"} {
		t.Run(mode, func(t *testing.T) {
			dir := t.TempDir()
			input, output := filepath.Join(dir, "input.pdf"), filepath.Join(dir, "output")
			writeFile(t, input, testPDF(0))
			files, err := expandInputs(t.Context(), []string{dir}, false)
			if err != nil || len(files) != 1 {
				t.Fatalf("preflight: %v, %v", files, err)
			}
			if err := os.Remove(input); err != nil {
				t.Fatal(err)
			}
			if err := syscall.Mkfifo(input, 0600); err != nil {
				t.Fatal(err)
			}
			done := make(chan error, 1)
			go func() {
				if mode == "compress" {
					_, err := prepare(t.Context(), files[0].path, output, options{}, io.Discard)
					done <- err
				} else {
					done <- extract(t.Context(), files[0].path, output, options{extract: mode}, io.Discard)
				}
			}()
			select {
			case err := <-done:
				if err == nil || !strings.Contains(err.Error(), "regular PDF file") {
					t.Fatalf("special input: %v", err)
				}
			case <-time.After(2 * time.Second):
				// Unblock an old blocking reader so a failing test leaves no goroutine.
				writer, err := os.OpenFile(input, os.O_WRONLY|syscall.O_NONBLOCK, 0)
				if err != nil {
					t.Fatal(err)
				}
				writer.Close()
				<-done
				t.Fatal("opening the replaced input blocked")
			}
			assertMissing(t, output)
			assertNoTemps(t, dir)
		})
	}
}

func TestExternalToolsUseInputSnapshot(t *testing.T) {
	for _, mode := range []string{"encrypted", "inline", "text"} {
		t.Run(mode, func(t *testing.T) {
			binary := "qpdf"
			if mode == "text" {
				if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
					t.Skip("optional tool integration")
				}
				requirePythonTools(t)
				binary = os.Getenv("PDF_SQUEEZER_PYTHON")
				if binary == "" {
					binary = "python3"
				}
			}
			requireTool(t, binary)
			real, err := exec.LookPath(binary)
			if err != nil {
				t.Fatal(err)
			}
			dir := t.TempDir()
			input, output := filepath.Join(dir, "input.pdf"), filepath.Join(dir, "output")
			args := []string{"--extract", "images", "--inline-images"}
			if mode == "encrypted" {
				plain, owner := filepath.Join(dir, "plain.pdf"), filepath.Join(dir, "owner.txt")
				writeFile(t, plain, testPDF(0))
				writeFile(t, owner, []byte("example-owner\n"))
				if err := run(t.Context(), []string{"--encrypt-owner-file", owner, "-o", input, plain}, io.Discard, io.Discard); err != nil {
					t.Fatal(err)
				}
				args = []string{"--password-file", owner}
			} else {
				writeFile(t, input, testPDF(0))
				if mode == "text" {
					args = []string{"--extract", "text"}
				}
			}
			stub := filepath.Join(dir, "qpdf")
			writeFile(t, stub, []byte("#!/bin/sh\nfor arg do\n  case \"$arg\" in \"$TEST_INPUT\"|\"--copy-encryption=$TEST_INPUT\") exit 91 ;; esac\ndone\nexec \"$TEST_TOOL\" \"$@\"\n"))
			if err := os.Chmod(stub, 0700); err != nil {
				t.Fatal(err)
			}
			t.Setenv("TEST_INPUT", input)
			t.Setenv("TEST_TOOL", real)
			t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
			if mode == "text" {
				t.Setenv("PDF_SQUEEZER_PYTHON", stub)
			}
			ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
			defer cancel()
			if err := run(ctx, append(args, "-o", output, input), io.Discard, io.Discard); err != nil {
				t.Fatalf("external tool reopened the input path: %v", err)
			}
			assertNoTemps(t, dir)
		})
	}
}

func TestToolsKeepScratchFilesInStaging(t *testing.T) {
	for _, mode := range []string{"ghostscript", "python"} {
		t.Run(mode, func(t *testing.T) {
			binary, args := "gs", []string{"--engine", "ghostscript"}
			if mode == "python" {
				if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
					t.Skip("optional tool integration")
				}
				requirePythonTools(t)
				binary, args = os.Getenv("PDF_SQUEEZER_PYTHON"), []string{"--extract", "text"}
				if binary == "" {
					binary = "python3"
				}
			}
			requireTool(t, binary)
			real, err := exec.LookPath(binary)
			if err != nil {
				t.Fatal(err)
			}
			dir, tools := t.TempDir(), t.TempDir()
			input, output, record := filepath.Join(dir, "input.pdf"), filepath.Join(dir, "output"), filepath.Join(tools, "record")
			writeFile(t, input, testPDF(64000))
			// Ghostscript must read the staged path, not a pipe it would spool to a temporary copy.
			stub := filepath.Join(tools, "gs")
			writeFile(t, stub, []byte("#!/bin/sh\nif [ \"$TEST_MODE\" = ghostscript ] && [ -n \"$(head -c 1)\" ]; then exit 92; fi\nprintf '%s\\n' \"$TMPDIR\" > \"$TEST_RECORD\"\nexec \"$TEST_TOOL\" \"$@\"\n"))
			if err := os.Chmod(stub, 0700); err != nil {
				t.Fatal(err)
			}
			t.Setenv("TEST_MODE", mode)
			t.Setenv("TEST_RECORD", record)
			t.Setenv("TEST_TOOL", real)
			t.Setenv("PATH", tools+string(os.PathListSeparator)+os.Getenv("PATH"))
			if mode == "python" {
				t.Setenv("PDF_SQUEEZER_PYTHON", stub)
			}
			if err := run(t.Context(), append(args, "-o", output, input), io.Discard, io.Discard); err != nil {
				t.Fatal(err)
			}
			if scratch := strings.TrimSpace(string(readFile(t, record))); !strings.HasPrefix(scratch, filepath.Join(dir, ".pdf-squeezer-")) {
				t.Fatalf("%s scratch directory %q is outside the private staging directory", mode, scratch)
			}
			assertNoTemps(t, dir)
		})
	}
}

func TestExpandInputsSkipsSpecialFiles(t *testing.T) {
	dir := t.TempDir()
	pipe := filepath.Join(dir, "pipe.pdf")
	if err := syscall.Mkfifo(pipe, 0600); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(dir, "link.pdf")
	if err := os.Symlink(pipe, link); err != nil {
		t.Fatal(err)
	}
	regular := filepath.Join(dir, "document.pdf")
	writeFile(t, regular, testPDF(0))
	for _, input := range []string{pipe, link} {
		if _, err := expandInputs(t.Context(), []string{input}, false); err == nil || !strings.Contains(err.Error(), "regular PDF file") {
			t.Errorf("special input %s: %v", input, err)
		}
	}
	for _, recursive := range []bool{false, true} {
		files, err := expandInputs(t.Context(), []string{dir}, recursive)
		if err != nil || len(files) != 1 || files[0].path != regular {
			t.Fatalf("directory scan: %v, %v", files, err)
		}
	}
}
