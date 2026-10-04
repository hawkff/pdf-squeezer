package main

import (
	"bytes"
	"compress/zlib"
	"context"
	"errors"
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

func TestGhostscriptImageFlags(t *testing.T) {
	requireTool(t, "gs")
	// 300x300 color noise drawn at 72x72 pt: 300 dpi, incompressible, nowhere gray.
	rgb := make([]byte, 0, 300*300*3)
	for i := 0; i < 300*300; i++ {
		rgb = append(rgb, byte(uint32(i)*2654435761>>24), 0, byte(uint32(i)*40503>>12))
	}
	data := deflate(t, rgb)
	content := "q 72 0 0 72 14 14 cm /Im Do Q"
	pdf := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im 5 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 300 /Height 300 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length %d >>\nstream\n%s\nendstream", len(data), data),
	)
	dir := t.TempDir()
	input, output := filepath.Join(dir, "in.pdf"), filepath.Join(dir, "out.pdf")
	writeFile(t, input, pdf)
	// The printer preset keeps 300 dpi and the colors on its own; the flags must override it.
	if err := run(t.Context(), []string{"--engine", "ghostscript", "--quality", "printer", "--dpi", "72", "--gray", "-o", output, input}, io.Discard, io.Discard); err != nil {
		t.Fatal(err)
	}
	result, err := api.ReadContext(t.Context(), bytes.NewReader(readFile(t, output)), model.NewStatelessConfiguration())
	if err != nil {
		t.Fatal(err)
	}
	var found int
	for _, entry := range result.Table {
		sd, ok := imageStream(entry)
		if !ok {
			continue
		}
		found++
		cs, width := sd.NameEntry("ColorSpace"), sd.IntEntry("Width")
		if cs == nil || *cs != "DeviceGray" || width == nil || *width > 100 {
			t.Errorf("want a DeviceGray image at most 100 px wide, got %s", sd.Dict)
		}
	}
	if found != 1 {
		t.Fatalf("found %d images, want 1", found)
	}
}

// jpegSamplesBaseline retains the interface-based RGB loop for equivalence and allocation comparisons.
func jpegSamplesBaseline(ctx context.Context, img image.Image) ([]byte, int, error) {
	b := img.Bounds()
	out := make([]byte, 0, b.Dx()*b.Dy()*3)
	for y := b.Min.Y; y < b.Max.Y; y++ {
		if err := ctx.Err(); err != nil {
			return nil, 0, err
		}
		for x := b.Min.X; x < b.Max.X; x++ {
			r, g, bl, _ := img.At(x, y).RGBA()
			out = append(out, byte(r>>8), byte(g>>8), byte(bl>>8))
		}
	}
	return out, 3, nil
}

func TestJPEGSamplesYCbCr(t *testing.T) {
	for _, ratio := range []image.YCbCrSubsampleRatio{
		image.YCbCrSubsampleRatio444, image.YCbCrSubsampleRatio422, image.YCbCrSubsampleRatio420,
		image.YCbCrSubsampleRatio440, image.YCbCrSubsampleRatio411, image.YCbCrSubsampleRatio410,
	} {
		for _, rect := range []image.Rectangle{image.Rect(0, 0, 31, 19), image.Rect(3, 5, 34, 24), image.Rect(-9, -7, 22, 12)} {
			t.Run(fmt.Sprintf("%s/%v", ratio, rect), func(t *testing.T) {
				img := image.NewYCbCr(rect, ratio)
				for j, plane := range [][]byte{img.Y, img.Cb, img.Cr} {
					for i := range plane {
						plane[i] = byte(uint32(i+j*31) * 2654435761 >> 13)
					}
				}
				for _, sample := range []image.Image{img, img.SubImage(rect.Inset(3))} {
					want, _, err := jpegSamplesBaseline(t.Context(), sample)
					if err != nil {
						t.Fatal(err)
					}
					got, comps, err := jpegSamples(t.Context(), sample)
					if err != nil || comps != 3 || !bytes.Equal(got, want) {
						t.Fatalf("RGB samples differ: components %d, error %v", comps, err)
					}
				}
				ctx, cancel := context.WithCancel(t.Context())
				cancel()
				if _, _, err := jpegSamples(ctx, img); !errors.Is(err, context.Canceled) {
					t.Fatal(err)
				}
			})
		}
	}
}

