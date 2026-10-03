package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

func imagePDF(bits int, colorSpace, extra string, data []byte, more ...string) []byte {
	content := "q 64 0 0 64 0 0 cm /Im Do Q\n"
	objects := []string{
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /%s /BitsPerComponent %d %s /Length %d >>\nstream\n%s\nendstream", colorSpace, bits, extra, len(data), data),
	}
	return buildPDF(0, append(objects, more...)...)
}

func readContext(t *testing.T, path, password string) *model.Context {
	t.Helper()
	pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(readFile(t, path)), configuration(password))
	if err != nil {
		t.Fatal(err)
	}
	return pdf
}

func firstImage(t *testing.T, pdf *model.Context) types.StreamDict {
	t.Helper()
	for _, e := range pdf.Table {
		if sd, ok := imageStream(e); ok {
			return sd
		}
	}
	t.Fatal("image missing")
	return types.StreamDict{}
}

func TestLossless16BitAndMattePreservation(t *testing.T) {
	for _, matte := range []bool{false, true} {
		t.Run(fmt.Sprint(matte), func(t *testing.T) {
			var rgb, mask []byte
			for i := 0; i < 4096; i++ {
				rgb = append(rgb, 0x80, byte(i), 0x80, byte(i), 0x80, byte(i))
				mask = append(mask, 0x7f, byte(i))
			}
			matteEntry := ""
			if matte {
				matteEntry = "/Matte [0.5 0.5 0.5]"
			}
			maskObject := fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 16 %s /Length %d >>\nstream\n%s\nendstream", matteEntry, len(mask), mask)
			dir := t.TempDir()
			input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
			writeFile(t, input, imagePDF(16, "DeviceRGB", "/SMask 6 0 R", rgb, maskObject))
			args := []string{"--images", "--force-recompression", "-V", "-o", output, input}
			if matte {
				args = append(args, "--reduce-bit-depth")
			} else {
				args = append(args, "--lossless")
			}
			var log bytes.Buffer
			if err := run(t.Context(), args, io.Discard, &log); err != nil {
				t.Fatalf("%v\n%s", err, &log)
			}
			pdf := readContext(t, output, "")
			var parent, alpha types.StreamDict
			for _, entry := range pdf.Table {
				if sd, ok := imageStream(entry); ok && sd.IndirectRefEntry("SMask") != nil {
					parent = sd
					m, _, err := pdf.DereferenceStreamDict(*sd.IndirectRefEntry("SMask"))
					if err != nil {
						t.Fatal(err)
					}
					alpha = *m
				}
			}
			if alpha.Dict == nil || *alpha.IntEntry("BitsPerComponent") != 16 {
				t.Fatal("16-bit soft mask lost precision")
			}
			if err := alpha.Decode(); err != nil || !bytes.Equal(alpha.Content, mask) {
				t.Fatalf("soft mask changed: %v", err)
			}
			if *parent.IntEntry("BitsPerComponent") != 16 {
				t.Fatal("parent bit depth changed")
			}
			if matte && (!bytes.Equal(parent.Raw, rgb) || !strings.Contains(log.String(), "soft-mask matte")) {
				t.Fatal("matte-backed image was transformed")
			}
		})
	}
}

func TestImageBudgetCancellationAndDecode(t *testing.T) {
	if _, ok := sampleSize(math.MaxInt, math.MaxInt, 4, 16); ok {
		t.Fatal("accepted overflowing dimensions")
	}
	if _, ok := sampleSize(0, 1, 1, 8); ok {
		t.Fatal("accepted zero width")
	}
	if n, ok := sampleSize(9, 2, 1, 1); !ok || n != 4 {
		t.Fatalf("packed row size = %d, %v", n, ok)
	}
	samples := bytes.Repeat([]byte{0, 255, 128, 64}, 1024)
	pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(imagePDF(8, "DeviceGray", "/Decode [1 0]", samples)), configuration(""))
	if err != nil {
		t.Fatal(err)
	}
	opts := options{imageMemory: 512, imageQuality: 75, imageCodecs: "flate", lossless: true, force: true}
	stats, err := optimizeImages(t.Context(), pdf, opts, func(string, ...any) {})
	if err != nil || stats.changed != 1 {
		t.Fatalf("stats %v, %v", stats, err)
	}
	sd := firstImage(t, pdf)
	if _, found := sd.Find("Decode"); found {
		t.Fatal("normalized Decode remains")
	}
	if err := sd.Decode(); err != nil {
		t.Fatal(err)
	}
	for i, v := range samples {
		if sd.Content[i] != ^v {
			t.Fatal("inversion changed appearance")
		}
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if _, err := optimizeImages(ctx, pdf, opts, func(string, ...any) {}); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
	pdf, err = api.ReadAndValidate(t.Context(), bytes.NewReader(imagePDF(8, "DeviceGray", "", samples)), configuration(""))
	if err != nil {
		t.Fatal(err)
	}
	opts.imageMemory = 1
	stats, err = optimizeImages(t.Context(), pdf, opts, func(string, ...any) {})
	if err != nil || stats.preserved["memory budget"] != 1 || stats.changed != 0 {
		t.Fatalf("budget not enforced: %v, %v", stats, err)
	}
}

