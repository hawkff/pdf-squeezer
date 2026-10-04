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
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

func TestEngines(t *testing.T) {
	for _, engine := range []string{"pdfcpu", "ghostscript"} {
		for _, verbose := range []bool{false, true} {
			t.Run(fmt.Sprintf("%s/verbose=%t", engine, verbose), func(t *testing.T) {
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
				if verbose {
					args = append(args, "-V")
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
				if verbose {
					if !strings.Contains(stderr.String(), "input: ") || !strings.Contains(stderr.String(), engine+": ") {
						t.Fatalf("missing verbose steps: %s", &stderr)
					}
				} else if stderr.Len() != 0 {
					t.Fatalf("unexpected stderr: %s", &stderr)
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

func TestPrivacy(t *testing.T) {
	for _, engine := range []string{"pdfcpu", "ghostscript"} {
		t.Run(engine, func(t *testing.T) {
			if engine == "ghostscript" {
				requireTool(t, "gs")
			}
			dir := t.TempDir()
			input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
			writeFile(t, input, metadataPDF())
			var stdout bytes.Buffer
			if err := run(t.Context(), []string{"--engine", engine, "--privacy", "-o", output, input}, &stdout, io.Discard); err != nil {
				t.Fatal(err)
			}
			if strings.Contains(stdout.String(), "copied the original") {
				t.Fatalf("copied the original with its metadata: %s", &stdout)
			}
			result := readFile(t, output)
			for _, secret := range []string{"Secret Report", "Jane Example", "Example Writer", "20260101", "xpacket", "Trace"} {
				if bytes.Contains(result, []byte(secret)) {
					t.Errorf("%q remains in the output", secret)
				}
			}
			pdf, err := api.ReadContext(t.Context(), bytes.NewReader(result), model.NewStatelessConfiguration())
			if err != nil {
				t.Fatal(err)
			}
			if pdf.Info != nil {
				if d, err := pdf.DereferenceDict(*pdf.Info); err != nil || len(d) != 0 {
					t.Errorf("info dict remains: %v, err = %v", d, err)
				}
			}
			for objNr, entry := range pdf.Table {
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
				if typ := d.Type(); typ != nil && *typ == "Metadata" {
					t.Errorf("object %d is a metadata stream", objNr)
				}
				for _, key := range []string{"Metadata", "PieceInfo", "LastModified"} {
					if _, found := d.Find(key); found {
						t.Errorf("object %d keeps %s", objNr, key)
					}
				}
			}
		})
	}
}

func TestMetadataStreamTypeRemoval(t *testing.T) {
	for _, typeEntry := range []string{"/Type /Metadata", "/Type 8 0 R", ""} {
		for _, flags := range [][]string{{"--privacy"}, {"--strip", "metadata"}, {"--metadata", "Title=Updated"}, {"--timestamps", "now"}, {"--timestamps", "modified"}} {
			t.Run(typeEntry+"/"+strings.Join(flags, " "), func(t *testing.T) {
				dir := t.TempDir()
				input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
				writeFile(t, input, metadataPDFWithType(typeEntry))
				if err := run(t.Context(), append(flags, "-o", output, input), io.Discard, io.Discard); err != nil {
					t.Fatal(err)
				}
				pdf := readContext(t, output, "")
				for n, entry := range pdf.Table {
					if entry == nil || entry.Free {
						continue
					}
					switch o := entry.Object.(type) {
					case types.Dict:
						if _, found := o["Metadata"]; found {
							t.Errorf("object %d retains XMP reference", n)
						}
					case types.StreamDict:
						if err := o.Decode(); err == nil && bytes.Contains(o.Content, []byte("xpacket")) {
							t.Errorf("object %d retains identifying XMP bytes", n)
						}
					}
				}
			})
		}
	}
}

func TestMetadataOnUntypedAnnotation(t *testing.T) {
	for _, subtype := range []string{"/XML", "6 0 R", "/XMP"} {
		for _, flags := range [][]string{{"--privacy"}, {"--strip", "metadata"}, {"--metadata", "Title=Updated"}, {"--timestamps", "now"}, {"--timestamps", "modified"}} {
			t.Run(subtype+"/"+strings.Join(flags, " "), func(t *testing.T) {
				const xmp = "<private>Identifying annotation metadata</private>"
				dir := t.TempDir()
				input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
				writeFile(t, input, buildPDF(0,
					"<< /Type /Catalog /Pages 2 0 R >>",
					"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
					"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Annots [4 0 R] >>",
					"<< /Subtype /Text /Rect [0 0 10 10] /Contents (Note) /Metadata 5 0 R >>",
					fmt.Sprintf("<< /Subtype %s /Length %d >>\nstream\n%s\nendstream", subtype, len(xmp), xmp),
					"/XML"))
				if err := run(t.Context(), append(flags, "-o", output, input), io.Discard, io.Discard); err != nil {
					t.Fatal(err)
				}
				pdf := readContext(t, output, "")
				foundAnnotation := false
				for _, entry := range pdf.Table {
					if entry == nil || entry.Free {
						continue
					}
					switch o := entry.Object.(type) {
					case types.Dict:
						if o["Subtype"] == types.Name("Text") {
							foundAnnotation = true
							if _, found := o["Metadata"]; found {
								t.Error("untyped annotation retains XMP reference")
							}
						}
					case types.StreamDict:
						if err := o.Decode(); err == nil && bytes.Contains(o.Content, []byte(xmp)) {
							t.Error("identifying annotation XMP remains")
						}
					}
				}
				if !foundAnnotation {
					t.Fatal("annotation removed instead of its metadata")
				}
			})
		}
	}
}

func TestPieceInfoPreservation(t *testing.T) {
	for _, flags := range [][]string{nil, {"--compression", "light"}, {"--compression", "balanced"}, {"--compression", "medium"}, {"--compression", "strong"}, {"--compression", "heavy"}, {"--mono-codecs", "ccitt"}, {"--privacy"}, {"--strip", "piece-info"}} {
		t.Run(strings.Join(flags, " "), func(t *testing.T) {
			if len(flags) == 2 && listContains("medium,strong,heavy,ccitt", flags[1]) && os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
				t.Skip("optional tool integration")
			}
			dir := t.TempDir()
			input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
			original := pieceInfoPDF()
			writeFile(t, input, original)
			if err := run(t.Context(), append(flags, "-o", output, input), io.Discard, io.Discard); err != nil {
				t.Fatal(err)
			}
			if bytes.Equal(readFile(t, output), original) {
				t.Fatal("size fallback hid the optimization result")
			}
			if os.Getenv("PDF_SQUEEZER_INTEGRATION") == "1" {
				if messages, err := exec.Command("qpdf", "--check", output).CombinedOutput(); err != nil {
					t.Fatalf("qpdf check: %v\n%s", err, messages)
				}
			}
			pdf := readContext(t, output, "")
			page, err := pdf.DereferenceDict(pdf.RootDict["TestPage"])
			if err != nil {
				t.Fatal(err)
			}
			form, _, err := pdf.DereferenceStreamDict(pdf.RootDict["TestForm"])
			if err != nil || form == nil {
				t.Fatalf("form missing: %v", err)
			}
			stripped := len(flags) > 0 && (flags[0] == "--privacy" || flags[0] == "--strip")
			for i, owner := range []types.Dict{pdf.RootDict, page, form.Dict} {
				if stripped {
					if _, found := owner["PieceInfo"]; found {
						t.Errorf("owner %d retains PieceInfo", i)
					}
					continue
				}
				piece, err := pdf.DereferenceDict(owner["PieceInfo"])
				if err != nil || piece == nil {
					t.Fatalf("owner %d lost PieceInfo: %v", i, err)
				}
				app, err := pdf.DereferenceDict(piece["ExampleApp"])
				if err != nil || app == nil {
					t.Fatalf("owner %d lost application data: %v", i, err)
				}
				private, _, err := pdf.DereferenceStreamDict(app["Private"])
				if err != nil || private == nil {
					t.Fatalf("owner %d lost private stream %v: %v", i, app["Private"], err)
				}
				if err := private.Decode(); err != nil || string(private.Content) != fmt.Sprintf("private-%d", i) {
					t.Fatalf("owner %d private bytes changed: %v", i, err)
				}
				if private.Dict["Shared"] != pdf.RootDict["PrivateAlias"] {
					t.Fatal("private stream graph lost its shared reference")
				}
			}
			shared, _, err := pdf.DereferenceStreamDict(pdf.RootDict["PrivateAlias"])
			if err != nil || shared == nil {
				t.Fatalf("shared stream freed: %v", err)
			}
			if err := shared.Decode(); err != nil || string(shared.Content) != "shared-private" {
				t.Fatalf("shared private bytes changed: %v", err)
			}
			if stripped {
				for _, entry := range pdf.Table {
					if entry == nil || entry.Free {
						continue
					}
					if sd, ok := entry.Object.(types.StreamDict); ok {
						if err := sd.Decode(); err == nil && bytes.Contains(sd.Content, []byte("private-")) {
							t.Fatal("stripped private stream remains reachable")
						}
					}
				}
			}
		})
	}
}

func TestPieceInfoEncryption(t *testing.T) {
	const marker = "0123456789abcdef0123456789abcdef"
	const payload = "synthetic private child stream"
	content, form := "/Fm Do\n", "q Q\n"
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "encrypted.pdf")
	owner, user := filepath.Join(dir, "owner.txt"), filepath.Join(dir, "user.txt")
	writeFile(t, owner, []byte("example-owner\n"))
	writeFile(t, user, []byte("example-reader\n"))
	writeFile(t, input, buildPDF(64000,
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Subtype /Form /BBox [0 0 64 64] /Resources << >> /PieceInfo 6 0 R /LastModified 9 0 R /Length %d >>\nstream\n%sendstream", len(form), form),
		fmt.Sprintf("<< /App << /LastModified 9 0 R /Private (%s) /Payload 7 0 R >> >>", marker),
		"<< /Stream 8 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%s\nendstream", len(payload), payload),
		"(D:20260101120000Z)"))
	if err := run(t.Context(), []string{"--encrypt-user-file", user, "--encrypt-owner-file", owner, "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	for _, plaintext := range []string{marker, payload} {
		if bytes.Contains(readFile(t, output), []byte(plaintext)) {
			t.Errorf("encrypted output exposes synthetic plaintext %q", plaintext)
		}
	}
	pdf := readContext(t, output, "example-owner")
	if pdf.Encrypt == nil || !pdf.AES4Strings || len(pdf.EncKey) != 32 {
		t.Fatal("expected AES-256 encryption")
	}
	if os.Getenv("PDF_SQUEEZER_INTEGRATION") == "1" {
		decrypted := filepath.Join(dir, "decrypted.pdf")
		if messages, err := exec.Command("qpdf", "--password-file="+owner, "--decrypt", output, decrypted).CombinedOutput(); err != nil {
			t.Fatalf("independent decryption: %v\n%s", err, messages)
		}
		pdf = readContext(t, decrypted, "")
	}
	forms := pageFormResources(t, pdf)
	sd, _, err := pdf.DereferenceStreamDict(forms["Fm"])
	if err != nil || sd == nil {
		t.Fatalf("form missing: %v", err)
	}
	piece, err := pdf.DereferenceDict(sd.Dict["PieceInfo"])
	if err != nil {
		t.Fatal(err)
	}
	app, err := pdf.DereferenceDict(piece["App"])
	if err != nil {
		t.Fatal(err)
	}
	if value, err := pdf.DereferenceText(app["Private"]); err != nil || value != marker {
		t.Errorf("decrypted private value = %q: %v", value, err)
	}
	if value, err := pdf.DereferenceText(app["LastModified"]); err != nil || value != "D:20260101120000Z" {
		t.Errorf("decrypted private date = %q: %v", value, err)
	}
	private, err := pdf.DereferenceDict(app["Payload"])
	if err != nil {
		t.Fatal(err)
	}
	child, _, err := pdf.DereferenceStreamDict(private["Stream"])
	if err != nil || child == nil {
		t.Fatalf("decrypted private child stream missing: %v", err)
	}
	if err := child.Decode(); err != nil || string(child.Content) != payload {
		t.Fatalf("decrypted private child bytes changed: %v", err)
	}
}

func TestPieceInfoDistinctFormBindings(t *testing.T) {
	content, form := "/A Do /B Do\n", "q Q\n"
	objects := []string{
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
	}
	for _, value := range []string{"private-A", "private-B"} {
		objects = append(objects, fmt.Sprintf("<< /Subtype /Form /BBox [0 0 64 64] /Resources << >> /LastModified (D:20260101120000Z) /PieceInfo << /App << /LastModified (D:20260101120000Z) /Private (%s) >> >> /pdfSqueezerPieceInfo 99 /pdfSqueezerPieceInfo_ 88 /Length %d >>\nstream\n%sendstream", value, len(form), form))
	}
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, buildPDF(64000, objects...))
	if err := run(t.Context(), []string{"-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	pdf := readContext(t, output, "")
	forms := pageFormResources(t, pdf)
	if forms["A"] == forms["B"] {
		t.Error("distinct private data merged into one resource binding")
	}
	for _, name := range []string{"A", "B"} {
		sd, _, err := pdf.DereferenceStreamDict(forms[name])
		if err != nil || sd == nil {
			t.Fatalf("form %s missing: %v", name, err)
		}
		piece, err := pdf.DereferenceDict(sd.Dict["PieceInfo"])
		if err != nil {
			t.Fatal(err)
		}
		app, err := pdf.DereferenceDict(piece["App"])
		if err != nil {
			t.Fatal(err)
		}
		if value, err := pdf.DereferenceText(app["Private"]); err != nil || value != "private-"+name {
			t.Errorf("resource %s private value = %q: %v", name, value, err)
		}
		if sd.Dict["pdfSqueezerPieceInfo"] != types.Integer(99) || sd.Dict["pdfSqueezerPieceInfo_"] != types.Integer(88) {
			t.Error("existing form extension entries changed")
		}
		if _, found := sd.Dict["pdfSqueezerPieceInfo__"]; found {
			t.Error("temporary form identity escaped into the output")
		}
	}
}

func TestPieceInfoStripSharedLazyGraph(t *testing.T) {
	requireTool(t, "qpdf")
	content, form := "/Fm Do\n", "q Q\n"
	const payload = "shared lazy dictionary child"
	const marker = "0123456789abcdef0123456789abcdef"
	for _, flags := range [][]string{{"--strip", "piece-info"}, {"--privacy"}} {
		for _, rewrite := range []string{"native", "encrypt", "monochrome"} {
			t.Run(strings.Join(flags, " ")+"/"+rewrite, func(t *testing.T) {
				if rewrite == "monochrome" && os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
					t.Skip("optional tool integration")
				}
				dir := t.TempDir()
				plain, input, output := filepath.Join(dir, "plain.pdf"), filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
				writeFile(t, plain, buildPDF(0,
					"<< /Type /Catalog /Pages 2 0 R /PieceInfo 6 0 R >>",
					"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
					"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R >>",
					fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
					fmt.Sprintf("<< /Subtype /Form /BBox [0 0 64 64] /Resources << >> /SharedPrivate 7 0 R /Length %d >>\nstream\n%sendstream", len(form), form),
					"<< /App << /LastModified (D:20260101120000Z) /Private 7 0 R >> >>",
					fmt.Sprintf("<< /Value (%s) /Payload 8 0 R >>", marker),
					fmt.Sprintf("<< /Length %d >>\nstream\n%s\nendstream", len(payload), payload)))
				if messages, err := exec.Command("qpdf", "--object-streams=generate", plain, input).CombinedOutput(); err != nil {
					t.Fatalf("prepare object streams: %v\n%s", err, messages)
				}
				before := readContext(t, input, "")
				forms := pageFormResources(t, before)
				sd, _, err := before.DereferenceStreamDict(forms["Fm"])
				if err != nil || sd == nil {
					t.Fatalf("form missing: %v", err)
				}
				shared := sd.Dict["SharedPrivate"].(types.IndirectRef)
				if _, lazy := before.Table[shared.ObjectNumber.Value()].Object.(types.LazyObjectStreamObject); !lazy {
					t.Fatal("fixture did not retain a lazy private dictionary")
				}
				args := append([]string{}, flags...)
				password := ""
				if rewrite == "encrypt" {
					password = "example-owner"
					owner := filepath.Join(dir, "owner.txt")
					writeFile(t, owner, []byte(password+"\n"))
					args = append(args, "--encrypt-owner-file", owner)
				}
				if rewrite == "monochrome" {
					args = append(args, "--mono-codecs", "ccitt")
				}
				if err := run(t.Context(), append(args, "-o", output, input), io.Discard, io.Discard); err != nil {
					t.Fatal(err)
				}
				if rewrite == "encrypt" {
					for _, plaintext := range []string{marker, payload} {
						if bytes.Contains(readFile(t, output), []byte(plaintext)) {
							t.Errorf("encrypted shared graph exposes synthetic plaintext %q", plaintext)
						}
					}
				}
				pdf := readContext(t, output, password)
				if rewrite == "encrypt" && pdf.Encrypt == nil {
					t.Fatal("shared graph output is not encrypted")
				}
				if _, found := pdf.RootDict["PieceInfo"]; found {
					t.Fatal("PieceInfo was not stripped")
				}
				forms = pageFormResources(t, pdf)
				sd, _, err = pdf.DereferenceStreamDict(forms["Fm"])
				if err != nil || sd == nil {
					t.Fatalf("form missing: %v", err)
				}
				private, err := pdf.DereferenceDict(sd.Dict["SharedPrivate"])
				if err != nil {
					t.Fatal(err)
				}
				if value, err := pdf.DereferenceText(private["Value"]); err != nil || value != marker {
					t.Errorf("shared private value = %q: %v", value, err)
				}
				child, _, err := pdf.DereferenceStreamDict(private["Payload"])
				if err != nil || child == nil {
					t.Fatalf("shared private child lost after stripping: %v", err)
				}
				if err := child.Decode(); err != nil || string(child.Content) != payload {
					t.Fatalf("shared private child bytes changed: %v", err)
				}
			})
		}
	}
}

func pageFormResources(t *testing.T, pdf *model.Context) types.Dict {
	t.Helper()
	pages, err := pdf.DereferenceDict(pdf.RootDict["Pages"])
	if err != nil {
		t.Fatal(err)
	}
	kids, err := pdf.DereferenceArray(pages["Kids"])
	if err != nil || len(kids) != 1 {
		t.Fatalf("expected one page: %v", err)
	}
	page, err := pdf.DereferenceDict(kids[0])
	if err != nil {
		t.Fatal(err)
	}
	resources, err := pdf.DereferenceDict(page["Resources"])
	if err != nil {
		t.Fatal(err)
	}
	forms, err := pdf.DereferenceDict(resources["XObject"])
	if err != nil || forms == nil {
		t.Fatalf("XObject resources missing: %v", err)
	}
	return forms
}

func pieceInfoPDF() []byte {
	content, form := "/Fm Do\n", "q Q\n"
	objects := []string{
		"<< /Type /Catalog /Pages 2 0 R /PieceInfo 6 0 R /TestPage 3 0 R /TestForm 5 0 R /PrivateAlias 12 0 R /Names << /EmbeddedFiles << /Names [(private.bin) 13 0 R] >> >> >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Fm 5 0 R >> >> /Contents 4 0 R /PieceInfo 7 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Subtype /Form /BBox [0 0 64 64] /Resources << >> /PieceInfo 8 0 R /Length %d >>\nstream\n%sendstream", len(form), form),
	}
	for i := range 3 {
		objects = append(objects, fmt.Sprintf("<< /ExampleApp << /LastModified (D:20260101120000Z) /Private %d 0 R >> >>", 9+i))
	}
	for i := range 3 {
		data := fmt.Sprintf("private-%d", i)
		objects = append(objects, fmt.Sprintf("<< /Shared 12 0 R /Back 6 0 R /Length %d >>\nstream\n%s\nendstream", len(data), data))
	}
	objects = append(objects, "<< /Type /EmbeddedFile /Length 14 >>\nstream\nshared-private\nendstream",
		"<< /Type /Filespec /F (private.bin) /EF << /F 12 0 R >> >>")
	return buildPDF(64000, objects...)
}

func TestRejectsInvalidArguments(t *testing.T) {
	for _, args := range [][]string{
		{}, {"one.pdf", "two.pdf"}, {"--engine", "other", "input.pdf"},
		{"--quality", "screen", "input.pdf"},
		{"--engine", "ghostscript", "--quality", "other", "input.pdf"}, {"--unknown"},
	} {
		if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
			t.Errorf("accepted %q", args)
		}
	}
	for _, arg := range []string{"-h", "--help"} {
		if err := run(t.Context(), []string{arg}, io.Discard, io.Discard); !errors.Is(err, flag.ErrHelp) {
			t.Fatalf("%s: %v", arg, err)
		}
	}
}

func TestTrailingFlagsAndOutputDir(t *testing.T) {
	dir, outDir := t.TempDir(), t.TempDir()
	input := filepath.Join(dir, "doc.pdf")
	writeFile(t, input, testPDF(1000))
	var stdout, stderr bytes.Buffer
	if err := run(t.Context(), []string{input, "-o", outDir, "-V"}, &stdout, &stderr); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(outDir, "doc.squeezed.pdf")
	if !strings.HasPrefix(stdout.String(), output+": ") || !strings.Contains(stderr.String(), "input: ") {
		t.Fatalf("stdout: %s\nstderr: %s", &stdout, &stderr)
	}
	readFile(t, output)
	if err := run(t.Context(), []string{"--", input, "-V"}, io.Discard, io.Discard); err == nil {
		t.Fatal("treated an argument after -- as a flag")
	}
}

func TestVersion(t *testing.T) {
	for _, arg := range []string{"-v", "--version"} {
		var stdout bytes.Buffer
		if err := run(t.Context(), []string{arg}, &stdout, io.Discard); err != nil {
			t.Fatal(err)
		}
		if v := strings.TrimSuffix(stdout.String(), "\n"); v == "" || strings.ContainsAny(v, " \n") {
			t.Fatalf("%s printed %q", arg, stdout.String())
		}
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

func TestGhostscriptRejectsDroppedImages(t *testing.T) {
	requireGhostscript10(t)
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, brokenImagePDF())
	err := run(t.Context(), []string{"--engine", "ghostscript", "-o", output, input}, io.Discard, io.Discard)
	if err == nil {
		t.Fatal("accepted output with a dropped image")
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
	return buildPDF(padding,
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
		"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
	)
}

// brokenImagePDF draws an image with an unknown color space. Ghostscript 10 skips it with a warning and writes a blank page.
func brokenImagePDF() []byte {
	content := "q 100 0 0 100 50 50 cm /Im1 Do Q\n"
	return buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		"<< /Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceFoo /BitsPerComponent 8 /Length 16 >>\nstream\n0123456789abcdef\nendstream",
	)
}

// metadataPDF carries metadata in every place the privacy flag must clear: the Info dict,
// a document XMP stream, and page-level XMP, piece info, and modification date.
func metadataPDF() []byte {
	return metadataPDFWithType("/Type /Metadata")
}

func metadataPDFWithType(typeEntry string) []byte {
	content := "BT /F1 12 Tf 20 100 Td (Squeeze this PDF.) Tj ET /Fm Do\n"
	xmp := `<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/">` +
		`<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" ` +
		`xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:creator><rdf:Seq><rdf:li>Jane Example</rdf:li></rdf:Seq>` +
		`</dc:creator></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>`
	pdf := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R /Metadata 7 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> /XObject << /Fm 9 0 R >> >> /Contents 5 0 R"+
			" /Metadata 7 0 R /LastModified (D:20260101120000Z) /PieceInfo << /ExampleApp << /LastModified (D:20260101120000Z) /Private (Trace) >> >> >>",
		"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		"<< /Title (Secret Report) /Author (Jane Example) /Creator (Example Writer) /Producer (Example Writer) /CreationDate (D:20260101120000Z) /ModDate (D:20260101120000Z) >>",
		fmt.Sprintf("<< %s /Subtype /XML /Length %d >>\nstream\n%s\nendstream", typeEntry, len(xmp), xmp),
		"/Metadata",
		"<< /Subtype /Form /BBox [0 0 200 200] /Resources << >> /Metadata 7 0 R /Length 4 >>\nstream\nq Q\nendstream",
	)
	// The trailer follows the xref table, so adding /Info there leaves the offsets intact.
	return bytes.Replace(pdf, []byte("/Root 1 0 R"), []byte("/Root 1 0 R /Info 6 0 R"), 1)
}

func buildPDF(padding int, objects ...string) []byte {
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

// requireGhostscript10 skips on Ghostscript 9, whose PDF interpreter stops or substitutes instead of skipping images.
func requireGhostscript10(t *testing.T) {
	t.Helper()
	requireTool(t, "gs")
	version, err := exec.Command("gs", "--version").Output()
	if err != nil {
		t.Fatal(err)
	}
	var major int
	if _, err := fmt.Sscanf(string(version), "%d", &major); err != nil {
		t.Fatalf("gs --version printed %q", version)
	}
	if major < 10 {
		if os.Getenv("PDF_SQUEEZER_INTEGRATION") == "1" {
			t.Fatalf("Ghostscript 10 or newer is required, found %s", version)
		}
		t.Skipf("Ghostscript %s does not skip images", strings.TrimSpace(string(version)))
	}
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
	paths, err := filepath.Glob(filepath.Join(dir, ".pdf-squeezer-*"))
	if err != nil || len(paths) != 0 {
		t.Fatalf("staging directories remain: %v, err = %v", paths, err)
	}
}