func TestJPEGSamplesGray(t *testing.T) {
	img := image.NewGray(image.Rect(-3, 5, 70, 78))
	for i := range img.Pix {
		img.Pix[i] = byte(i*17 + i/img.Stride)
	}
	var encoded bytes.Buffer
	if err := jpeg.Encode(&encoded, img.SubImage(image.Rect(0, 8, 65, 73)), &jpeg.Options{Quality: 95}); err != nil {
		t.Fatal(err)
	}
	decoded, err := jpeg.Decode(&encoded)
	if err != nil {
		t.Fatal(err)
	}
	for name, sample := range map[string]*image.Gray{
		"packed": img, "subimage": img.SubImage(image.Rect(0, 8, 65, 73)).(*image.Gray), "decoded": decoded.(*image.Gray),
	} {
		t.Run(name, func(t *testing.T) {
			var want []byte
			bounds := sample.Bounds()
			for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
				for x := bounds.Min.X; x < bounds.Max.X; x++ {
					want = append(want, sample.GrayAt(x, y).Y)
				}
			}
			got, comps, err := jpegSamples(t.Context(), sample)
			if err != nil || comps != 1 || !bytes.Equal(got, want) {
				t.Fatalf("gray samples differ: got %d bytes, want %d, components %d, error %v", len(got), len(want), comps, err)
			}
			ctx, cancel := context.WithCancel(t.Context())
			cancel()
			if _, _, err := jpegSamples(ctx, sample); !errors.Is(err, context.Canceled) {
				t.Fatalf("cancellation: %v", err)
			}
		})
	}
}

func TestReencodeGrayJPEG(t *testing.T) {
	for _, white := range []bool{false, true} {
		t.Run(fmt.Sprintf("white=%v", white), func(t *testing.T) {
			img := image.NewGray(image.Rect(0, 0, 65, 65))
			for i := range img.Pix {
				img.Pix[i] = byte((i%65 + i/65) * 2)
				if white {
					img.Pix[i] = 255
				}
			}
			var raw bytes.Buffer
			if err := jpeg.Encode(&raw, img, &jpeg.Options{Quality: 95}); err != nil {
				t.Fatal(err)
			}
			decoded, err := jpeg.Decode(bytes.NewReader(raw.Bytes()))
			if err != nil {
				t.Fatal(err)
			}
			data := imagePDF(8, "DeviceGray", "/Filter /DCTDecode", raw.Bytes())
			data = bytes.Replace(data, []byte("/Width 64 /Height 64"), []byte("/Width 65 /Height 65"), 1)
			pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(data), configuration(""))
			if err != nil {
				t.Fatal(err)
			}
			stats, err := optimizeImages(t.Context(), pdf, options{imageMemory: 512, imageCodecs: "flate", force: true}, func(string, ...any) {})
			if err != nil || stats.changed != 1 {
				t.Fatalf("reencode: %v, %v", stats, err)
			}
			sd := firstImage(t, pdf)
			if err := sd.Decode(); err != nil {
				t.Fatal(err)
			}
			bits := *sd.IntEntry("BitsPerComponent")
			if !white && bits != 8 || white && bits != 1 {
				t.Fatalf("unexpected bit depth %d", bits)
			}
			for y := range 65 {
				for x := range 65 {
					var got byte
					if bits == 1 {
						if sd.Content[y*9+x/8]&(0x80>>uint(x%8)) != 0 {
							got = 255
						}
					} else {
						got = sd.Content[y*65+x]
					}
					if want := decoded.(*image.Gray).GrayAt(x, y).Y; got != want {
						t.Fatalf("pixel (%d,%d): got %d, want %d", x, y, got, want)
					}
				}
			}
		})
	}
}

