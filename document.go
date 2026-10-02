package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
	"time"
	"unicode/utf16"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/types"
)

func pdfString(s string) types.HexLiteral {
	data := []byte{0xfe, 0xff}
	for _, n := range utf16.Encode([]rune(s)) {
		data = append(data, byte(n>>8), byte(n))
	}
	return types.HexLiteral(fmt.Sprintf("%x", data))
}

func stripMetadata(pdf *model.Context) error {
	ref, err := pdf.IndRefForNewObject(types.NewDict())
	if err != nil {
		return err
	}
	pdf.Info, pdf.ID = ref, nil
	return nil
}

func transformDocument(ctx context.Context, pdf *model.Context, opts options) error {
	if opts.privacy {
		if err := stripMetadata(pdf); err != nil {
			return err
		}
	}
	if len(opts.metadata) > 0 || opts.timestamps == "now" {
		var info types.Dict
		var err error
		if pdf.Info != nil {
			info, err = pdf.DereferenceDict(*pdf.Info)
			if err != nil {
				return err
			}
		}
		if info == nil {
			info = types.NewDict()
			pdf.Info, err = pdf.IndRefForNewObject(info)
			if err != nil {
				return err
			}
		}
		if opts.timestamps == "now" && !opts.privacy {
			date := types.StringLiteral("D:" + time.Now().UTC().Format("20060102150405") + "Z")
			info["CreationDate"], info["ModDate"] = date, date
		}
		for _, setting := range opts.metadata {
			key, value, _ := strings.Cut(setting, "=")
			if value == "" {
				delete(info, key)
			} else if key == "CreationDate" || key == "ModDate" {
				info[key] = types.StringLiteral(value)
			} else {
				info[key] = pdfString(value)
			}
		}
	}
	removeImages := listContains(opts.strip, "images")
	for _, entry := range pdf.Table {
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry == nil || entry.Free {
			continue
		}
		if _, ok := imageStream(entry); ok && removeImages {
			data := []byte("q Q\n")
			length := int64(len(data))
			entry.Object = types.StreamDict{Dict: types.Dict{"Type": types.Name("XObject"), "Subtype": types.Name("Form"), "BBox": types.NewIntegerArray(0, 0, 1, 1), "Length": types.Integer(length)}, Raw: data, StreamLength: &length}
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
		// Resource names are arbitrary. A resource named /Metadata or /B must
		// not be mistaken for a metadata field or a page's article-bead array.
		if opts.privacy || len(opts.metadata) > 0 || opts.timestamps == "now" || listContains(opts.strip, "metadata") {
			if metadata, _, err := pdf.DereferenceStreamDict(d["Metadata"]); err == nil && metadata != nil {
				if typ := metadata.NameEntry("Type"); typ != nil && *typ == "Metadata" {
					delete(d, "Metadata")
				}
			}
		}
		typ := ""
		if name := d.NameEntry("Type"); name != nil {
			typ = *name
		}
		if opts.privacy || listContains(opts.strip, "piece-info") {
			if typ == "Catalog" || typ == "Page" || typ == "XObject" {
				delete(d, "PieceInfo")
				delete(d, "LastModified")
			}
		}
		if typ == "Page" {
			if removeImages || listContains(opts.strip, "thumbnails") {
				delete(d, "Thumb")
			}
			if listContains(opts.strip, "threads") {
				delete(d, "B")
			}
		}
		if st := d.NameEntry("Subtype"); st != nil && *st == "Image" && listContains(opts.strip, "alternates") {
			delete(d, "Alternates")
		}
		if listContains(opts.strip, "tags") {
			for _, key := range []string{"StructParent", "StructParents"} {
				if value, err := pdf.Dereference(d[key]); err == nil {
					if _, ok := value.(types.Integer); ok {
						delete(d, key)
					}
				}
			}
		}
		if opts.removeStandardFonts {
			removeStandardFont(d, pdf)
		}
		if d.NameEntry("Type") == nil || *d.NameEntry("Type") != "Page" {
			continue
		}
		annots, err := pdf.DereferenceArray(d["Annots"])
		if err != nil {
			return err
		}
		var retained types.Array
		for _, a := range annots {
			ad, err := pdf.DereferenceDict(a)
			if err != nil {
				return err
			}
			kind := "annotations"
			if st := ad.NameEntry("Subtype"); st != nil {
				if *st == "Widget" {
					kind = "forms"
				}
				if *st == "Link" {
					kind = "links"
				}
			}
			if !listContains(opts.strip, kind) {
				retained = append(retained, a)
			}
		}
		if len(annots) != len(retained) {
			if len(retained) == 0 {
				delete(d, "Annots")
			} else {
				d["Annots"] = retained
			}
		}
	}
	if listContains(opts.strip, "forms") {
		delete(pdf.RootDict, "AcroForm")
	}
	if listContains(opts.strip, "threads") {
		delete(pdf.RootDict, "Threads")
	}
	if listContains(opts.strip, "output-intents") {
		delete(pdf.RootDict, "OutputIntents")
	}
	if listContains(opts.strip, "tags") {
		delete(pdf.RootDict, "StructTreeRoot")
		delete(pdf.RootDict, "MarkInfo")
	}
	if opts.privacy {
		delete(pdf.RootDict, "SpiderInfo")
	}
	return nil
}

func documentDates(pdf *model.Context) (map[string]types.Object, error) {
	dates := map[string]types.Object{}
	if pdf.Info == nil {
		return dates, nil
	}
	info, err := pdf.DereferenceDict(*pdf.Info)
	if err != nil {
		return nil, err
	}
	for _, key := range []string{"CreationDate", "ModDate"} {
		value, err := pdf.Dereference(info[key])
		if err != nil {
			return nil, err
		}
		switch value.(type) {
		case types.StringLiteral, types.HexLiteral:
			dates[key] = value
		}
	}
	return dates, nil
}

func restoreDocumentDates(pdf *model.Context, dates map[string]types.Object) error {
	if len(dates) == 0 {
		return nil
	}
	var info types.Dict
	var err error
	if pdf.Info != nil {
		info, err = pdf.DereferenceDict(*pdf.Info)
		if err != nil {
			return err
		}
	}
	if info == nil {
		info = types.NewDict()
		pdf.Info, err = pdf.IndRefForNewObject(info)
		if err != nil {
			return err
		}
	}
	for key, value := range dates {
		info[key] = value
	}
	return nil
}

func removeStandardFont(d types.Dict, pdf *model.Context) {
	name, subtype := d.NameEntry("BaseFont"), d.NameEntry("Subtype")
	if name == nil || subtype == nil || (*subtype != "Type1" && *subtype != "TrueType") {
		return
	}
	if !listContains("Courier,Courier-Bold,Courier-Oblique,Courier-BoldOblique,Helvetica,Helvetica-Bold,Helvetica-Oblique,Helvetica-BoldOblique,Times-Roman,Times-Bold,Times-Italic,Times-BoldItalic,Symbol,ZapfDingbats", *name) {
		return
	}
	if o, found := d.Find("Encoding"); found {
		n, ok := o.(types.Name)
		if !ok || !listContains("WinAnsiEncoding,MacRomanEncoding,StandardEncoding", string(n)) {
			return
		}
	}
	if _, err := pdf.DereferenceDict(d["FontDescriptor"]); err != nil {
		return
	}
	delete(d, "FontDescriptor")
	d["Subtype"] = types.Name("Type1")
}

// streamIdentity only normalizes generalized lossless filters. Encoded image
// filters retain their Filter/DecodeParms, including bitonal polarity and globals.
func streamIdentity(sd types.StreamDict) (string, []byte, error) {
	d := sd.Dict.Clone().(types.Dict)
	delete(d, "Length")
	normalize := true
	for _, f := range sd.FilterPipeline {
		switch f.Name {
		case "FlateDecode", "LZWDecode", "ASCIIHexDecode", "ASCII85Decode", "RunLengthDecode":
		default:
			normalize = false
		}
	}
	data := sd.Raw
	if normalize {
		if err := sd.DecodeWithLimit(32 << 20); err != nil {
			return "", nil, err
		}
		data = sd.Content
		delete(d, "Filter")
		delete(d, "DecodeParms")
	}
	return d.PDFString(), data, nil
}

func deduplicateStreams(ctx context.Context, pdf *model.Context) error {
	seen := map[[32]byte]int{}
	replace := map[int]types.IndirectRef{}
	var ids []int
	for n := range pdf.Table {
		ids = append(ids, n)
	}
	sort.Ints(ids)
	for _, n := range ids {
		if err := ctx.Err(); err != nil {
			return err
		}
		e := pdf.Table[n]
		if e == nil || e.Free {
			continue
		}
		sd, ok := e.Object.(types.StreamDict)
		if !ok {
			continue
		}
		if typ := sd.NameEntry("Type"); typ != nil && (*typ == "XRef" || *typ == "ObjStm") {
			continue
		}
		key, data, err := streamIdentity(sd)
		if err != nil {
			continue
		} // Unsupported/oversized streams stay unchanged.
		h := sha256.New()
		fmt.Fprintf(h, "%d:", len(key))
		io.WriteString(h, key)
		h.Write(data)
		var digest [32]byte
		copy(digest[:], h.Sum(nil))
		original, found := seen[digest]
		if !found {
			seen[digest] = n
			continue
		}
		other := pdf.Table[original]
		otherStream := other.Object.(types.StreamDict)
		otherKey, otherData, err := streamIdentity(otherStream)
		if err != nil || key != otherKey || !bytes.Equal(data, otherData) {
			continue
		}
		// Keep the smallest encoding, not whichever object happens to come first.
		duplicate, retained, entry := n, original, other
		if len(sd.Raw) < len(otherStream.Raw) {
			duplicate, retained, entry = original, n, e
			seen[digest] = n
		}
		generation := 0
		if entry.Generation != nil {
			generation = *entry.Generation
		}
		replace[duplicate] = *types.NewIndirectRef(retained, generation)
	}
	if len(replace) == 0 {
		return nil
	}
	// Later candidates can supersede earlier representatives. Compress these
	// chains once before rewriting references, rather than leaving dangling aliases.
	for nr := range replace {
		var path []int
		ref := replace[nr]
		for current := nr; ; {
			next, ok := replace[current]
			if !ok {
				break
			}
			path = append(path, current)
			ref, current = next, next.ObjectNumber.Value()
		}
		for _, n := range path {
			replace[n] = ref
		}
	}
	var rewrite func(types.Object, int) (types.Object, error)
	rewrite = func(obj types.Object, depth int) (types.Object, error) {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		if depth > 100 {
			return nil, errors.New("object nesting exceeds deduplication limit")
		}
		switch o := obj.(type) {
		case types.IndirectRef:
			if replacement, ok := replace[o.ObjectNumber.Value()]; ok {
				return replacement, nil
			}
		case types.Dict:
			for k, v := range o {
				value, err := rewrite(v, depth+1)
				if err != nil {
					return nil, err
				}
				o[k] = value
			}
		case types.StreamDict:
			if _, err := rewrite(o.Dict, depth+1); err != nil {
				return nil, err
			}
		case types.Array:
			for i, v := range o {
				value, err := rewrite(v, depth+1)
				if err != nil {
					return nil, err
				}
				o[i] = value
			}
		}
		return obj, nil
	}
	for _, entry := range pdf.Table {
		if entry == nil || entry.Free {
			continue
		}
		obj, err := rewrite(entry.Object, 0)
		if err != nil {
			return err
		}
		entry.Object = obj
	}
	return nil
}

func encryptPDF(ctx context.Context, input, output string, opts options) error {
	in, err := os.Open(input)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(output, os.O_WRONLY|os.O_TRUNC, 0600)
	if err != nil {
		return err
	}
	conf := configuration("")
	conf.UserPW, conf.OwnerPW = opts.encryptUser, opts.encryptOwner
	conf.EncryptUsingAES, conf.EncryptKeyLength = true, 256
	conf.Permissions = model.PermissionsAll
	if opts.permissions == "print" {
		conf.Permissions = model.PermissionsPrint
	}
	if opts.permissions == "none" {
		conf.Permissions = model.PermissionsNone
	}
	err = api.Encrypt(ctx, in, out, conf)
	return errors.Join(err, out.Close())
}
