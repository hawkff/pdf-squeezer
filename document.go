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

// pdfcpu v0.16 deletes PieceInfo and frees its entire referenced graph during
// optimization. Detach the entries first, then restore them before writing so
// private streams (including shared objects) remain live. Explicit stripping
// happens later in transformDocument without freeing potentially shared objects.
func optimizeDocument(ctx context.Context, pdf *model.Context) error {
	type pieceInfo struct {
		dict   types.Dict
		value  types.Object
		marked bool
	}
	// A temporary identity prevents Forms with detached private data from
	// comparing equal. Choose a key absent from every stream dictionary.
	const prefix = "pdfSqueezerPieceInfo"
	marker := prefix
	for _, entry := range pdf.Table {
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry == nil || entry.Free {
			continue
		}
		if sd, ok := entry.Object.(types.StreamDict); ok {
			for key := range sd.Dict {
				if strings.HasPrefix(key, prefix) && len(key) >= len(marker) {
					marker = key + "_"
				}
			}
		}
	}
	var saved []pieceInfo
	defer func() {
		for _, item := range saved {
			item.dict["PieceInfo"] = item.value
			if item.marked {
				delete(item.dict, marker)
			}
		}
	}()
	for n, entry := range pdf.Table {
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry == nil || entry.Free {
			continue
		}
		var d types.Dict
		switch o := entry.Object.(type) {
		case types.Dict:
			d = o
		case types.StreamDict:
			d = o.Dict
		}
		if value, found := d["PieceInfo"]; found {
			typ, _ := pdf.Dereference(d["Type"])
			subtype, _ := pdf.Dereference(d["Subtype"])
			_, stream := entry.Object.(types.StreamDict)
			if typ == types.Name("Catalog") || typ == types.Name("Page") || stream && subtype == types.Name("Form") {
				saved = append(saved, pieceInfo{d, value, stream})
				delete(d, "PieceInfo")
				if stream {
					d[marker] = types.Integer(n)
				}
			}
		}
	}
	return api.OptimizeContext(ctx, pdf)
}

// The pdfcpu writer copies lazy object-stream dictionaries without traversing
// or encrypting their values. Resolve them before writing their graphs.
func materializeObjectGraph(ctx context.Context, pdf *model.Context, obj types.Object, seen map[int]bool, depth int) error {
	var pending []types.IndirectRef
	var walkDirect func(types.Object, int) error
	walkDirect = func(obj types.Object, depth int) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		if ref, ok := obj.(types.IndirectRef); ok {
			n := ref.ObjectNumber.Value()
			if !seen[n] {
				seen[n] = true
				pending = append(pending, ref)
			}
			return nil
		}
		if depth > 100 {
			return errors.New("object nesting exceeds materialization limit")
		}
		switch o := obj.(type) {
		case types.StreamDict:
			return walkDirect(o.Dict, depth+1)
		case types.Dict:
			for _, value := range o {
				if err := walkDirect(value, depth+1); err != nil {
					return err
				}
			}
		case types.Array:
			for _, value := range o {
				if err := walkDirect(value, depth+1); err != nil {
					return err
				}
			}
		}
		return nil
	}
	if err := walkDirect(obj, depth); err != nil {
		return err
	}
	for len(pending) > 0 {
		if err := ctx.Err(); err != nil {
			return err
		}
		i := len(pending) - 1
		ref := pending[i]
		pending = pending[:i]
		value, err := pdf.Dereference(ref)
		if err != nil {
			return err
		}
		// Indirect links start a new direct object; chain length is not nesting.
		if err := walkDirect(value, 0); err != nil {
			return err
		}
	}
	return nil
}

// After a re-read, surviving aliases may no longer have a PieceInfo discovery
// root. Materialize reachable dictionaries for writing, without decoding streams.
func materializeWriteRoots(ctx context.Context, pdf *model.Context) error {
	seen := map[int]bool{}
	for _, ref := range []*types.IndirectRef{pdf.Root, pdf.Info} {
		if ref != nil {
			if err := materializeObjectGraph(ctx, pdf, *ref, seen, 0); err != nil {
				return err
			}
		}
	}
	return nil
}

func materializeDocumentPieceInfo(ctx context.Context, pdf *model.Context) error {
	seen := map[int]bool{}
	for _, entry := range pdf.Table {
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry == nil || entry.Free {
			continue
		}
		var d types.Dict
		switch o := entry.Object.(type) {
		case types.Dict:
			d = o
		case types.StreamDict:
			d = o.Dict
		}
		if value, found := d["PieceInfo"]; found {
			if err := materializeObjectGraph(ctx, pdf, value, seen, 0); err != nil {
				return err
			}
		}
	}
	return nil
}

