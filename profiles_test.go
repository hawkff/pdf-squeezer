package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

func TestCompressionTierOptions(t *testing.T) {
	for _, tc := range []struct {
		name         string
		quality, dpi int
		bits         bool
	}{{"light", 90, 0, false}, {"balanced", 85, 0, true}, {"medium", 65, 150, true}, {"strong", 45, 120, true}, {"heavy", 30, 96, true}} {
		t.Run(tc.name, func(t *testing.T) {
			o, _, _, err := parseOptions(t.Context(), []string{"--compression", tc.name, "in.pdf"}, io.Discard)
			if err != nil || !o.images || o.imageQuality != tc.quality || o.dpi != tc.dpi || o.reduceBits != tc.bits {
				t.Fatalf("tier settings: %+v, %v", o, err)
			}
			if o.bitmap || o.mrc || o.gray || o.cffFonts || o.removeStandardFonts || o.flatten != "" || o.strip != "" || o.privacy {
				t.Fatal("tier enables document destruction")
			}
		})
	}
	o, _, _, err := parseOptions(t.Context(), []string{"--compression", "heavy", "--image-quality", "99", "--dpi", "0", "--clip-images=false", "--inline-images=false", "--merge-fonts=false", "--reduce-bit-depth=false", "--color-reduction", "preserve", "in.pdf"}, io.Discard)
	if err != nil || o.imageQuality != 99 || o.dpi != 0 || o.clip || o.inline || o.mergeFonts || o.reduceBits || o.colorReduction != "preserve" {
		t.Fatalf("explicit settings lost: %+v, %v", o, err)
	}
	for _, args := range [][]string{{"--compression", "unknown"}, {"--compression", "medium", "--lossless"}, {"--color-reduction", "unknown"}, {"--timestamps", "unknown"}, {"--output", ""}} {
		if _, _, _, err := parseOptions(t.Context(), append(args, "in.pdf"), io.Discard); err == nil {
			t.Fatalf("accepted %v", args)
		}
	}
}

func TestTierImageOptOut(t *testing.T) {
	path := filepath.Join(t.TempDir(), "disabled.json")
	writeFile(t, path, []byte(`{"version":1,"flags":{"images":"false"}}`))
	for _, level := range []string{"light", "balanced", "medium", "strong", "heavy"} {
		for _, flags := range [][]string{{"--images=false"}, {"--profile", path}} {
			args := append([]string{"--compression", level, "in.pdf"}, flags...)
			o, _, _, err := parseOptions(t.Context(), args, io.Discard)
			if err != nil || o.images || o.reduceBits || o.dpi != 0 || o.clip || o.inline {
				t.Fatalf("image opt-out %v: %+v, %v", args, o, err)
			}
		}
	}
	if _, _, _, err := parseOptions(t.Context(), []string{"--images=false", "--reduce-bit-depth", "in.pdf"}, io.Discard); err == nil {
		t.Fatal("accepted contradictory explicit image settings")
	}
}

func TestProfileExportSizeLimit(t *testing.T) {
	dir := t.TempDir()
	input, baseline := filepath.Join(dir, "input.json"), filepath.Join(dir, "baseline.json")
	write := func(path, name string) {
		t.Helper()
		data, err := json.Marshal(profile{Version: 1, Name: name, Flags: map[string]string{}})
		if err != nil {
			t.Fatal(err)
		}
		writeFile(t, path, data)
	}
	save := func(output string, paths ...string) error {
		args := []string{"--save-profile", output}
		for _, path := range paths {
			args = append(args, "--profile", path)
		}
		_, _, _, err := parseOptions(t.Context(), args, io.Discard)
		return err
	}
	write(input, "x")
	if err := save(baseline, input); err != nil {
		t.Fatal(err)
	}
	nameLength := 1 + maxProfileBytes - len(readFile(t, baseline))
	write(input, strings.Repeat("x", nameLength))
	atLimit := filepath.Join(dir, "at-limit.json")
	if err := save(atLimit, input); err != nil {
		t.Fatal(err)
	}
	if len(readFile(t, atLimit)) != maxProfileBytes {
		t.Fatal("boundary does not include final newline")
	}
	if _, err := loadProfiles(t.Context(), atLimit, io.Discard); err != nil {
		t.Fatalf("export cannot reload: %v", err)
	}
	write(input, strings.Repeat("x", nameLength+1))
	tooLarge := filepath.Join(dir, "too-large.json")
	if err := save(tooLarge, input); err == nil {
		t.Fatal("accepted oversized singleton export")
	}
	assertMissing(t, tooLarge)
	other := filepath.Join(dir, "other.json")
	write(input, strings.Repeat("a", 600000))
	write(other, strings.Repeat("b", 600000))
	if err := save(tooLarge, input, other); err == nil {
		t.Fatal("accepted oversized combined bundle")
	}
	assertMissing(t, tooLarge)
}

