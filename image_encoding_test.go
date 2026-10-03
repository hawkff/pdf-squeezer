package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"image"
	"image/jpeg"
	"image/png"
	"testing"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

func referencePNG(img image.Image, best bool) ([]byte, []byte, error) {
	var file bytes.Buffer
	encoder := png.Encoder{}
	if best {
		encoder.CompressionLevel = png.BestCompression
	}
	if err := encoder.Encode(&file, img); err != nil {
		return nil, nil, err
	}
	var payload []byte
	for data := file.Bytes()[8:]; len(data) >= 12; {
		n := int(binary.BigEndian.Uint32(data[:4]))
		if string(data[4:8]) == "IDAT" {
			payload = append(payload, data[8:8+n]...)
		}
		data = data[n+12:]
	}
	return file.Bytes(), payload, nil
}

func TestPNGStreamingAndBounds(t *testing.T) {
	rgb := image.NewRGBA(image.Rect(0, 0, 512, 128))
	for i := 0; i < len(rgb.Pix); i += 4 {
		for c := range 3 {
			rgb.Pix[i+c] = byte(uint32(i+c) * 2654435761 >> 17)
		}
		rgb.Pix[i+3] = 255
	}
	gray := image.NewGray16(image.Rect(3, 5, 259, 261))
	for i := 0; i < len(gray.Pix); i += 2 {
		binary.BigEndian.PutUint16(gray.Pix[i:], uint16(i/2))
	}
	for _, img := range []image.Image{rgb, gray, gray.SubImage(image.Rect(8, 7, 80, 99))} {
		for _, best := range []bool{false, true} {
			file, want, err := referencePNG(img, best)
			if err != nil {
				t.Fatal(err)
			}
			for _, limit := range []int{0, len(want) - 1, len(want), len(want) + 1} {
				got, err := pngIDAT(t.Context(), img, best, limit)
				if limit > 0 && limit < len(want) {
					if !errors.Is(err, errEncodingLarger) {
						t.Fatalf("limit %d: %v", limit, err)
					}
				} else if err != nil || !bytes.Equal(got, want) {
					t.Fatalf("payload changed at limit %d: %v", limit, err)
				}
			}
			for _, chunk := range []int{1, 3, 7, 8, 4096, len(file)} {
				writer := pngIDATWriter{output: encodingBuffer{limit: len(want)}, skip: 8}
				for data := file; len(data) > 0; {
					n := min(chunk, len(data))
					written, err := writer.Write(data[:n])
					if err != nil || written != n {
						t.Fatal(written, err)
					}
					data = data[n:]
				}
				if !bytes.Equal(writer.output.data.Bytes(), want) || writer.headerBytes != 0 || writer.remaining != 0 || writer.skip != 0 {
					t.Fatal("split PNG framing changed payload")
				}
			}
		}
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if _, err := pngIDAT(ctx, rgb, false, 1); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
}

func TestDeflateCandidateBounds(t *testing.T) {
	data := blocks(256, 128, []byte{0, 19, 84, 255}, 7)
	for _, best := range []bool{false, true} {
		want, err := deflateSamples(t.Context(), data, best, 0)
		if err != nil {
			t.Fatal(err)
		}
		for _, limit := range []int{len(want) - 1, len(want), len(want) + 1} {
			got, err := deflateSamples(t.Context(), data, best, limit)
			if limit < len(want) {
				if !errors.Is(err, errEncodingLarger) {
					t.Fatal(err)
				}
			} else if err != nil || !bytes.Equal(got, want) {
				t.Fatal("bounded Flate changed output", err)
			}
		}
	}
}

func TestBoundedImageSamples(t *testing.T) {
	data := blocks(64, 64, []byte{0, 18, 200, 255}, 3)
	compressed := deflate(t, data)
	badChecksum := bytes.Clone(compressed)
	badChecksum[len(badChecksum)-1] ^= 1
	for name, raw := range map[string][]byte{
		"valid": compressed, "short": deflate(t, data[:len(data)-1]),
		"long": deflate(t, append(bytes.Clone(data), 0)), "checksum": badChecksum,
		"missing trailer": compressed[:len(compressed)-2],
	} {
		t.Run(name, func(t *testing.T) {
			sd := types.StreamDict{Dict: types.NewDict(), Raw: raw, FilterPipeline: []types.PDFFilter{{Name: "FlateDecode"}}}
			baseline := sd
			baseErr := baseline.DecodeWithLimit(2 * int64(len(data)+64))
			got, err := imageSamples(t.Context(), &sd, int64(len(data)), 64)
			accepted := err == nil && len(got) == len(data)
			if accepted != (baseErr == nil && len(baseline.Content) == len(data)) {
				t.Fatalf("decode behavior differs: %v / %v", err, baseErr)
			}
			if accepted && !bytes.Equal(got, baseline.Content) {
				t.Fatal("decoded bytes changed")
			}
		})
	}
	_, pngData, err := referencePNG(&image.Gray{Pix: data, Stride: 64, Rect: image.Rect(0, 0, 64, 64)}, false)
	if err != nil {
		t.Fatal(err)
	}
	sd := types.StreamDict{Dict: types.NewDict(), Raw: pngData, FilterPipeline: []types.PDFFilter{{Name: "FlateDecode", DecodeParms: types.Dict{"Predictor": types.Integer(15), "Colors": types.Integer(1), "Columns": types.Integer(64), "BitsPerComponent": types.Integer(8)}}}}
	if got, err := imageSamples(t.Context(), &sd, int64(len(data)), 64); err != nil || !bytes.Equal(got, data) {
		t.Fatal("predictor fallback changed samples", err)
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if _, err := imageSamples(ctx, &sd, int64(len(data)), 64); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
}

func TestReencodeMatchesFullCandidates(t *testing.T) {
	for _, bits := range []int{8, 16} {
		for _, comps := range []int{1, 3} {
			for _, best := range []bool{false, true} {
				for _, reduce := range []bool{false, true} {
					for _, flat := range []bool{false, true} {
						w, h := 64, 64
						samples := make([]byte, w*h*comps*bits/8)
						for i := range samples {
							if flat {
								samples[i] = 83
							} else {
								samples[i] = byte(uint32(i) * 2654435761 >> 13)
							}
						}
						original := bytes.Clone(samples)
						outBits := bits
						reference := samples
						if bits == 16 && reduce {
							outBits = 8
							reference = make([]byte, len(samples)/2)
							for i := range reference {
								reference[i] = samples[2*i]
							}
						}
						var img image.Image
						if comps == 1 {
							if outBits == 16 {
								img = &image.Gray16{Pix: reference, Stride: 2 * w, Rect: image.Rect(0, 0, w, h)}
							} else {
								img = &image.Gray{Pix: reference, Stride: w, Rect: image.Rect(0, 0, w, h)}
							}
						} else if outBits == 16 {
							rgba := image.NewNRGBA64(image.Rect(0, 0, w, h))
							for i := 0; i < w*h; i++ {
								copy(rgba.Pix[8*i:], reference[6*i:6*i+6])
								rgba.Pix[8*i+6], rgba.Pix[8*i+7] = 255, 255
							}
							img = rgba
						} else {
							rgba := image.NewRGBA(image.Rect(0, 0, w, h))
							for i := 0; i < w*h; i++ {
								copy(rgba.Pix[4*i:], reference[3*i:3*i+3])
								rgba.Pix[4*i+3] = 255
							}
							img = rgba
						}
						_, want, err := referencePNG(img, best)
						if err != nil {
							t.Fatal(err)
						}
						wantFilter, wantPredictor := "FlateDecode", true
						if best {
							raw, err := deflateSamples(t.Context(), reference, true, 0)
							if err != nil {
								t.Fatal(err)
							}
							if len(raw) < len(want) {
								want, wantPredictor = raw, false
							}
						}
						if outBits == 8 {
							var jpegData bytes.Buffer
							if err := jpeg.Encode(&jpegData, img, &jpeg.Options{Quality: 45}); err != nil {
								t.Fatal(err)
							}
							if jpegData.Len() < len(want) {
								want, wantFilter, wantPredictor = jpegData.Bytes(), "DCTDecode", false
							}
						}
						o := options{imageCodecs: "flate,jpeg", imageQuality: 45, force: true, reduceBits: reduce, colorReduction: "preserve"}
						if best {
							o.compression = "heavy"
						}
						job := imageJob{comps: comps, sd: types.StreamDict{Dict: types.Dict{"Width": types.Integer(w), "Height": types.Integer(h), "BitsPerComponent": types.Integer(bits)}, Raw: samples}}
						got := reencode(t.Context(), &job, o)
						if got.err != nil || got.reason != "" || got.bpc != outBits || got.filter != wantFilter || (got.parms != nil) != wantPredictor || !bytes.Equal(got.data, want) {
							t.Fatalf("bits=%d comps=%d heavy=%v reduce=%v flat=%v: %+v", bits, comps, best, reduce, flat, got)
						}
						if !bytes.Equal(samples, original) {
							t.Fatal("encoding mutated original samples")
						}
						o.force = false
						got = reencode(t.Context(), &job, o)
						if got.err != nil || !bytes.Equal(samples, original) {
							t.Fatal("size fallback mutated original samples", got.err)
						}
					}
				}
			}
		}
	}
}

func TestDuplicateMaskEncodingWork(t *testing.T) {
	mask := bytes.Repeat([]byte{0x7f, 0x81}, 4096)
	body := bytes.Repeat([]byte{30}, 4096)
	content := "q 64 0 0 64 0 0 cm /A Do Q q 64 0 0 64 64 0 cm /B Do Q"
	maskObject := fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 16 /Length %d >>\nstream\n%s\nendstream", len(mask), mask)
	data := buildPDF(0,
		"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 128 64] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>",
		fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(content), content),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 7 0 R /Length %d >>\nstream\n%s\nendstream", len(body), body),
		fmt.Sprintf("<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 8 0 R /Length %d >>\nstream\n%s\nendstream", len(body), body), maskObject, maskObject)
	for _, cached := range []int{0, 7, 8} {
		t.Run(fmt.Sprintf("cached=%d", cached), func(t *testing.T) {
			pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(data), configuration(""))
			if err != nil {
				t.Fatal(err)
			}
			alternate := bytes.Repeat([]byte{0x33, 0x77}, 4096)
			wantJobs := 3
			if cached != 0 {
				sd, _ := imageStream(pdf.Table[cached])
				sd.Content = alternate
				pdf.Table[cached].Object = sd
				wantJobs = 4
			}
			jobs := 0
			stats, err := optimizeImages(t.Context(), pdf, options{imageMemory: 512, imageQuality: 75, imageCodecs: "flate", force: true, reduceBits: true}, func(string, ...any) { jobs++ })
			if err != nil || stats.seen != 4 || stats.changed != 4 || jobs != wantJobs {
				t.Fatalf("mask work: %v, jobs=%d, %v", stats, jobs, err)
			}
			for _, nr := range []int{7, 8} {
				want := mask
				if nr == cached {
					want = alternate
				}
				sd, _ := imageStream(pdf.Table[nr])
				if err := sd.Decode(); err != nil || *sd.IntEntry("BitsPerComponent") != 16 || !bytes.Equal(sd.Content, want) {
					t.Fatal("shared mask changed samples or precision", err)
				}
			}
			for nr, want := range map[int]int{5: 7, 6: 8} {
				sd, _ := imageStream(pdf.Table[nr])
				if sd.IndirectRefEntry("SMask").ObjectNumber.Value() != want {
					t.Fatal("mask references changed before final deduplication")
				}
			}
		})
	}
}