func TestJPEGColorTransformPreservation(t *testing.T) {
	img := image.NewRGBA(image.Rect(0, 0, 64, 64))
	for i := 0; i < len(img.Pix); i += 4 {
		img.Pix[i], img.Pix[i+1], img.Pix[i+2], img.Pix[i+3] = 30, 80, 140, 255
	}
	var raw bytes.Buffer
	if err := jpeg.Encode(&raw, img, nil); err != nil {
		t.Fatal(err)
	}
	decoded, err := jpeg.Decode(bytes.NewReader(raw.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	want, _, err := jpegSamplesBaseline(t.Context(), decoded)
	if err != nil {
		t.Fatal(err)
	}
	for _, transform := range []int{0, 1} {
		for _, form := range []string{"direct", "indirect dictionary", "indirect value", "array", "indirect array", "indirect array dictionary", "absent", "empty", "empty array", "null array"} {
			t.Run(fmt.Sprintf("%d/%s", transform, form), func(t *testing.T) {
				dict := fmt.Sprintf("<< /ColorTransform %d >>", transform)
				extra := "/Filter /DCTDecode /DecodeParms "
				var more []string
				preserved := true
				switch form {
				case "direct":
					extra += dict
				case "indirect dictionary":
					extra += "6 0 R"
					more = []string{dict}
				case "indirect value":
					extra += "<< /ColorTransform 6 0 R >>"
					more = []string{fmt.Sprint(transform)}
				case "array":
					extra = "/Filter [/DCTDecode] /DecodeParms [" + dict + "]"
				case "indirect array":
					extra = "/Filter [/DCTDecode] /DecodeParms 6 0 R"
					more = []string{"[" + dict + "]"}
				case "indirect array dictionary":
					extra = "/Filter [/DCTDecode] /DecodeParms [6 0 R]"
					more = []string{dict}
				case "absent":
					extra, preserved = "/Filter /DCTDecode", false
				case "empty":
					extra, preserved = extra+"<< >>", false
				case "empty array":
					extra, preserved = extra+"[]", false
				case "null array":
					extra, preserved = extra+"[null]", false
				}
				pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(imagePDF(8, "DeviceRGB", extra, raw.Bytes(), more...)), configuration(""))
				if err != nil {
					t.Fatal(err)
				}
				before := firstImage(t, pdf).Dict.PDFString()
				stats, err := optimizeImages(t.Context(), pdf, options{imageMemory: 512, imageCodecs: "flate", force: true}, func(string, ...any) {})
				if err != nil {
					t.Fatal(err)
				}
				sd := firstImage(t, pdf)
				if preserved {
					if stats.changed != 0 || stats.preserved["jpeg color transform"] != 1 || !bytes.Equal(sd.Raw, raw.Bytes()) || sd.Dict.PDFString() != before {
						t.Fatalf("explicit transform changed: %v, %s", stats, sd.Dict)
					}
				} else {
					if stats.changed != 1 || *sd.NameEntry("Filter") != "FlateDecode" {
						t.Fatalf("default transform not reencoded: %v", stats)
					}
					if err := sd.Decode(); err != nil || !bytes.Equal(sd.Content, want) {
						t.Fatalf("default transform changed samples: %v", err)
					}
				}
			})
		}
	}
}

func BenchmarkJPEGSamplesYCbCr(b *testing.B) {
	img := image.NewYCbCr(image.Rect(0, 0, 512, 512), image.YCbCrSubsampleRatio420)
	for j, plane := range [][]byte{img.Y, img.Cb, img.Cr} {
		for i := range plane {
			plane[i] = byte(uint32(i+j*31) * 2654435761 >> 13)
		}
	}
	for _, tc := range []struct {
		name    string
		samples func(context.Context, image.Image) ([]byte, int, error)
	}{{"interface-baseline", jpegSamplesBaseline}, {"concrete", jpegSamples}} {
		b.Run(tc.name, func(b *testing.B) {
			b.ReportAllocs()
			b.SetBytes(512 * 512 * 3)
			for b.Loop() {
				out, comps, err := tc.samples(b.Context(), img)
				if err != nil || comps != 3 || len(out) != 512*512*3 {
					b.Fatal("invalid samples", err)
				}
			}
		})
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
