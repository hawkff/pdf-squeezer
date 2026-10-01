package main

import (
	"bytes"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"runtime"
	"sort"
	"strings"
	"sync"

	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

const (
	jpegQuality     = 75
	minImageSavings = 1024      // bytes; below this a re-encode is not worth the quality risk
	maxImageBytes   = 256 << 20 // decoded samples; larger images are left alone
)

type imageStats struct {
	seen, changed int
	before, after int64
	preserved     map[string]int
}

func (s imageStats) String() string {
	reasons := make([]string, 0, len(s.preserved))
	for reason, n := range s.preserved {
		reasons = append(reasons, fmt.Sprintf("%s %d", reason, n))
	}
	sort.Strings(reasons)
	out := fmt.Sprintf("images: %d seen, %d re-encoded, %d -> %d bytes", s.seen, s.changed, s.before, s.after)
	if len(reasons) > 0 {
		out += "; preserved: " + strings.Join(reasons, ", ")
	}
	return out
}

// optimizeImages re-encodes the images it fully understands: 8- or 16-bit DeviceGray, DeviceRGB,
// or ICC gray/RGB samples stored with lossless filters or as a single JPEG. RGB images whose
// pixels are all exactly gray become DeviceGray; gray images holding only black and white become
// 1-bit; each image then keeps the smaller of Flate with PNG predictors and JPEG, and the original
// bytes stay unless the re-encode saves at least 2% and 1 KiB. Everything else is left untouched.
func optimizeImages(pdf *model.Context) (imageStats, error) {
	stats := imageStats{preserved: map[string]int{}}
	softMasks := map[int]bool{}
	for _, entry := range pdf.Table {
		if sd, ok := imageStream(entry); ok {
			if ref := sd.IndirectRefEntry("SMask"); ref != nil {
				softMasks[ref.ObjectNumber.Value()] = true
			}
		}
	}
	type job struct {
		objNr    int
		entry    *model.XRefTableEntry
		sd       types.StreamDict
		comps    int
		softMask bool
		encoding
	}
	var jobs []*job
	for objNr, entry := range pdf.Table {
		if sd, ok := imageStream(entry); ok {
			var comps int
			reason := precheck(&sd)
			if reason == "" {
				comps, reason = components(pdf, &sd)
			}
			jobs = append(jobs, &job{objNr: objNr, entry: entry, sd: sd, comps: comps, softMask: softMasks[objNr], encoding: encoding{reason: reason}})
		}
	}
	// Images are independent; the color space lookups above were the only reads of the context.
	var wg sync.WaitGroup
	workers := make(chan struct{}, runtime.GOMAXPROCS(0))
	for _, j := range jobs {
		if j.reason != "" {
			continue
		}
		wg.Add(1)
		workers <- struct{}{}
		go func(j *job) {
			defer wg.Done()
			j.encoding = reencode(&j.sd, j.comps, j.softMask)
			<-workers
		}(j)
	}
	wg.Wait()
	for _, j := range jobs {
		sd := j.sd
		stats.seen++
		stats.before += int64(len(sd.Raw))
		if j.err != nil {
			return stats, fmt.Errorf("image object %d: %w", j.objNr, j.err)
		}
		if j.reason != "" {
			stats.preserved[j.reason]++
			stats.after += int64(len(sd.Raw))
			continue
		}
		data, filter, parms, bpc, gray := j.data, j.filter, j.parms, j.bpc, j.gray
		sd.Raw, sd.Content = data, nil
		length := int64(len(data))
		sd.StreamLength, sd.StreamLengthObjNr = &length, nil
		sd.Update("Length", types.Integer(length))
		sd.Update("BitsPerComponent", types.Integer(bpc))
		sd.Update("Filter", types.Name(filter))
		if parms == nil {
			sd.Delete("DecodeParms")
			sd.FilterPipeline = []types.PDFFilter{{Name: filter}}
		} else {
			sd.Update("DecodeParms", parms)
			sd.FilterPipeline = []types.PDFFilter{{Name: filter, DecodeParms: parms}}
		}
		if gray {
			sd.Update("ColorSpace", types.Name("DeviceGray"))
		}
		j.entry.Object = sd
		stats.changed++
		stats.after += length
	}
	return stats, nil
}

func imageStream(entry *model.XRefTableEntry) (types.StreamDict, bool) {
	if entry == nil || entry.Free {
		return types.StreamDict{}, false
	}
	sd, ok := entry.Object.(types.StreamDict)
	if !ok {
		return sd, false
	}
	subtype := sd.NameEntry("Subtype")
	return sd, subtype != nil && *subtype == "Image"
}

// encoding is the outcome for one image: new stream data, or a reason to leave it alone.
type encoding struct {
	data   []byte
	filter string
	parms  types.Dict
	bpc    int
	gray   bool // color space becomes DeviceGray
	reason string
	err    error
}

func preserve(reason string) encoding {
	return encoding{reason: reason}
}

// precheck returns the reason an image is out of scope before its color space is looked at.
func precheck(sd *types.StreamDict) string {
	if mask := sd.BooleanEntry("ImageMask"); mask != nil && *mask {
		return "image mask"
	}
	for _, key := range []string{"Mask", "Decode"} {
		if _, found := sd.Find(key); found {
			return strings.ToLower(key) + " entry"
		}
	}
	width, height := sd.IntEntry("Width"), sd.IntEntry("Height")
	bits := sd.IntEntry("BitsPerComponent")
	if width == nil || height == nil || bits == nil || *width <= 0 || *height <= 0 {
		return "invalid dimensions"
	}
	if *bits != 8 && *bits != 16 {
		return fmt.Sprintf("%d-bit", *bits)
	}
	return ""
}

func reencode(sd *types.StreamDict, comps int, isSoftMask bool) encoding {
	width, height, bits := sd.IntEntry("Width"), sd.IntEntry("Height"), sd.IntEntry("BitsPerComponent")
	if int64(*width)*int64(*height)*int64(comps)*int64(*bits/8) > maxImageBytes {
		return preserve("too large")
	}
	w, h, declared := *width, *height, comps
	var samples []byte // 8-bit, comps per pixel, row-major
	switch kind := filterKind(sd.FilterPipeline); kind {
	case "lossless":
		if err := sd.Decode(); err != nil {
			return preserve("undecodable")
		}
		samples = sd.Content
		if *bits == 16 {
			samples = highBytes(samples)
		}
		if len(samples) != w*h*comps {
			return preserve("length mismatch")
		}
	case "jpeg":
		img, err := jpeg.Decode(bytes.NewReader(sd.Raw))
		if err != nil || img.Bounds().Dx() != w || img.Bounds().Dy() != h {
			return preserve("undecodable jpeg")
		}
		if samples, comps = jpegSamples(img); samples == nil {
			return preserve("jpeg color model")
		}
	default:
		return preserve(kind)
	}
	var out encoding
	if comps != declared {
		if comps != 1 {
			return preserve("jpeg color model")
		}
		out.gray = true // a gray JPEG declared as RGB
	}
	if comps == 3 && allGray(samples) {
		samples, comps, out.gray = grayChannel(samples), 1, true
	}
	bilevel := comps == 1 && !isSoftMask && onlyBlackAndWhite(samples)

	var img image.Image
	out.bpc = 8
	switch {
	case bilevel:
		out.bpc = 1
		p := image.NewPaletted(image.Rect(0, 0, w, h), color.Palette{color.Gray{0}, color.Gray{255}})
		for i, v := range samples {
			if v != 0 {
				p.Pix[i] = 1
			}
		}
		img = p
	case comps == 1:
		img = &image.Gray{Pix: samples, Stride: w, Rect: image.Rect(0, 0, w, h)}
	default:
		rgba := image.NewRGBA(image.Rect(0, 0, w, h))
		for i := 0; i < w*h; i++ {
			copy(rgba.Pix[4*i:4*i+3], samples[3*i:3*i+3])
			rgba.Pix[4*i+3] = 255
		}
		img = rgba
	}
	flate, err := pngIDAT(img)
	if err != nil {
		return encoding{err: err}
	}
	out.data, out.filter = flate, "FlateDecode"
	out.parms = types.Dict{"Predictor": types.Integer(15), "Colors": types.Integer(comps), "BitsPerComponent": types.Integer(out.bpc), "Columns": types.Integer(w)}
	if !bilevel && !isSoftMask {
		var buf bytes.Buffer
		if err := jpeg.Encode(&buf, img, &jpeg.Options{Quality: jpegQuality}); err != nil {
			return encoding{err: err}
		}
		if buf.Len() < len(flate) {
			out.data, out.filter, out.parms = buf.Bytes(), "DCTDecode", nil
		}
	}
	saved := len(sd.Raw) - len(out.data)
	if saved < minImageSavings || saved < len(sd.Raw)/50 {
		return preserve("no gain")
	}
	return out
}

// components resolves the color space to 1 (gray) or 3 (RGB) samples per pixel.
func components(pdf *model.Context, sd *types.StreamDict) (int, string) {
	cs, err := pdf.Dereference(sd.Dict["ColorSpace"])
	if err != nil {
		return 0, "color space"
	}
	switch cs := cs.(type) {
	case types.Name:
		switch cs {
		case "DeviceGray":
			return 1, ""
		case "DeviceRGB":
			return 3, ""
		}
		return 0, "color space " + string(cs)
	case types.Array:
		if len(cs) == 0 {
			return 0, "color space"
		}
		name, ok := cs[0].(types.Name)
		if !ok {
			return 0, "color space"
		}
		if name == "ICCBased" && len(cs) == 2 {
			if profile, _, err := pdf.DereferenceStreamDict(cs[1]); err == nil && profile != nil {
				if n := profile.IntEntry("N"); n != nil && (*n == 1 || *n == 3) {
					return *n, ""
				}
			}
		}
		return 0, "color space " + string(name)
	}
	return 0, "color space"
}

func filterKind(pipeline []types.PDFFilter) string {
	if len(pipeline) == 0 {
		return "lossless"
	}
	if len(pipeline) == 1 && pipeline[0].Name == "DCTDecode" {
		return "jpeg"
	}
	for _, f := range pipeline {
		switch f.Name {
		case "FlateDecode", "LZWDecode", "ASCIIHexDecode", "ASCII85Decode", "RunLengthDecode":
		default:
			return "filter " + f.Name
		}
	}
	return "lossless"
}

func highBytes(samples16 []byte) []byte {
	out := make([]byte, len(samples16)/2)
	for i := range out {
		out[i] = samples16[2*i]
	}
	return out
}

func jpegSamples(img image.Image) ([]byte, int) {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	switch img := img.(type) {
	case *image.Gray:
		return img.Pix, 1
	case *image.YCbCr, *image.RGBA, *image.NRGBA:
		out := make([]byte, 0, w*h*3)
		for y := b.Min.Y; y < b.Max.Y; y++ {
			for x := b.Min.X; x < b.Max.X; x++ {
				r, g, bl, _ := img.At(x, y).RGBA()
				out = append(out, byte(r>>8), byte(g>>8), byte(bl>>8))
			}
		}
		return out, 3
	}
	return nil, 0
}

func allGray(rgb []byte) bool {
	for i := 0; i+2 < len(rgb); i += 3 {
		if rgb[i] != rgb[i+1] || rgb[i] != rgb[i+2] {
			return false
		}
	}
	return true
}

func grayChannel(rgb []byte) []byte {
	out := make([]byte, len(rgb)/3)
	for i := range out {
		out[i] = rgb[3*i]
	}
	return out
}

func onlyBlackAndWhite(gray []byte) bool {
	for _, v := range gray {
		if v != 0 && v != 255 {
			return false
		}
	}
	return true
}

// pngIDAT returns the image's PNG pixel stream: zlib data with per-row predictors,
// exactly what /FlateDecode with /Predictor 15 expects.
func pngIDAT(img image.Image) ([]byte, error) {
	var file bytes.Buffer
	if err := png.Encode(&file, img); err != nil {
		return nil, err
	}
	var idat []byte
	data := file.Bytes()[8:]
	for len(data) >= 12 {
		length := int(data[0])<<24 | int(data[1])<<16 | int(data[2])<<8 | int(data[3])
		if len(data) < 12+length {
			return nil, errors.New("truncated PNG chunk")
		}
		if string(data[4:8]) == "IDAT" {
			idat = append(idat, data[8:8+length]...)
		}
		data = data[12+length:]
	}
	return idat, nil
}