func TestStreamDeduplicationRawAliasesAndCachedContent(t *testing.T) {
	for _, cached := range []bool{false, true} {
		data := bytes.Repeat([]byte("q Q\n"), 1024)
		compressed := deflate(t, data)
		pdf, err := api.ReadAndValidate(t.Context(), bytes.NewReader(buildPDF(0,
			"<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
			"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << >> /Contents [4 0 R 5 0 R 6 0 R] >>",
			fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(data), data),
			fmt.Sprintf("<< /Length %d /Filter /FlateDecode >>\nstream\n%s\nendstream", len(compressed), compressed),
			fmt.Sprintf("<< /Length %d >>\nstream\n%sendstream", len(data), data))), configuration(""))
		if err != nil {
			t.Fatal(err)
		}
		if cached {
			for _, n := range []int{4, 6} {
				sd := pdf.Table[n].Object.(types.StreamDict)
				sd.Content = []byte(fmt.Sprintf("q %d 0 0 %d 0 0 cm Q", n, n))
				pdf.Table[n].Object = sd
			}
		}
		if err := deduplicateStreams(t.Context(), pdf); err != nil {
			t.Fatal(err)
		}
		page, err := pdf.DereferenceDict(*types.NewIndirectRef(3, 0))
		if err != nil {
			t.Fatal(err)
		}
		contents, err := pdf.DereferenceArray(page["Contents"])
		if err != nil {
			t.Fatal(err)
		}
		a, b, c := contents[0].(types.IndirectRef), contents[1].(types.IndirectRef), contents[2].(types.IndirectRef)
		if cached {
			if a == b || a == c || b == c {
				t.Fatal("stale raw bytes merged distinct cached content")
			}
		} else if a != b || a != c {
			t.Fatal("raw alias chain did not reach the normalized representative")
		}
	}
}

func BenchmarkPNGGray16(b *testing.B) {
	img := image.NewGray16(image.Rect(0, 0, 512, 256))
	for i := range img.Pix {
		img.Pix[i] = byte(uint32(i) * 2654435761 >> 13)
	}
	gray16Colors()
	for _, baseline := range []bool{true, false} {
		b.Run(fmt.Sprint("baseline=", baseline), func(b *testing.B) {
			b.ReportAllocs()
			for b.Loop() {
				var data []byte
				var err error
				if baseline {
					_, data, err = referencePNG(img, false)
				} else {
					data, err = pngIDAT(b.Context(), img, false, 0)
				}
				if err != nil || len(data) == 0 {
					b.Fatal(err)
				}
			}
		})
	}
}
