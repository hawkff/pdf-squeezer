//go:build !windows

package main

import (
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
)

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