func TestFinalDeduplicationAcrossLosslessFilters(t *testing.T) {
	content := "q 32 0 0 32 0 0 cm /A Do Q q 32 0 0 32 32 0 cm /B Do Q"
	data := bytes.Repeat([]byte{19, 200}, 2048)
	compressed := deflate(t, data)
	pdf := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray /Length %d >>\nstream\n%s\nendstream", len(data), data),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray /Filter /FlateDecode /Length %d >>\nstream\n%s\nendstream", len(compressed), compressed))
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, pdf)
	if err := run(t.Context(), []string{"-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	result := readContext(t, output, "")
	count := 0
	for _, entry := range result.Table {
		if _, ok := imageStream(entry); ok {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("got %d image objects, wanted one", count)
	}
}

func TestProfilesBatchAndTimestamps(t *testing.T) {
	dir, output := t.TempDir(), t.TempDir()
	profilePath := filepath.Join(t.TempDir(), "profile.json")
	if err := run(t.Context(), []string{"--lossless", "--save-profile", profilePath}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	var p profile
	if err := json.Unmarshal(readFile(t, profilePath), &p); err != nil {
		t.Fatal(err)
	}
	if p.Version != 1 || p.Flags["lossless"] != "true" || p.Flags["image-codecs"] != "flate" {
		t.Fatalf("wrong profile: %v", p)
	}
	if err := os.Mkdir(filepath.Join(dir, "child"), 0700); err != nil {
		t.Fatal(err)
	}
	date := time.Date(2020, 2, 3, 4, 5, 6, 0, time.UTC)
	for _, name := range []string{"a.pdf", "child/b.pdf", "a.squeezed.pdf"} {
		path := filepath.Join(dir, name)
		writeFile(t, path, testPDF(64000))
		if err := os.Chtimes(path, date, date); err != nil {
			t.Fatal(err)
		}
	}
	args := []string{"--profile", profilePath, "--recursive", "-o", output, dir}
	if err := run(t.Context(), args, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"a.squeezed.pdf", "child/b.squeezed.pdf"} {
		info, err := os.Stat(filepath.Join(output, name))
		if err != nil {
			t.Fatal(err)
		}
		if !info.ModTime().Equal(date) {
			t.Errorf("timestamp changed: %v", info.ModTime())
		}
	}
	assertMissing(t, filepath.Join(output, "a.squeezed.squeezed.pdf"))
	if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
		t.Fatal("overwrote batch outputs")
	}
	if err := run(t.Context(), append(args, "--collision", "number"), io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	readFile(t, filepath.Join(output, "a.squeezed.1.pdf"))
	writeFile(t, profilePath, []byte(`{"version":1,"flags":{"output":"unexpected.pdf"}}`))
	if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
		t.Fatal("accepted path-bearing profile")
	}
}

func TestMetadataEditingAndRequiredOutput(t *testing.T) {
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, metadataPDF())
	if err := run(t.Context(), []string{"--metadata", "Title=Résumé", "--metadata", "Author=", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	pdf := readContext(t, output, "")
	info, err := pdf.DereferenceDict(*pdf.Info)
	if err != nil {
		t.Fatal(err)
	}
	title, err := pdf.DereferenceText(info["Title"])
	if err != nil || title != "Résumé" {
		t.Fatalf("title %q, %v", title, err)
	}
	if _, ok := info["Author"]; ok {
		t.Fatal("author remains")
	}
	for _, entry := range pdf.Table {
		if entry == nil || entry.Free {
			continue
		}
		if sd, ok := entry.Object.(types.StreamDict); ok && sd.NameEntry("Type") != nil && *sd.NameEntry("Type") == "Metadata" {
			t.Fatal("stale XMP remains")
		}
	}
}

func TestEncryptionRoundTrip(t *testing.T) {
	requireTool(t, "qpdf")
	dir := t.TempDir()
	input, encrypted, squeezed, decrypted := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "encrypted.pdf"), filepath.Join(dir, "squeezed.pdf"), filepath.Join(dir, "decrypted.pdf")
	user, owner := filepath.Join(dir, "user.txt"), filepath.Join(dir, "owner.txt")
	writeFile(t, input, testPDF(64000))
	writeFile(t, user, []byte("example-reader\n"))
	writeFile(t, owner, []byte("example-owner\n"))
	if err := run(t.Context(), []string{"--encrypt-user-file", user, "--encrypt-owner-file", owner, "-o", encrypted, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	if _, err := api.ReadContext(t.Context(), bytes.NewReader(readFile(t, encrypted)), configuration("wrong")); err == nil {
		t.Fatal("encrypted PDF opened without password")
	}
	if pdf := readContext(t, encrypted, "example-reader"); pdf.Encrypt == nil {
		t.Fatal("missing encryption")
	}
	if err := run(t.Context(), []string{"--password-file", user, "-o", squeezed, encrypted}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	if pdf := readContext(t, squeezed, "example-reader"); pdf.Encrypt == nil {
		t.Fatal("input encryption was discarded")
	}
	if err := run(t.Context(), []string{"--password-file", owner, "--decrypt", "-o", decrypted, encrypted}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	if pdf := readContext(t, decrypted, ""); pdf.Encrypt != nil {
		t.Fatal("decryption did not remove encryption")
	}
}

func TestExtractAndInlineImages(t *testing.T) {
	requireTool(t, "qpdf")
	dir := t.TempDir()
	input := filepath.Join(dir, "inline.pdf")
	content := "q 64 0 0 64 0 0 cm BI /W 64 /H 64 /BPC 8 /CS /G ID\n" + strings.Repeat("x", 4096) + "\nEI Q\n"
	pdf := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content))
	writeFile(t, input, pdf)
	output := filepath.Join(dir, "out.pdf")
	if err := run(t.Context(), []string{"--inline-images", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	firstImage(t, readContext(t, output, ""))
	extracted := filepath.Join(dir, "extracted")
	if err := run(t.Context(), []string{"--extract", "images", "-o", extracted, output}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(extracted)
	if err != nil || len(entries) != 1 {
		t.Fatalf("extracted %d images, %v", len(entries), err)
	}
	if err := run(t.Context(), []string{"--extract", "images", "-o", extracted, output}, io.Discard, io.Discard); err == nil {
		t.Fatal("overwrote extraction directory")
	}
}

func TestGhostscriptCompactFontPrograms(t *testing.T) {
	requireTool(t, "gs")
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, testPDF(64000))
	if err := run(t.Context(), []string{"--convert-fonts-cff", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	pdf := readContext(t, output, "")
	for _, entry := range pdf.Table {
		if entry == nil || entry.Free {
			continue
		}
		if sd, ok := entry.Object.(types.StreamDict); ok {
			if subtype := sd.NameEntry("Subtype"); subtype != nil && (*subtype == "Type1C" || *subtype == "CIDFontType0C") {
				return
			}
		}
	}
	t.Fatal("Ghostscript did not embed a compact font program")
}

func TestGhostscriptFontConversionKeepsImagesLossless(t *testing.T) {
	requireTool(t, "gs")
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	// pdfwrite only auto-selects DCT for large photographic images; use xorshift noise.
	samples, x := make([]byte, 3*1024*1024), uint32(2463534242)
	for i := range samples {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		samples[i] = byte(x)
	}
	content := "q 200 0 0 200 0 0 cm /Im Do Q\n"
	writeFile(t, input, buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 1024 /Height 1024 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length %d >>\nstream\n%s\nendstream", len(samples), samples)))
	// --timestamps now keeps the rewritten document even when it is larger.
	if err := run(t.Context(), []string{"--convert-fonts-cff", "--timestamps", "now", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	for _, filter := range firstImage(t, readContext(t, output, "")).FilterPipeline {
		if filter.Name == "DCTDecode" {
			t.Fatal("font conversion re-encoded a lossless image as JPEG")
		}
	}
}

func TestOptionsRejectConflicts(t *testing.T) {
	for _, args := range [][]string{
		{"--lossless", "--reduce-bit-depth"}, {"--lossless", "--dpi", "72"}, {"--lossless", "--image-codecs", "jpeg"},
		{"--image-memory", "0"}, {"--image-quality", "101"}, {"--dpi-threshold", "NaN"},
		{"--strip", "forms", "--flatten", "forms"}, {"--privacy", "--metadata", "Title=test"},
		{"--bitmap", "--mrc"}, {"--extract", "images", "--gray"},
		{"--encrypt-user-file", "example.txt"}, {"--flatten", "all,links"},
		{"--pdfa", "4", "--privacy"}, {"--pdfa", "4", "--strip", "output-intents"}, {"--output-intent", "profile.icc"},
		{"--pdfa", "4", "--font-file", "Arial"}, {"--strip", "javascript"}, {"--extract", "text", "--pdfa", "4"},
		{"--pdfa", "1b"}, {"--pdfa", "4f"},
	} {
		if _, _, _, err := parseOptions(t.Context(), append(args, "input.pdf"), io.Discard); err == nil {
			t.Errorf("accepted %v", args)
		}
	}
}

// pdfaPDF declares a PDF/A level in XMP without conforming to it.
func pdfaPDF(part, conformance string) []byte {
	xmp := `<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/" pdfaid:part="` + part + `"`
	if conformance != "" {
		xmp += ` pdfaid:conformance="` + conformance + `"`
	}
	xmp += `/></rdf:RDF></x:xmpmeta>`
	// Padding makes the rewrite smaller than the input, so the output is not the
	// original; the DeviceRGB fill keeps the document from conforming by accident.
	content := "0 0 1 rg 10 10 30 30 re f\n"
	return buildPDF(64000,
		"<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 5 0 R >>",
		fmt.Sprintf("<< /Type /Metadata /Subtype /XML /Length %d >>\nstream\n%s\nendstream", len(xmp), xmp),
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content))
}

func requirePythonTools(t *testing.T) {
	t.Helper()
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	if err := exec.Command(python, "-c", "import pikepdf, pymupdf, PIL, fontTools").Run(); err != nil {
		t.Fatal("optional Python tools are required")
	}
}

func TestPDFAValidationFailsClosed(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("POSIX tool stub")
	}
	dir := t.TempDir()
	document := filepath.Join(dir, "declared.pdf")
	writeFile(t, document, pdfaPDF("4", ""))
	job := func(report string) string {
		return `<report><jobs><job><item><name>` + document + `</name></item>` + report + `</job></jobs></report>`
	}
	failure := job(`<validationReport profileName="PDF/A-4 validation profile" isCompliant="false"><details>` +
		`<rule specification="ISO 19005-4:2020" clause="6.2.10.4.1" testNumber="1" status="failed"><description>The font programs for all fonts used for rendering within a conforming file shall be embedded</description>` +
		`<check status="failed"><context>root/document[0]/pages[0](3 0 obj PDPage)/contentStream[0](4 0 obj PDContentStream)/operators[3]/font[0](Helvetica)</context></check></rule>` +
		`<rule specification="ISO 19005-4:2020" clause="6.1.3" testNumber="1" status="passed"><description>irrelevant</description></rule>` +
		`</details></validationReport>`)
	for report, want := range map[string]string{
		job(`<validationReport profileName="PDF/A-4 validation profile" isCompliant="true"/>`): "",
		failure: "fonts 6.2.10.4.1-1",
		job(`<validationReport profileName="PDF/A-2B validation profile" isCompliant="true"/>`): "did not confirm",
		`<report/>`:  "did not report",
		`broken xml`: "veraPDF report",
		``:           "no report",
	} {
		path := filepath.Join(dir, "verapdf")
		writeFile(t, path, []byte("#!/bin/sh\nprintf '%s' '"+report+"'\n"))
		if err := os.Chmod(path, 0700); err != nil {
			t.Fatal(err)
		}
		t.Setenv("PATH", dir)
		results, err := validatePDFA(t.Context(), "4", []string{document}, io.Discard)
		if err == nil {
			err = results[document]
		}
		if want == "" && err != nil {
			t.Fatalf("compliant report rejected: %v", err)
		}
		if want != "" && (err == nil || !strings.Contains(err.Error(), want)) {
			t.Fatalf("report %.60s: got %v, want %q", report, err, want)
		}
		if report == failure && !strings.Contains(err.Error(), "object 4 Helvetica") {
			t.Fatalf("diagnostics lack the object: %v", err)
		}
	}
	for _, c := range []struct{ part, conformance, want string }{
		{"4", "", "4"}, {"4", "F", "4f"}, {"2", "B", "2b"}, {"1", "A", "1a"}, {"3", "U", "3u"},
	} {
		writeFile(t, document, pdfaPDF(c.part, c.conformance))
		if flavour, err := declaredFlavour(t.Context(), document); err != nil || flavour != c.want {
			t.Fatalf("part %s conformance %q: flavour %q, %v", c.part, c.conformance, flavour, err)
		}
	}
	writeFile(t, document, pdfaPDF("2", ""))
	if _, err := declaredFlavour(t.Context(), document); err == nil {
		t.Fatal("part 2 without conformance accepted")
	}
	plain := filepath.Join(dir, "plain.pdf")
	writeFile(t, plain, testPDF(0))
	if flavour, err := declaredFlavour(t.Context(), plain); err != nil || flavour != "" {
		t.Fatalf("undeclared document: %q, %v", flavour, err)
	}
}

func TestDeclaredPDFA1KeepsClassicCrossReference(t *testing.T) {
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, pdfaPDF("1", "B"))
	t.Setenv("PATH", t.TempDir()) // no veraPDF: preserve structure, warn, publish
	var messages bytes.Buffer
	if err := run(t.Context(), []string{"-V", "-o", output, input}, io.Discard, &messages); err != nil {
		t.Fatalf("%v\n%s", err, &messages)
	}
	data := readFile(t, output)
	if bytes.Contains(data, []byte("/XRef")) || bytes.Contains(data, []byte("/ObjStm")) {
		t.Fatal("PDF/A-1 input was written with cross-reference or object streams")
	}
	if !strings.Contains(messages.String(), "declares PDF/A-1B") || !strings.Contains(messages.String(), "not verified") {
		t.Fatalf("missing preservation messages:\n%s", &messages)
	}
	broken := filepath.Join(dir, "broken.pdf")
	messages.Reset()
	if err := run(t.Context(), []string{"--privacy", "-o", broken, input}, io.Discard, &messages); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(messages.String(), "--privacy removes what it requires") {
		t.Fatalf("missing conflict warning:\n%s", &messages)
	}
}

func TestPDFA4FailsClosedWithoutTools(t *testing.T) {
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, testPDF(0))
	t.Setenv("PATH", t.TempDir())
	t.Setenv("PDF_SQUEEZER_PYTHON", "")
	if err := run(t.Context(), []string{"--pdfa", "4", "-o", output, input}, io.Discard, io.Discard); err == nil {
		t.Fatal("PDF/A-4 succeeded without its tools")
	}
	assertMissing(t, output)
	assertNoTemps(t, dir)
}

func TestPDFAConversion(t *testing.T) {
	if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
		t.Skip("optional tool integration")
	}
	requireTool(t, "verapdf")
	requireTool(t, "qpdf")
	requirePythonTools(t)
	dir := t.TempDir()
	input := filepath.Join(dir, "text.pdf")
	writeFile(t, input, testPDF(0))
	attached := filepath.Join(dir, "attached.pdf")
	writeFile(t, attached, buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles << /Names [(note.txt) 4 0 R] >> >> >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
		"<< /Type /Filespec /F (note.txt) /EF << /F 5 0 R >> >>",
		"<< /Type /EmbeddedFile /Length 5 >>\nstream\nhello\nendstream"))
	for _, c := range []struct {
		name, input, header string
		args                []string
		want                string
	}{
		{"text-4", input, "%PDF-2.0", []string{"--pdfa", "4", "-V"}, "4"},
		{"text-2b", input, "%PDF-1.7", []string{"--pdfa", "2b"}, "2b"},
		{"attached-4f", attached, "%PDF-2.0", []string{"--pdfa", "4"}, "4f"},
		{"attached-3b", attached, "%PDF-1.7", []string{"--pdfa", "3b"}, "3b"},
		{"attached-stripped", attached, "%PDF-2.0", []string{"--pdfa", "4", "--strip", "attachments"}, "4"},
	} {
		out := filepath.Join(dir, c.name+".pdf")
		var messages bytes.Buffer
		if err := run(t.Context(), append(c.args, "-o", out, c.input), io.Discard, &messages); err != nil {
			t.Fatalf("%s: %v\n%s", c.name, err, &messages)
		}
		if !bytes.HasPrefix(readFile(t, out), []byte(c.header)) {
			t.Fatalf("%s: header is not %s", c.name, c.header)
		}
		if flavour, err := declaredFlavour(t.Context(), out); err != nil || flavour != c.want {
			t.Fatalf("%s: flavour %q, %v", c.name, flavour, err)
		}
	}
	// PDF/A-2 refuses attachments it cannot prove to be PDF/A.
	var messages bytes.Buffer
	if err := run(t.Context(), []string{"--pdfa", "2b", "-o", filepath.Join(dir, "refused.pdf"), attached}, io.Discard, &messages); err == nil || !strings.Contains(messages.String(), "--pdfa 3b") {
		t.Fatalf("attachments accepted for PDF/A-2b: %v\n%s", err, &messages)
	}
	// A batch shares one validator run and publishes every compliant result.
	batch := filepath.Join(dir, "batch")
	if err := os.Mkdir(batch, 0700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"a.pdf", "b.pdf", "c.pdf"} {
		writeFile(t, filepath.Join(batch, name), testPDF(0))
	}
	outDir := filepath.Join(dir, "batch-out")
	if err := os.Mkdir(outDir, 0700); err != nil {
		t.Fatal(err)
	}
	messages.Reset()
	if err := run(t.Context(), []string{"--pdfa", "4", "-o", outDir, batch}, io.Discard, &messages); err != nil {
		t.Fatalf("batch: %v\n%s", err, &messages)
	}
	for _, name := range []string{"a.squeezed.pdf", "b.squeezed.pdf", "c.squeezed.pdf"} {
		if flavour, err := declaredFlavour(t.Context(), filepath.Join(outDir, name)); err != nil || flavour != "4" {
			t.Fatalf("%s: flavour %q, %v", name, flavour, err)
		}
	}
	cmyk := filepath.Join(dir, "cmyk.pdf")
	content := "0 0 0 1 k 10 10 50 50 re f\n"
	writeFile(t, cmyk, buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content)))
	failed := filepath.Join(dir, "cmyk-a4.pdf")
	messages.Reset()
	err := run(t.Context(), []string{"--pdfa", "4", "-o", failed, cmyk}, io.Discard, &messages)
	if err == nil || !strings.Contains(messages.String(), "--output-intent") {
		t.Fatalf("DeviceCMYK without a CMYK intent: %v\n%s", err, &messages)
	}
	assertMissing(t, failed)
	assertNoTemps(t, dir)
	// A declared PDF/A input that does not conform is published with a warning.
	declared := filepath.Join(dir, "declared.pdf")
	writeFile(t, declared, pdfaPDF("2", "B"))
	messages.Reset()
	if err := run(t.Context(), []string{"-o", filepath.Join(dir, "declared-out.pdf"), declared}, io.Discard, &messages); err != nil {
		t.Fatalf("declared input: %v\n%s", err, &messages)
	}
	if !strings.Contains(messages.String(), "does not conform") {
		t.Fatalf("expected a warning about the input:\n%s", &messages)
	}
}

func TestAdvancedCLI(t *testing.T) {
	if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
		t.Skip("optional tool integration")
	}
	requirePythonTools(t)
	dir := t.TempDir()
	input := filepath.Join(dir, "in.pdf")
	writeFile(t, input, imagePDF(8, "DeviceRGB", "", bytes.Repeat([]byte{30, 80, 140}, 4096)))
	for i, flags := range [][]string{{"--dpi", "36"}, {"--gray"}, {"--bitmap"}, {"--mrc"}, {"--subset-fonts"}, {"--convert-fonts-cff"}} {
		output := filepath.Join(dir, fmt.Sprintf("out-%d.pdf", i))
		var messages bytes.Buffer
		if err := run(t.Context(), append(flags, "-o", output, input), io.Discard, &messages); err != nil {
			t.Fatalf("%v: %v\n%s", flags, err, &messages)
		}
		readContext(t, output, "")
	}
	textInput, textOutput := filepath.Join(dir, "text.pdf"), filepath.Join(dir, "text.txt")
	writeFile(t, textInput, testPDF(0))
	if err := run(t.Context(), []string{"--extract", "text", "-o", textOutput, textInput}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(readFile(t, textOutput), []byte("Squeeze this PDF.")) {
		t.Fatal("text extraction failed")
	}
	assertNoTemps(t, dir)
}

func TestBoundedDiagnosticsAndLateImageError(t *testing.T) {
	var output boundedBuffer
	_, err := io.Copy(&output, contextReader{t.Context(), strings.NewReader(strings.Repeat("x", 2<<20))})
	if err != nil || len(output.Bytes()) != 1<<20 || !output.truncated {
		t.Fatal("diagnostic limit was bypassed")
	}
	output.Write([]byte("recoverable image "))
	output.Write([]byte("error"))
	if !output.imageError {
		t.Fatal("image error after truncation was lost")
	}
}

func TestPasswordFileReuseAndMetadataDates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "password.txt")
	writeFile(t, path, []byte("fictional-example-password\n"))
	if _, _, _, err := parseOptions(t.Context(), []string{"--encrypt-user-file", path, "--encrypt-owner-file", path, "input.pdf"}, io.Discard); err == nil {
		t.Fatal("same file bypassed distinct-password requirement")
	}
	if _, _, _, err := parseOptions(t.Context(), []string{"--metadata", "CreationDate=D:20261301120000Z", "input.pdf"}, io.Discard); err == nil {
		t.Fatal("invalid calendar date accepted")
	}
	opts, _, _, err := parseOptions(t.Context(), []string{"--password-file", path, "--encrypt-owner-file", path, "input.pdf"}, io.Discard)
	if err != nil || opts.password != "fictional-example-password" || opts.encryptOwner != opts.password {
		t.Fatalf("shared file not read for both uses: %v", err)
	}
}

func TestLosslessKeepsRGBColorSpace(t *testing.T) {
	samples := bytes.Repeat([]byte{100, 100, 100}, 4096)
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, imagePDF(8, "DeviceRGB", "", samples))
	if err := run(t.Context(), []string{"--lossless", "--force-recompression", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	sd := firstImage(t, readContext(t, output, ""))
	if sd.NameEntry("ColorSpace") == nil || *sd.NameEntry("ColorSpace") != "DeviceRGB" {
		t.Fatal("lossless mode changed color semantics")
	}
	if err := sd.Decode(); err != nil || !bytes.Equal(sd.Content, samples) {
		t.Fatalf("samples changed: %v", err)
	}
}

func TestStripDoesNotRemoveNamedResources(t *testing.T) {
	for _, name := range []string{"B", "Metadata"} {
		t.Run(name, func(t *testing.T) {
			content := "q 64 0 0 64 0 0 cm /" + name + " Do Q"
			pdf := buildPDF(0,
				"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
				"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject 6 0 R >> /Contents 4 0 R >>",
				fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
				"<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /BitsPerComponent 8 /ColorSpace /DeviceGray /Length 4096 >>\nstream\n"+strings.Repeat("x", 4096)+"\nendstream",
				"<< /"+name+" 5 0 R >>")
			dir := t.TempDir()
			input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
			writeFile(t, input, pdf)
			if err := run(t.Context(), []string{"--strip", "threads,metadata", "-o", output, input}, io.Discard, io.Discard); err != nil {
				t.Fatal(err)
			}
			firstImage(t, readContext(t, output, ""))
		})
	}
}

func TestMonochromeCLIUsesRequestedCodec(t *testing.T) {
	if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
		t.Skip("optional tool integration")
	}
	dir := t.TempDir()
	input := filepath.Join(dir, "in.pdf")
	writeFile(t, input, imagePDF(8, "DeviceGray", "", bytes.Repeat([]byte{0, 255}, 2048)))
	for _, codec := range []string{"ccitt", "jbig2"} {
		output := filepath.Join(dir, codec+".pdf")
		var messages bytes.Buffer
		if err := run(t.Context(), []string{"--images", "--force-recompression", "--mono-codecs", codec, "-o", output, input}, io.Discard, &messages); err != nil {
			t.Fatalf("%s: %v\n%s", codec, err, &messages)
		}
		sd := firstImage(t, readContext(t, output, ""))
		filter := "CCITTFaxDecode"
		if codec == "jbig2" {
			filter = "JBIG2Decode"
		}
		if sd.NameEntry("Filter") == nil || *sd.NameEntry("Filter") != filter {
			t.Fatalf("requested %s, got %v", codec, sd.NameEntry("Filter"))
		}
	}
}