func TestProfileBundlesAndPrecedence(t *testing.T) {
	dir := t.TempDir()
	first, second, saved := filepath.Join(dir, "first.json"), filepath.Join(dir, "second.json"), filepath.Join(dir, "bundle.json")
	writeFile(t, first, []byte(`{"version":1,"name":"Light","flags":{"images":"true","image-quality":"80"}}`))
	writeFile(t, second, []byte(`{"version":1,"name":"Heavy","flags":{"compression":"heavy","image-quality":"70"},"metadata":["Title=Example"]}`))
	args := []string{"--profile", first, "--profile", second, "--profile-entry", "Heavy", "--image-quality", "88", "--save-profile", saved}
	o, _, _, err := parseOptions(t.Context(), args, io.Discard)
	if err != nil || o.compression != "heavy" || o.imageQuality != 88 || !o.reduceBits || len(o.metadata) != 1 {
		t.Fatalf("profile precedence: %+v, %v", o, err)
	}
	var bundle profileBundle
	if err := json.Unmarshal(readFile(t, saved), &bundle); err != nil || bundle.Version != 2 || len(bundle.Profiles) != 2 {
		t.Fatalf("invalid saved bundle: %+v, %v", bundle, err)
	}
	if bundle.Profiles[0].Flags["image-quality"] != "80" || bundle.Profiles[1].Flags["image-quality"] != "88" {
		t.Fatal("saving replaced the wrong entry")
	}
	for _, selector := range []string{"2", "Heavy"} {
		o, _, _, err := parseOptions(t.Context(), []string{"--profile", saved, "--profile-entry", selector, "in.pdf"}, io.Discard)
		if err != nil || o.imageQuality != 88 || o.compression != "heavy" {
			t.Fatalf("selection %s: %+v, %v", selector, o, err)
		}
	}
	o, _, _, err = parseOptions(t.Context(), []string{"--profile", saved, "in.pdf"}, io.Discard)
	if err != nil || o.imageQuality != 80 {
		t.Fatalf("default selection: %+v, %v", o, err)
	}
	for _, bad := range []string{
		`{"version":2,"profiles":[]}`,
		`{"version":2,"profiles":[{"version":1,"flags":{}},{"version":1,"flags":{"password-file":"example.txt"}}]}`,
		`{"version":2,"profiles":[{"version":3}]}`,
		`{"version":1,"flags":{},"unknown":true}`,
		`{"version":1,"flags":{}} {}`,
		`{"version":1,"flags":{}}` + strings.Repeat(" ", 1<<20),
	} {
		writeFile(t, saved, []byte(bad))
		if _, _, _, err := parseOptions(t.Context(), []string{"--profile", saved, "in.pdf"}, io.Discard); err == nil {
			t.Fatal("accepted invalid or unsafe profile")
		}
	}
	if _, err := selectProfile([]profile{{Name: "same"}, {Name: "same"}}, "same"); err == nil {
		t.Fatal("ambiguous profile name accepted")
	}
	if _, err := selectProfile(nil, "2"); err == nil {
		t.Fatal("selection without profiles accepted")
	}
}

