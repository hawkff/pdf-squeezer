package main

import (
	"bytes"
	"compress/zlib"
	"context"
	"encoding/binary"
	"errors"
	"image"
	"image/color"
	"image/png"
	"io"
	"sync"

	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

var errEncodingLarger = errors.New("encoded candidate exceeds the current winner")

type encodingBuffer struct {
	data  bytes.Buffer
	limit int // zero means no competing candidate
}

func (b *encodingBuffer) Write(p []byte) (int, error) {
	if b.limit > 0 && len(p) > b.limit-b.data.Len() {
		return 0, errEncodingLarger
	}
	return b.data.Write(p)
}

// pngIDATWriter consumes the standard encoder's PNG stream, retaining only IDAT
// bytes. Chunk framing and checksums do not count against the candidate limit.
type pngIDATWriter struct {
	output      encodingBuffer
	header      [8]byte
	headerBytes int
	remaining   uint32
	skip        int
	idat        bool
}

func (w *pngIDATWriter) Write(p []byte) (int, error) {
	n := len(p)
	for len(p) > 0 {
		switch {
		case w.skip > 0:
			count := min(w.skip, len(p))
			w.skip -= count
			p = p[count:]
		case w.remaining > 0:
			count := int(min(uint64(w.remaining), uint64(len(p))))
			if w.idat {
				if _, err := w.output.Write(p[:count]); err != nil {
					return n - len(p), err
				}
			}
			w.remaining -= uint32(count)
			p = p[count:]
			if w.remaining == 0 {
				w.skip = 4 // chunk CRC
			}
		default:
			count := copy(w.header[w.headerBytes:], p)
			w.headerBytes += count
			p = p[count:]
			if w.headerBytes == len(w.header) {
				w.remaining = binary.BigEndian.Uint32(w.header[:4])
				w.idat = string(w.header[4:]) == "IDAT"
				w.headerBytes = 0
				if w.remaining == 0 {
					w.skip = 4
				}
			}
		}
	}
	return n, nil
}

// PNG's Gray16 path boxes a color.Color for every pixel. The finite set of
// immutable sample values needs about 1.1 MiB once, rather than per-pixel garbage.
var gray16Colors = sync.OnceValue(func() []color.Color {
	colors := make([]color.Color, 1<<16)
	for i := range colors {
		colors[i] = color.Gray16{Y: uint16(i)}
	}
	return colors
})

type gray16Image struct {
	*image.Gray16
	colors []color.Color
}

func (g gray16Image) At(x, y int) color.Color {
	return g.colors[g.Gray16At(x, y).Y]
}

// Stable pointers to owned pixels avoid color interface allocations in PNG's
// 16-bit RGB path. Alpha is opaque, as it is in the PDF's RGB sample stream.
type rgb16Image struct {
	rect   image.Rectangle
	pixels []color.RGBA64
}

func (i *rgb16Image) ColorModel() color.Model { return color.RGBA64Model }
func (i *rgb16Image) Bounds() image.Rectangle { return i.rect }
func (i *rgb16Image) Opaque() bool            { return true }
func (i *rgb16Image) At(x, y int) color.Color {
	if !image.Pt(x, y).In(i.rect) {
		return color.RGBA64{}
	}
	return &i.pixels[(y-i.rect.Min.Y)*i.rect.Dx()+x-i.rect.Min.X]
}

// PNG IDAT data is exactly the zlib stream expected by FlateDecode/Predictor 15.
func pngIDAT(ctx context.Context, img image.Image, best bool, limit int) ([]byte, error) {
	if gray, ok := img.(*image.Gray16); ok {
		img = gray16Image{gray, gray16Colors()}
	}
	output := pngIDATWriter{output: encodingBuffer{limit: limit}, skip: 8}
	encoder := png.Encoder{}
	if best {
		encoder.CompressionLevel = png.BestCompression
	}
	if err := encoder.Encode(contextWriter{ctx, &output}, img); err != nil {
		return nil, err
	}
	if output.headerBytes != 0 || output.remaining != 0 || output.skip != 0 {
		return nil, errors.New("incomplete PNG output")
	}
	return output.output.data.Bytes(), nil
}

func imageSamples(ctx context.Context, sd *types.StreamDict, expected int64, rows int) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if expected <= 0 || expected > maxImageBytes {
		return nil, errors.New("image sample size exceeds limit")
	}
	if len(sd.Content) > 0 {
		return sd.Content, nil
	}
	if len(sd.FilterPipeline) == 1 && sd.FilterPipeline[0].Name == "FlateDecode" {
		predictor, present := sd.FilterPipeline[0].DecodeParms["Predictor"]
		if !present || predictor == types.Integer(1) {
			reader, err := zlib.NewReader(contextReader{ctx, bytes.NewReader(sd.Raw)})
			if err != nil {
				return nil, err
			}
			defer reader.Close()
			// One extra byte detects oversize data and forces the checksum read.
			data := make([]byte, expected+1)
			n, err := io.ReadFull(reader, data)
			if err != io.EOF && err != io.ErrUnexpectedEOF {
				if err == nil {
					err = errors.New("image has excess samples")
				}
				return nil, err
			}
			return data[:n], nil
		}
	}
	// General filter pipelines retain pdfcpu's bounded decoding and predictors.
	if err := sd.DecodeWithLimit(min(maxImageBytes, 2*(expected+int64(rows)))); err != nil {
		return nil, err
	}
	return sd.Content, nil
}
