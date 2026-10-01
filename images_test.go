package main

import (
	"bytes"
	"compress/zlib"
	"fmt"
	"image"
	"image/jpeg"
	"io"
	"path/filepath"
	"strings"
	"testing"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

// blocks fills w*h pixels with 4x4 blocks of pseudo-random levels: cheap for Flate with
// predictors, expensive for JPEG, and not trivially repetitive for plain Flate.
func blocks(w, h int, levels []byte, seed uint32) []byte {
	out := make([]byte, w*h)
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			n := (uint32(x/4)+seed)*2654435761 + uint32(y/4)*40503
			out[y*w+x] = levels[(n>>7)%uint32(len(levels))]
		}
	}
	return out
}

func deflate(t *testing.T, data []byte) []byte {
	t.Helper()
	var buf bytes.Buffer
	w := zlib.NewWriter(&buf)
	if _, err := w.Write(data); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func TestImages(t *testing.T) {
	gray := blocks(512, 512, []byte{0, 128, 255}, 1)
	rgb := make([]byte, 0, 3*len(gray))
	for _, v := range gray {
		rgb = append(rgb, v, v, v)
	}
	// Three distinct black-and-white images: pdfcpu would merge identical streams into one object.
	bilevel, masked, mask := blocks(512, 256, []byte{0, 255}, 2), blocks(512, 256, []byte{0, 255}, 3), blocks(512, 256, []byte{0, 255}, 4)
	noise := image.NewRGBA(image.Rect(0, 0, 64, 64))
	for i := range noise.Pix {
		noise.Pix[i] = byte(uint32(i)*2654435761>>13) | 0x01
	}
	var photo bytes.Buffer
	if err := jpeg.Encode(&photo, noise, &jpeg.Options{Quality: 50}); err != nil {
		t.Fatal(err)
	}
	stream := func(name, dict string, data []byte) string {
		return fmt.Sprintf("<< /Type /XObject /Subtype /Image /Name /%s %s /Length %d >>\nstream\n%s\nendstream", name, dict, len(data), data)
	}
	content := "" // pdfcpu drops XObjects the page never draws
	for i, name := range []string{"A", "B", "C", "D", "E", "F"} {
		content += fmt.Sprintf("q 30 0 0 30 %d 0 cm /%s Do Q\n", 32*i, name)
	}
	pdf := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R"+
			" /Resources << /XObject << /A 5 0 R /B 6 0 R /C 7 0 R /D 8 0 R /E 9 0 R /F 10 0 R >> >> >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		stream("grayrgb", "/Width 512 /Height 512 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode", deflate(t, rgb)),
		stream("bilevel", "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode", deflate(t, bilevel)),
		stream("photo", "/Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode", photo.Bytes()),
		stream("mask", "/Width 8 /Height 8 /ImageMask true /BitsPerComponent 1", bytes.Repeat([]byte{0xAA}, 8)),
		stream("indexed", "/Width 16 /Height 16 /ColorSpace [/Indexed /DeviceRGB 1 <000000ffffff>] /BitsPerComponent 8", bytes.Repeat([]byte{0, 1}, 128)),
		stream("softmasked", "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /SMask 11 0 R", deflate(t, masked)),
		stream("softmask", "/Width 512 /Height 256 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode", deflate(t, mask)),
	)
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, pdf)
	var stdout, stderr bytes.Buffer
	if err := run(t.Context(), []string{"--images", "-V", "-o", output, input}, &stdout, &stderr); err != nil {
		t.Fatalf("%v\n%s", err, &stderr)
	}
	if !strings.Contains(stderr.String(), "images: 7 seen, ") || !strings.Contains(stderr.String(), "preserved: color space Indexed 1, image mask 1, no gain") {
		t.Fatalf("unexpected image stats:\n%s", &stderr)
	}
	result, err := api.ReadContext(t.Context(), bytes.NewReader(readFile(t, output)), model.NewStatelessConfiguration())
	if err != nil {
		t.Fatal(err)
	}
	images := map[string]types.StreamDict{}
	for _, entry := range result.Table {
		if sd, ok := imageStream(entry); ok {
			images[*sd.NameEntry("Name")] = sd
		}
	}
	if len(images) != 7 {
		t.Fatalf("found %d images, want 7", len(images))
	}
	decoded := func(name string) types.StreamDict {
		sd := images[name]
		if err := sd.Decode(); err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		return sd
	}
	if sd := decoded("grayrgb"); *sd.NameEntry("ColorSpace") != "DeviceGray" || *sd.IntEntry("BitsPerComponent") != 8 || !bytes.Equal(sd.Content, gray) {
		t.Errorf("grayrgb: want lossless DeviceGray, got %s", sd.Dict)
	}
	packed := make([]byte, 512/8*256)
	for i, v := range bilevel {
		if v != 0 {
			packed[i/8] |= 0x80 >> (i % 8)
		}
	}
	if sd := decoded("bilevel"); *sd.IntEntry("BitsPerComponent") != 1 || !bytes.Equal(sd.Content, packed) {
		t.Errorf("bilevel: want 1-bit with the same pixels, got %s", sd.Dict)
	}
	if sd := decoded("softmask"); *sd.IntEntry("BitsPerComponent") != 8 || !bytes.Equal(sd.Content, mask) {
		t.Errorf("softmask: soft masks must stay 8-bit, got %s", sd.Dict)
	}
	if sd := decoded("softmasked"); *sd.IntEntry("BitsPerComponent") != 1 || sd.IndirectRefEntry("SMask") == nil {
		t.Errorf("softmasked: want 1-bit with its SMask kept, got %s", sd.Dict)
	}
	for name, raw := range map[string][]byte{"photo": photo.Bytes(), "mask": bytes.Repeat([]byte{0xAA}, 8), "indexed": bytes.Repeat([]byte{0, 1}, 128)} {
		if !bytes.Equal(images[name].Raw, raw) {
			t.Errorf("%s: bytes changed", name)
		}
	}
	if len(images["grayrgb"].Raw)+len(images["bilevel"].Raw) >= len(deflate(t, rgb))+len(deflate(t, bilevel)) {
		t.Error("re-encoded images are not smaller")
	}
}

func TestImagesFlagNeedsPdfcpu(t *testing.T) {
	for _, args := range [][]string{
		{"--images", "--engine", "ghostscript", "in.pdf"},
		{"--dpi", "72", "in.pdf"}, {"--gray", "in.pdf"}, {"--engine", "ghostscript", "--dpi", "-1", "in.pdf"},
	} {
		if err := run(t.Context(), args, io.Discard, io.Discard); err == nil {
			t.Errorf("accepted %q", args)
		}
	}
}