func TestLegacyPropertyListProfiles(t *testing.T) {
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	requireTool(t, python)
	data := `[{"title":"Light","optimizeImages":true,"imageQuality":0.8,"imageResolution":300,"colorConversion":0,"reduceColorComplexity":false,"removeTitle":true,"customTitle":"Example","stripSpiderInfo":true,"updateModificationDate":true},{"title":"Strong","optimizeImages":true,"imageQuality":0.5,"imageResolution":144,"downsampleMonochromeScans":true,"updateModificationDate":true,"updateModificationAndCreationDate":true}]`
	for _, format := range []string{"FMT_XML", "FMT_BINARY"} {
		t.Run(format, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "settings.pdfscp")
			cmd := exec.Command(python, "-I", "-c", "import json,plistlib,sys;sys.stdout.buffer.write(plistlib.dumps(json.load(sys.stdin),fmt=plistlib."+format+"))")
			cmd.Stdin = strings.NewReader(data)
			encoded, err := cmd.Output()
			if err != nil {
				t.Fatal(err)
			}
			writeFile(t, path, encoded)
			o, _, _, err := parseOptions(t.Context(), []string{"--profile", path, "in.pdf"}, io.Discard)
			if err != nil || o.imageQuality != 80 || o.dpi != 300 || o.timestamps != "modified" || o.colorReduction != "preserve" || o.strip != "web-capture" || strings.Join(o.metadata, "") != "Title=Example" {
				t.Fatalf("first profile: %+v, %v", o, err)
			}
			o, _, _, err = parseOptions(t.Context(), []string{"--profile", path, "--profile-entry", "Strong", "--image-quality", "65", "in.pdf"}, io.Discard)
			if err != nil || o.imageQuality != 65 || o.dpi != 144 || o.monoDPI != 144 || o.timestamps != "now" {
				t.Fatalf("named profile: %+v, %v", o, err)
			}
		})
	}
	for _, data := range []string{
		`{"imageQuality":1.1}`,
		`{"imageResolution":144.5}`,
		`{"optimizeImages":false,"colorConversion":1}`,
		`{"optimizeImages":false,"optimizeResources":false}`,
		`{"optimizeImages":false,"removeProducer":true}`,
		`{"optimizeImages":false,"unknown":true}`,
		`{"optimizeImages":null}`,
		`{"optimizeImages":false,"clipImages":2}`,
	} {
		var settings map[string]json.RawMessage
		if err := json.Unmarshal([]byte(data), &settings); err != nil {
			t.Fatal(err)
		}
		if _, err := legacyProfile(settings); err == nil {
			t.Fatalf("accepted ambiguous mapping: %s", data)
		}
	}
}

func TestLegacyProfileOptionalSettings(t *testing.T) {
	for _, tc := range []struct {
		settings     string
		tier         string
		images       bool
		quality, dpi int
	}{
		{`{"removeAuthor":true,"customAuthor":"Example","stripSpiderInfo":true}`, "", false, 75, 0},
		{`{"optimizeImages":true}`, "", true, 75, 0},
		{`{"imageQuality":0.8}`, "", false, 80, 0},
		{`{"imageResolution":144}`, "", false, 75, 144},
		{`{"removeAuthor":true,"customAuthor":"Example"}`, "light", true, 90, 0},
	} {
		var settings map[string]json.RawMessage
		if err := json.Unmarshal([]byte(tc.settings), &settings); err != nil {
			t.Fatal(err)
		}
		p, err := legacyProfile(settings)
		if err != nil {
			t.Fatal(err)
		}
		data, err := json.Marshal(p)
		if err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(t.TempDir(), "settings.json")
		writeFile(t, path, data)
		args := []string{"--profile", path, "in.pdf"}
		if tc.tier != "" {
			args = append(args, "--compression", tc.tier)
		}
		o, _, _, err := parseOptions(t.Context(), args, io.Discard)
		if err != nil || o.images != tc.images || o.imageQuality != tc.quality || o.dpi != tc.dpi {
			t.Fatalf("optional settings %s: %+v, %v", tc.settings, o, err)
		}
		if _, ok := settings["removeAuthor"]; ok && strings.Join(o.metadata, "") != "Author=Example" {
			t.Fatal("metadata was lost")
		}
	}
}

func TestLegacyQualityTruncation(t *testing.T) {
	for input, want := range map[string]string{"0": "1", "0.57": "57", "0.58": "58", "0.5799": "57", "1": "100"} {
		p, err := legacyProfile(map[string]json.RawMessage{"imageQuality": json.RawMessage(input), "imageResolution": json.RawMessage("144")})
		if err != nil || p.Flags["image-quality"] != want {
			t.Fatalf("quality %s: %v, %v", input, p.Flags, err)
		}
	}
}

