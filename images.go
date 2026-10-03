package main

import (
	"bytes"
	"compress/zlib"
	"context"
	"errors"
	"fmt"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"io"
	"runtime"
	"sort"
	"strings"

	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

const (
	minImageSavings = 1024
	maxImageBytes   = 256 << 20
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

type imageJob struct {
	objNr            int
	entry            *model.XRefTableEntry
	sd               types.StreamDict
	comps            int
	device, softMask bool
	invert           []bool
	cost             int64
	reason           string
}

type encoding struct {
	data              []byte
	filter            string
	parms             types.Dict
	bpc               int
	gray, clearDecode bool
	reason            string
	err               error
}

func preserve(reason string) encoding { return encoding{reason: reason} }

// optimizeImages resolves document references before starting workers. Only the
// collector mutates the document, and each worker releases decoded samples before
// returning. The budget covers estimated working buffers, not the parsed PDF.
func optimizeImages(ctx context.Context, pdf *model.Context, opts options, logf func(string, ...any)) (imageStats, error) {
	stats := imageStats{preserved: map[string]int{}}
	softMasks := map[int]bool{}
	defaultColors := hasDefaultColorSpace(pdf)
	for _, entry := range pdf.Table {
		if sd, ok := imageStream(entry); ok {
			if ref := sd.IndirectRefEntry("SMask"); ref != nil {
				softMasks[ref.ObjectNumber.Value()] = true
			}
		}
	}
	var ids []int
	for nr, entry := range pdf.Table {
		if _, ok := imageStream(entry); ok {
			ids = append(ids, nr)
		}
	}
	sort.Ints(ids)
	budget := int64(opts.imageMemory) << 20
	var jobs []imageJob
	for _, nr := range ids {
		if err := ctx.Err(); err != nil {
			return stats, err
		}
		sd, _ := imageStream(pdf.Table[nr])
		j := imageJob{objNr: nr, entry: pdf.Table[nr], sd: sd, softMask: softMasks[nr]}
		j.reason = precheck(&sd)
		if j.reason == "" {
			j.comps, j.device, j.reason = components(pdf, &sd)
			if defaultColors && j.comps == 3 {
				j.device = false
			}
		}
		if j.reason == "" {
			if ref := sd.IndirectRefEntry("SMask"); ref != nil {
				mask, _, err := pdf.DereferenceStreamDict(*ref)
				if err != nil || mask == nil {
					j.reason = "invalid soft mask"
				} else if _, ok := mask.Find("Matte"); ok {
					j.reason = "soft-mask matte"
				}
			}
		}
		if j.reason == "" {
			j.invert, j.reason = imageDecode(pdf, &sd, j.comps, j.softMask)
		}
		if j.reason == "" {
			n, ok := sampleSize(*sd.IntEntry("Width"), *sd.IntEntry("Height"), j.comps, *sd.IntEntry("BitsPerComponent"))
			if !ok {
				j.reason = "too large"
			} else {
				pixels := int64(*sd.IntEntry("Width")) * int64(*sd.IntEntry("Height"))
				j.cost = 4*n + 12*pixels + 2*int64(len(sd.Raw)) + 8<<20
				if j.cost > budget {
					j.reason = "memory budget"
				}
			}
		}
		stats.seen++
		stats.before += int64(len(sd.Raw))
		if j.reason != "" {
			stats.preserved[j.reason]++
			stats.after += int64(len(sd.Raw))
			continue
		}
		jobs = append(jobs, j)
	}
	type result struct {
		job imageJob
		encoding
	}
	workers := max(1, runtime.GOMAXPROCS(0))
	results := make(chan result, workers)
	workCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	var firstErr error
	var used int64
	next, active, done := 0, 0, 0
	for next < len(jobs) || active > 0 {
		if workCtx.Err() != nil && firstErr == nil {
			firstErr = workCtx.Err()
		}
		for firstErr == nil && next < len(jobs) && active < workers && used+jobs[next].cost <= budget {
			j := jobs[next]
			next++
			active++
			used += j.cost
			go func() { e := reencode(workCtx, &j, opts); j.sd.Content = nil; results <- result{j, e} }()
		}
		if active == 0 {
			break
		}
		r := <-results
		active--
		used -= r.job.cost
		done++
		if r.err != nil && firstErr == nil {
			firstErr = fmt.Errorf("image object %d: %w", r.job.objNr, r.err)
			cancel()
		}
		if firstErr != nil {
			continue
		}
		if r.reason != "" {
			stats.preserved[r.reason]++
			stats.after += int64(len(r.job.sd.Raw))
		} else {
			applyEncoding(r.job.entry, r.job.sd, r.encoding)
			stats.changed++
			stats.after += int64(len(r.data))
		}
		logf("image %d/%d: object %d", done, len(jobs), r.job.objNr)
	}
	if firstErr == nil {
		firstErr = ctx.Err()
	}
	return stats, firstErr
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

func precheck(sd *types.StreamDict) string {
	if mask := sd.BooleanEntry("ImageMask"); mask != nil && *mask {
		return "image mask"
	}
	if _, found := sd.Find("Mask"); found {
		return "mask entry"
	}
	width, height, bits := sd.IntEntry("Width"), sd.IntEntry("Height"), sd.IntEntry("BitsPerComponent")
	if width == nil || height == nil || bits == nil || *width <= 0 || *height <= 0 {
		return "invalid dimensions"
	}
	if *bits != 1 && *bits != 8 && *bits != 16 {
		return fmt.Sprintf("%d-bit", *bits)
	}
	return ""
}

func sampleSize(w, h, comps, bits int) (int64, bool) {
	if w <= 0 || h <= 0 || comps < 1 || comps > 4 || bits != 1 && bits != 8 && bits != 16 {
		return 0, false
	}
	// Divide before multiplying so hostile dimensions cannot overflow the guard.
	if int64(w) > maxImageBytes*8/int64(comps*bits) {
		return 0, false
	}
	row := (int64(w)*int64(comps*bits) + 7) / 8
	if int64(h) > maxImageBytes/row {
		return 0, false
	}
	return row * int64(h), true
}

// Only identity and inversion Decode arrays are normalized. Other transfer
// functions remain attached to their original bytes rather than being guessed.
func imageDecode(pdf *model.Context, sd *types.StreamDict, comps int, mask bool) ([]bool, string) {
	o, found := sd.Find("Decode")
	if !found {
		return nil, ""
	}
	a, err := pdf.DereferenceArray(o)
	if err != nil || len(a) != 2*comps {
		return nil, "decode entry"
	}
	invert := make([]bool, comps)
	for c := range comps {
		lo, err1 := pdf.DereferenceNumber(a[2*c])
		hi, err2 := pdf.DereferenceNumber(a[2*c+1])
		if err1 != nil || err2 != nil || !(lo == 0 && hi == 1 || lo == 1 && hi == 0) {
			return nil, "decode entry"
		}
		invert[c] = lo == 1
		if mask && invert[c] {
			return nil, "soft-mask decode"
		}
	}
	return invert, ""
}

func reencode(ctx context.Context, job *imageJob, opts options) encoding {
	sd := &job.sd
	defer func() { sd.Content = nil }()
	if err := ctx.Err(); err != nil {
		return encoding{err: err}
	}
	w, h, bits := *sd.IntEntry("Width"), *sd.IntEntry("Height"), *sd.IntEntry("BitsPerComponent")
	comps := job.comps
	expected, ok := sampleSize(w, h, comps, bits)
	if !ok {
		return preserve("too large")
	}
	if bits == 1 && comps != 1 {
		return preserve("multichannel 1-bit")
	}
	if bits == 1 && opts.monoCodecs != "" && opts.monoCodecs != "flate" {
		return preserve("external monochrome codec")
	}
	kind := filterKind(sd.FilterPipeline)
	if opts.lossless && kind == "jpeg" {
		return preserve("lossy original")
	}
	var samples []byte
	switch kind {
	case "lossless":
		// pdfcpu applies the limit to every filter stage. Intermediate stages hold
		// compressed data or predictor rows, so allow twice the final sample size.
		if err := sd.DecodeWithLimit(min(maxImageBytes, 2*(expected+int64(h)))); err != nil {
			return preserve("undecodable")
		}
		if int64(len(sd.Content)) != expected {
			return preserve("length mismatch")
		}
		samples = sd.Content
	case "jpeg":
		if comps == 4 {
			return preserve("CMYK JPEG")
		}
		config, err := jpeg.DecodeConfig(bytes.NewReader(sd.Raw))
		if err != nil || config.Width != w || config.Height != h {
			return preserve("undecodable jpeg")
		}
		img, err := jpeg.Decode(contextReader{ctx, bytes.NewReader(sd.Raw)})
		if ctx.Err() != nil {
			return encoding{err: ctx.Err()}
		}
		if err != nil {
			return preserve("undecodable jpeg")
		}
		var actual int
		samples, actual, err = jpegSamples(ctx, img)
		if err != nil {
			return encoding{err: err}
		}
		if actual == 1 && comps == 3 && job.device {
			rgb := make([]byte, 3*len(samples))
			for i, value := range samples {
				rgb[3*i], rgb[3*i+1], rgb[3*i+2] = value, value, value
			}
			samples, actual = rgb, 3
		}
		if samples == nil || actual != comps {
			return preserve("jpeg color model")
		}
		bits = 8
	default:
		return preserve(kind)
	}
	if err := ctx.Err(); err != nil {
		return encoding{err: err}
	}
	out := encoding{bpc: bits, clearDecode: job.invert != nil}
	if job.invert != nil {
		samples = bytes.Clone(samples)
		for i := range samples {
			if i%65536 == 0 && ctx.Err() != nil {
				return encoding{err: ctx.Err()}
			}
			component := 0
			if bits >= 8 {
				component = i / (bits / 8) % comps
			}
			if job.invert[component] {
				samples[i] = ^samples[i]
			}
		}
	}
	if bits == 16 && opts.reduceBits && !job.softMask {
		samples = highBytes(samples)
		bits, out.bpc = 8, 8
	}
	if comps == 3 && job.device && !opts.lossless && opts.colorReduction != "preserve" && allGray(samples, bits) {
		samples, comps, out.gray = grayChannel(samples, bits), 1, true
	}
	if bits == 1 {
		return encodePacked(ctx, sd, samples, out, opts)
	}
	bilevel := bits == 8 && comps == 1 && !opts.lossless && opts.colorReduction != "preserve" && !job.softMask && job.device && onlyBlackAndWhite(samples)
	var img image.Image
	switch {
	case comps == 4:
		// Go's PNG/JPEG encoders cannot preserve CMYK sample semantics.
		if !listContains(opts.imageCodecs, "flate") {
			return preserve("no permitted CMYK codec")
		}
		data, err := deflateSamples(ctx, samples, opts.compression == "heavy")
		if err != nil {
			return encoding{err: err}
		}
		out.data, out.filter = data, "FlateDecode"
		return keepEncoding(sd, out, opts.force)
	case bilevel:
		out.bpc = 1
		p := image.NewPaletted(image.Rect(0, 0, w, h), color.Palette{color.Gray{Y: 0}, color.Gray{Y: 255}})
		for i, v := range samples {
			if v != 0 {
				p.Pix[i] = 1
			}
		}
		img = p
	case comps == 1 && bits == 16:
		img = &image.Gray16{Pix: samples, Stride: w * 2, Rect: image.Rect(0, 0, w, h)}
	case comps == 1:
		img = &image.Gray{Pix: samples, Stride: w, Rect: image.Rect(0, 0, w, h)}
	case bits == 16:
		rgba := image.NewNRGBA64(image.Rect(0, 0, w, h))
		for y := range h {
			if err := ctx.Err(); err != nil {
				return encoding{err: err}
			}
			for x := range w {
				i := y*w + x
				copy(rgba.Pix[8*i:8*i+6], samples[6*i:6*i+6])
				rgba.Pix[8*i+6], rgba.Pix[8*i+7] = 255, 255
			}
		}
		img = rgba
	default:
		rgba := image.NewRGBA(image.Rect(0, 0, w, h))
		for y := range h {
			if err := ctx.Err(); err != nil {
				return encoding{err: err}
			}
			for x := range w {
				i := y*w + x
				copy(rgba.Pix[4*i:4*i+3], samples[3*i:3*i+3])
				rgba.Pix[4*i+3] = 255
			}
		}
		img = rgba
	}
	if listContains(opts.imageCodecs, "flate") || bilevel || job.softMask {
		data, err := pngIDAT(ctx, img, opts.compression == "heavy")
		if err != nil {
			return encoding{err: err}
		}
		out.data, out.filter = data, "FlateDecode"
		out.parms = types.Dict{"Predictor": types.Integer(15), "Colors": types.Integer(comps), "BitsPerComponent": types.Integer(out.bpc), "Columns": types.Integer(w)}
	}
	if opts.compression == "heavy" && !bilevel && (listContains(opts.imageCodecs, "flate") || job.softMask) {
		// Some scan samples compress better without PNG row predictors.
		data, err := deflateSamples(ctx, samples, true)
		if err != nil {
			return encoding{err: err}
		}
		if out.data == nil || len(data) < len(out.data) {
			out.data, out.filter, out.parms = data, "FlateDecode", nil
		}
	}
	if bits == 8 && !bilevel && !job.softMask && !opts.lossless && listContains(opts.imageCodecs, "jpeg") {
		var buf bytes.Buffer
		if err := jpeg.Encode(contextWriter{ctx, &buf}, img, &jpeg.Options{Quality: opts.imageQuality}); err != nil {
			return encoding{err: err}
		}
		if out.data == nil || buf.Len() < len(out.data) {
			out.data, out.filter, out.parms = buf.Bytes(), "DCTDecode", nil
		}
	}
	if out.data == nil {
		return preserve("no permitted codec")
	}
	return keepEncoding(sd, out, opts.force)
}

func encodePacked(ctx context.Context, sd *types.StreamDict, samples []byte, out encoding, opts options) encoding {
	data, err := deflateSamples(ctx, samples, opts.compression == "heavy")
	if err != nil {
		return encoding{err: err}
	}
	out.data, out.filter, out.bpc = data, "FlateDecode", 1
	return keepEncoding(sd, out, opts.force)
}

func deflateSamples(ctx context.Context, samples []byte, best bool) ([]byte, error) {
	var buf bytes.Buffer
	level := zlib.DefaultCompression
	if best {
		level = zlib.BestCompression
	}
	w, err := zlib.NewWriterLevel(contextWriter{ctx, &buf}, level)
	if err != nil {
		return nil, err
	}
	_, err = io.Copy(w, contextReader{ctx, bytes.NewReader(samples)})
	err = errors.Join(err, w.Close())
	return buf.Bytes(), err
}

func keepEncoding(sd *types.StreamDict, out encoding, force bool) encoding {
	saved := len(sd.Raw) - len(out.data)
	if !force && (saved < minImageSavings || saved < (len(sd.Raw)+49)/50) {
		return preserve("no gain")
	}
	return out
}

func applyEncoding(entry *model.XRefTableEntry, sd types.StreamDict, out encoding) {
	sd.Raw, sd.Content = out.data, nil
	length := int64(len(out.data))
	sd.StreamLength, sd.StreamLengthObjNr = &length, nil
	sd.Update("Length", types.Integer(length))
	sd.Update("BitsPerComponent", types.Integer(out.bpc))
	sd.Update("Filter", types.Name(out.filter))
	if out.parms == nil {
		sd.Delete("DecodeParms")
	} else {
		sd.Update("DecodeParms", out.parms)
	}
	sd.FilterPipeline = []types.PDFFilter{{Name: out.filter, DecodeParms: out.parms}}
	if out.gray {
		sd.Update("ColorSpace", types.Name("DeviceGray"))
	}
	if out.clearDecode {
		sd.Delete("Decode")
	}
	entry.Object = sd
}

func hasDefaultColorSpace(pdf *model.Context) bool {
	var contains func(types.Object, int) bool
	contains = func(obj types.Object, depth int) bool {
		if depth > 100 {
			return true
		}
		switch o := obj.(type) {
		case types.Dict:
			if _, ok := o["DefaultRGB"]; ok {
				return true
			}
			if _, ok := o["DefaultGray"]; ok {
				return true
			}
			for _, value := range o {
				if contains(value, depth+1) {
					return true
				}
			}
		case types.StreamDict:
			return contains(o.Dict, depth+1)
		case types.Array:
			for _, value := range o {
				if contains(value, depth+1) {
					return true
				}
			}
		}
		return false
	}
	for _, entry := range pdf.Table {
		if entry != nil && !entry.Free && contains(entry.Object, 0) {
			return true
		}
	}
	return false
}

func components(pdf *model.Context, sd *types.StreamDict) (int, bool, string) {
	cs, err := pdf.Dereference(sd.Dict["ColorSpace"])
	if err != nil {
		return 0, false, "color space"
	}
	switch cs := cs.(type) {
	case types.Name:
		switch cs {
		case "DeviceGray":
			return 1, true, ""
		case "DeviceRGB":
			return 3, true, ""
		case "DeviceCMYK":
			return 4, true, ""
		}
		return 0, false, "color space " + string(cs)
	case types.Array:
		if len(cs) == 0 {
			return 0, false, "color space"
		}
		name, ok := cs[0].(types.Name)
		if !ok {
			return 0, false, "color space"
		}
		if name == "ICCBased" && len(cs) == 2 {
			if profile, _, err := pdf.DereferenceStreamDict(cs[1]); err == nil && profile != nil {
				if n := profile.IntEntry("N"); n != nil && (*n == 1 || *n == 3 || *n == 4) {
					return *n, false, ""
				}
			}
		}
		return 0, false, "color space " + string(name)
	}
	return 0, false, "color space"
}

func filterKind(pipeline []types.PDFFilter) string {
	if len(pipeline) == 1 && pipeline[0].Name == "DCTDecode" {
		return "jpeg"
	}
	for _, f := range pipeline {
		switch f.Name {
		case "FlateDecode", "LZWDecode", "ASCIIHexDecode", "ASCII85Decode", "RunLengthDecode", "CCITTFaxDecode":
		default:
			return "filter " + f.Name
		}
	}
	return "lossless"
}

func highBytes(samples []byte) []byte {
	out := make([]byte, len(samples)/2)
	for i := range out {
		out[i] = samples[2*i]
	}
	return out
}

func jpegSamples(ctx context.Context, img image.Image) ([]byte, int, error) {
	b := img.Bounds()
	switch img := img.(type) {
	case *image.Gray:
		return img.Pix, 1, nil
	case *image.YCbCr:
		out := make([]byte, 0, b.Dx()*b.Dy()*3)
		for y := b.Min.Y; y < b.Max.Y; y++ {
			if err := ctx.Err(); err != nil {
				return nil, 0, err
			}
			yOffset := img.YOffset(b.Min.X, y)
			for x := b.Min.X; x < b.Max.X; x++ {
				cOffset := img.COffset(x, y)
				r, g, bl := color.YCbCrToRGB(img.Y[yOffset+x-b.Min.X], img.Cb[cOffset], img.Cr[cOffset])
				out = append(out, r, g, bl)
			}
		}
		return out, 3, nil
	case *image.RGBA, *image.NRGBA:
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
	return nil, 0, nil
}

func allGray(rgb []byte, bits int) bool {
	n := bits / 8
	for i := 0; i+3*n <= len(rgb); i += 3 * n {
		if !bytes.Equal(rgb[i:i+n], rgb[i+n:i+2*n]) || !bytes.Equal(rgb[i:i+n], rgb[i+2*n:i+3*n]) {
			return false
		}
	}
	return true
}
func grayChannel(rgb []byte, bits int) []byte {
	n := bits / 8
	out := make([]byte, len(rgb)/3)
	for i := 0; i < len(out); i += n {
		copy(out[i:i+n], rgb[3*i:3*i+n])
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

// PNG IDAT data is exactly the zlib stream expected by FlateDecode/Predictor 15.
func pngIDAT(ctx context.Context, img image.Image, best bool) ([]byte, error) {
	var file bytes.Buffer
	encoder := png.Encoder{}
	if best {
		encoder.CompressionLevel = png.BestCompression
	}
	if err := encoder.Encode(contextWriter{ctx, &file}, img); err != nil {
		return nil, err
	}
	var idat []byte
	data := file.Bytes()[8:]
	for len(data) >= 12 {
		length := int(data[0])<<24 | int(data[1])<<16 | int(data[2])<<8 | int(data[3])
		if length < 0 || length > len(data)-12 {
			return nil, errors.New("truncated PNG chunk")
		}
		if string(data[4:8]) == "IDAT" {
			idat = append(idat, data[8:8+length]...)
		}
		data = data[12+length:]
	}
	return idat, nil
}