func transformDocument(ctx context.Context, pdf *model.Context, opts options) error {
	// Resolve shared descendants before stripping removes their discovery root.
	if err := materializeDocumentPieceInfo(ctx, pdf); err != nil {
		return err
	}
	if opts.privacy {
		if err := stripMetadata(pdf); err != nil {
			return err
		}
	}
	updateDates := opts.timestamps == "now" || opts.timestamps == "modified"
	if len(opts.metadata) > 0 || updateDates {
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
		if updateDates && !opts.privacy {
			date := types.StringLiteral("D:" + time.Now().UTC().Format("20060102150405") + "Z")
			info["ModDate"] = date
			if opts.timestamps == "now" {
				info["CreationDate"] = date
			}
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
		typ := ""
		if name, err := pdf.Dereference(d["Type"]); err == nil {
			if name, ok := name.(types.Name); ok {
				typ = string(name)
			}
		}
		_, stream := entry.Object.(types.StreamDict)
		// Resource names are arbitrary. An untyped stream needs an information
		// object owner or an XML/XMP subtype, not just a resource named Metadata.
		if opts.privacy || len(opts.metadata) > 0 || updateDates || listContains(opts.strip, "metadata") {
			if metadata, _, err := pdf.DereferenceStreamDict(d["Metadata"]); err == nil && metadata != nil {
				metadataType, err := pdf.Dereference(metadata.Dict["Type"])
				metadataSubtype, _ := pdf.Dereference(metadata.Dict["Subtype"])
				xmp := metadataSubtype == types.Name("XML") || metadataSubtype == types.Name("XMP")
				if err == nil && (metadataType == types.Name("Metadata") || metadataType == nil && (typ != "" || stream || xmp)) {
					delete(d, "Metadata")
				}
			}
		}
		if opts.privacy || listContains(opts.strip, "piece-info") {
			subtype, _ := pdf.Dereference(d["Subtype"])
			if typ == "Catalog" || typ == "Page" || typ == "XObject" || stream && (subtype == types.Name("Form") || subtype == types.Name("Image")) {
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
	if opts.privacy || listContains(opts.strip, "web-capture") {
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

func streamDictionary(sd types.StreamDict) types.Dict {
	d := sd.Dict.Clone().(types.Dict)
	delete(d, "Length")
	return d
}

func streamDigest(key string, data []byte) [32]byte {
	h := sha256.New()
	fmt.Fprintf(h, "%d:", len(key))
	io.WriteString(h, key)
	h.Write(data)
	var digest [32]byte
	copy(digest[:], h.Sum(nil))
	return digest
}

// normalizedStreamDictionary only normalizes generalized lossless filters.
// Encoded image filters retain parameters, including bitonal polarity and globals.
func normalizedStreamDictionary(sd types.StreamDict) (types.Dict, bool) {
	d := streamDictionary(sd)
	normalize := true
	for _, f := range sd.FilterPipeline {
		switch f.Name {
		case "FlateDecode", "LZWDecode", "ASCIIHexDecode", "ASCII85Decode", "RunLengthDecode":
		default:
			normalize = false
		}
	}
	if normalize {
		delete(d, "Filter")
		delete(d, "DecodeParms")
	}
	return d, normalize
}

func streamIdentity(sd types.StreamDict) (string, []byte, error) {
	d, normalize := normalizedStreamDictionary(sd)
	data := sd.Raw
	if normalize {
		if err := sd.DecodeWithLimit(32 << 20); err != nil {
			return "", nil, err
		}
		data = sd.Content
	}
	return d.PDFString(), data, nil
}

func deduplicateStreams(ctx context.Context, pdf *model.Context) error {
	seen := map[[32]byte]int{}
	rawSeen := map[[32]byte]int{}
	replace := map[int]types.IndirectRef{}
	var ids []int
	keys := map[int]string{}
	peers := map[string]int{}
	for n, e := range pdf.Table {
		if err := ctx.Err(); err != nil {
			return err
		}
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
		d, _ := normalizedStreamDictionary(sd)
		key := d.PDFString()
		keys[n] = key
		peers[key]++
		ids = append(ids, n)
	}
	sort.Ints(ids)
	for _, n := range ids {
		if err := ctx.Err(); err != nil {
			return err
		}
		// Different normalized dictionaries cannot match, regardless of bytes.
		if peers[keys[n]] < 2 {
			continue
		}
		e := pdf.Table[n]
		sd := e.Object.(types.StreamDict)
		// Exact encodings need no decoding, including aliases re-encoded together.
		rawKey := streamDictionary(sd).PDFString()
		rawDigest := streamDigest(rawKey, sd.Raw)
		if original, found := rawSeen[rawDigest]; found {
			other := pdf.Table[original]
			otherStream := other.Object.(types.StreamDict)
			if len(sd.Content) == 0 && len(otherStream.Content) == 0 && rawKey == streamDictionary(otherStream).PDFString() && bytes.Equal(sd.Raw, otherStream.Raw) {
				generation := 0
				if other.Generation != nil {
					generation = *other.Generation
				}
				replace[n] = *types.NewIndirectRef(original, generation)
				continue
			}
		} else {
			rawSeen[rawDigest] = n
		}
		key, data, err := streamIdentity(sd)
		if err != nil {
			continue
		} // Unsupported/oversized streams stay unchanged.
		digest := streamDigest(key, data)
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
	conf := configuration("")
	conf.Cmd = model.ENCRYPT
	conf.UserPW, conf.OwnerPW = opts.encryptUser, opts.encryptOwner
	conf.EncryptUsingAES, conf.EncryptKeyLength = true, 256
	conf.Permissions = model.PermissionsAll
	if opts.permissions == "print" {
		conf.Permissions = model.PermissionsPrint
	}
	if opts.permissions == "none" {
		conf.Permissions = model.PermissionsNone
	}
	// Use the same preparation as api.Encrypt, but resolve reachable dictionaries
	// in the context that will be encrypted, not just in the preceding stage.
	pdf, err := api.ReadValidateAndOptimize(ctx, in, conf, nil)
	if err != nil {
		return err
	}
	if err := materializeWriteRoots(ctx, pdf); err != nil {
		return err
	}
	out, err := os.OpenFile(output, os.O_WRONLY|os.O_TRUNC, 0600)
	if err != nil {
		return err
	}
	err = api.WriteContext(ctx, pdf, out)
	return errors.Join(err, out.Close())
}