func TestLegacyInactiveImagesAndBinaryTrailer(t *testing.T) {
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	requireTool(t, python)
	cmd := exec.Command(python, "-I", "-c", `import plistlib, sys
for n in range(1024):
 data = plistlib.dumps([dict(title="Example", uuid="x"*n, optimizeImages=False, clipImages=True, forceRecompression=True)], fmt=plistlib.FMT_BINARY)
 if data[-1] in b" \t\r\n":
  sys.stdout.buffer.write(data)
  break
else:
 sys.exit(1)
`)
	data, err := cmd.Output()
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "inactive.pdfscp")
	writeFile(t, path, data)
	o, _, _, err := parseOptions(t.Context(), []string{"--profile", path, "in.pdf"}, io.Discard)
	if err != nil || o.images || o.clip || o.force {
		t.Fatalf("inactive settings: %+v, %v", o, err)
	}
	output := filepath.Join(t.TempDir(), "output.pdfscp")
	if _, _, _, err := parseOptions(t.Context(), []string{"--save-profile", output}, io.Discard); err == nil {
		t.Fatal("wrote JSON under a .pdfscp extension")
	}
	assertMissing(t, output)
}

func TestPerInputDestinationsPreserveArgumentOrder(t *testing.T) {
	dir := t.TempDir()
	first, second := filepath.Join(dir, "z.pdf"), filepath.Join(dir, "a.pdf")
	out1, out2 := filepath.Join(dir, "first.pdf"), filepath.Join(dir, "second.pdf")
	writeFile(t, first, bytes.Replace(testPDF(64000), []byte("Squeeze this PDF."), []byte("First file text!!"), 1))
	writeFile(t, second, bytes.Replace(testPDF(64000), []byte("Squeeze this PDF."), []byte("Other file text!!"), 1))
	if err := run(t.Context(), []string{first, second, "-o", out1, "-o", out2}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	for path, text := range map[string]string{out1: "First file text!!", out2: "Other file text!!"} {
		pdf := readContext(t, path, "")
		found := false
		for _, e := range pdf.Table {
			if e == nil || e.Free {
				continue
			}
			if sd, ok := e.Object.(types.StreamDict); ok && sd.Decode() == nil && bytes.Contains(sd.Content, []byte(text)) {
				found = true
			}
		}
		if !found {
			t.Fatalf("wrong input at %s", path)
		}
	}
	for _, inputs := range [][]string{{first, first}, {first}, {dir, second}} {
		fresh := filepath.Join(dir, "fresh.pdf")
		args := append(append([]string{}, inputs...), "-o", fresh, "-o", filepath.Join(dir, "other-fresh.pdf"))
		if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
			t.Fatal("accepted mismatched destinations")
		}
		assertMissing(t, fresh)
	}
	fresh := filepath.Join(dir, "fresh.pdf")
	if err := run(t.Context(), []string{first, second, "-o", fresh, "-o", out2}, io.Discard, io.Discard); err == nil {
		t.Fatal("accepted existing destination")
	}
	assertMissing(t, fresh)
}

func TestTargetedDocumentControls(t *testing.T) {
	pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(metadataPDF()), configuration(""))
	if err != nil {
		t.Fatal(err)
	}
	pdf.RootDict["SpiderInfo"] = types.Dict{"V": types.Integer(1)}
	if err := transformDocument(t.Context(), pdf, options{strip: "web-capture", timestamps: "preserve"}); err != nil {
		t.Fatal(err)
	}
	if _, ok := pdf.RootDict["SpiderInfo"]; ok {
		t.Fatal("web capture remains")
	}
	info, err := pdf.DereferenceDict(*pdf.Info)
	if err != nil {
		t.Fatal(err)
	}
	if title, _ := pdf.DereferenceText(info["Title"]); title != "Secret Report" {
		t.Fatal("targeted removal discarded title")
	}
	if _, ok := pdf.RootDict["Metadata"]; !ok {
		t.Fatal("targeted removal discarded XMP")
	}
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, metadataPDF())
	if err := run(t.Context(), []string{"--timestamps", "modified", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	pdf = readContext(t, output, "")
	info, err = pdf.DereferenceDict(*pdf.Info)
	if err != nil {
		t.Fatal(err)
	}
	created, _ := pdf.DereferenceText(info["CreationDate"])
	modified, _ := pdf.DereferenceText(info["ModDate"])
	if created != "D:20260101120000Z" || modified == created || !strings.HasPrefix(modified, "D:") {
		t.Fatalf("dates: %s %s", created, modified)
	}
	if _, ok := pdf.RootDict["Metadata"]; ok {
		t.Fatal("stale XMP remains")
	}
}

func TestIndependentColorReduction(t *testing.T) {
	data := bytes.Repeat([]byte{0, 0, 0, 255, 255, 255}, 2048)
	for _, mode := range []string{"exact", "preserve"} {
		dir := t.TempDir()
		input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
		writeFile(t, input, imagePDF(8, "DeviceRGB", "", data))
		if err := run(t.Context(), []string{"--images", "--force-recompression", "--color-reduction", mode, "-o", output, input}, io.Discard, io.Discard); err != nil {
			t.Fatal(err)
		}
		sd := firstImage(t, readContext(t, output, ""))
		want, bits := "DeviceGray", 1
		if mode == "preserve" {
			want, bits = "DeviceRGB", 8
		}
		if *sd.NameEntry("ColorSpace") != want || *sd.IntEntry("BitsPerComponent") != bits {
			t.Fatalf("%s changed color policy: %v", mode, sd.Dict)
		}
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if _, err := deflateSamples(ctx, []byte("sample"), true, 0); err == nil {
		t.Fatal("heavy encoding ignored cancellation")
	}
}

func TestHeavyFlateCandidatesPreserveSamples(t *testing.T) {
	for _, bits := range []int{8, 16} {
		data := make([]byte, 64*64*bits/8)
		pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(imagePDF(bits, "DeviceGray", "", data)), configuration(""))
		if err != nil {
			t.Fatal(err)
		}
		o := options{compression: "heavy", colorReduction: "preserve", imageMemory: 512, imageCodecs: "flate", force: true}
		stats, err := optimizeImages(t.Context(), pdf, o, func(string, ...any) {})
		if err != nil || stats.changed != 1 {
			t.Fatalf("heavy encoding: %v, %v", stats, err)
		}
		sd := firstImage(t, pdf)
		if err := sd.Decode(); err != nil || !bytes.Equal(sd.Content, data) || *sd.IntEntry("BitsPerComponent") != bits {
			t.Fatalf("changed %d-bit samples: %v", bits, err)
		}
	}
}

func TestCompressionTiersPreserveDocumentFeatures(t *testing.T) {
	if os.Getenv("PDF_SQUEEZER_INTEGRATION") != "1" {
		t.Skip("optional tool integration")
	}
	requirePythonTools(t)
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	dir := t.TempDir()
	input := filepath.Join(dir, "features.pdf")
	create := exec.Command(python, "-c", `import sys,pymupdf,pikepdf
with pymupdf.open() as doc:
 page=doc.new_page(width=200,height=200)
 page.insert_text((10,30),"Retain searchable text")
 shape=page.new_shape();shape.draw_rect((10,50,60,80));shape.finish(color=(1,0,0),fill=(0,0,1));shape.commit()
 page.insert_link({"kind":pymupdf.LINK_URI,"from":pymupdf.Rect(10,10,80,35),"uri":"https://example.invalid/"})
 widget=pymupdf.Widget();widget.field_name="example";widget.field_value="Keep me";widget.field_type=pymupdf.PDF_WIDGET_TYPE_TEXT;widget.rect=pymupdf.Rect(10,100,180,130);page.add_widget(widget)
 doc.save(sys.argv[1])
with pikepdf.open(sys.argv[1],allow_overwriting_input=True) as doc:
 doc.attachments["note.txt"]=pikepdf.AttachedFileSpec(doc,b"keep attachment",filename="note.txt")
 doc.save(sys.argv[1])
`, input)
	if output, err := create.CombinedOutput(); err != nil {
		t.Fatalf("create fixture: %v\n%s", err, output)
	}
	for _, level := range []string{"light", "balanced", "medium", "strong", "heavy"} {
		output := filepath.Join(dir, level+".pdf")
		var messages bytes.Buffer
		if err := run(t.Context(), []string{"--compression", level, "--force-recompression", "-o", output, input}, io.Discard, &messages); err != nil {
			t.Fatalf("%s: %v\n%s", level, err, &messages)
		}
		check := exec.Command(python, "-c", `import sys,pymupdf,pikepdf
with pymupdf.open(sys.argv[1]) as doc:
 assert len(doc)==1
 page=doc[0]
 assert "Retain searchable text" in page.get_text()
 assert len(page.get_drawings())>=1
 assert page.get_links()[0]["uri"]=="https://example.invalid/"
 assert next(page.widgets()).field_value=="Keep me"
with pikepdf.open(sys.argv[1]) as doc:
 assert doc.attachments["note.txt"].get_file().read_bytes()==b"keep attachment"
`, output)
		if result, err := check.CombinedOutput(); err != nil {
			t.Fatalf("%s lost features: %v\n%s", level, err, result)
		}
	}
}
