package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

const usage = `Usage: pdf-squeezer [flags] input.pdf [more.pdf | directory ...]

Compress PDFs without overwriting inputs or existing outputs. Directory inputs
include PDFs only; --recursive includes subdirectories. Batch outputs retain
relative directories. Advanced operations require optional tools; see README.

General:
  --engine NAME           pdfcpu (default) or ghostscript
  -o, --output PATH        output file or existing directory
  --recursive             recurse into directory inputs; skip *.squeezed.pdf
  --collision POLICY      error (default) or number
  --profile FILE          load a versioned JSON compression profile
  --save-profile FILE     save settings without paths or passwords
  --timestamps POLICY     preserve (default) or now
  -V, --verbose           report stages, image progress, and preserved images
  -v, --version           print version
  -h, --help              print help

Images:
  --images                re-encode supported images; may use lossy JPEG
  --lossless              only lossless image transformations; implies --images
  --image-quality N       JPEG quality, 1..100 (default 75)
  --image-codecs LIST     flate,jpeg (default), flate, or jpeg
  --mono-codecs LIST      flate (default), ccitt, jbig2, or a comma-separated list
  --reduce-bit-depth      allow 16-bit to 8-bit conversion; never for soft masks
  --force-recompression   keep requested re-encoding even without size savings
  --image-memory N        native image working-buffer budget in MiB (default 512)
  --dpi N                 color/gray target DPI; 0 keeps resolution
  --gray-dpi N            override the gray target DPI
  --mono-dpi N            monochrome target DPI; 0 keeps resolution
  --dpi-threshold N       downsample above this multiple of target DPI (default 1.5)
  --clip-images           crop unused image margins; conservative shared-use analysis
  --inline-images         externalize inline images before optimization
  --extended-images       also decode JPEG 2000 and re-encode CMYK images
  --gray                  convert all page colors to grayscale
  --quality PRESET        Ghostscript: screen, ebook (default), printer, prepress

Document:
  --privacy               remove document info, XMP, piece info, web capture data
  --metadata KEY=VALUE    set Title, Author, Subject, Keywords, Creator, Producer,
                          CreationDate or ModDate; repeatable; empty removes a key
  --strip LIST            thumbnails,alternates,threads,tags,output-intents,piece-info,
                          metadata,links,annotations,forms,images,actions,multimedia,
                          hidden,xfa,attachments; opt-in removals
  --flatten LIST          forms,annotations,links or all; freezes appearances
  --subset-fonts          subset eligible embedded fonts
  --merge-fonts           merge compatible embedded TrueType subsets
  --convert-fonts-cff     rewrite through Ghostscript with compact font programs
  --remove-standard-fonts remove embedded copies of the standard PDF fonts

Security and conversion:
  --password-file FILE    input password, first line; never passed in command arguments
  --decrypt               explicitly remove input encryption
  --encrypt-user-file FILE  output user password, first line
  --encrypt-owner-file FILE output owner password; AES-256; required with user password
  --permissions POLICY    output encryption permissions: all (default), print, none
  --extract MODE          images or text; output is a new directory or text file
  --bitmap                render pages to an image-only PDF; loses text/interactive data
  --mrc                   image-only layered PDF for scans; preserves fine dark text
  --render-dpi N          bitmap/MRC rendering resolution (default 200)
  --background-dpi N      MRC background resolution (default 72)
  --pdfa LEVEL            convert to PDF/A 2b, 3b, or 4 (4f with attachments); veraPDF
                          must confirm. Inputs that declare PDF/A keep their level.
  --output-intent FILE    ICC profile for the PDF/A output intent (default: bundled sRGB)
  --font-file NAME=PATH   font program for a non-embedded font; repeatable
`

type options struct {
	engine, quality, output, collision, timestamps               string
	dpi, grayDPI, monoDPI, imageQuality, imageMemory             int
	dpiThreshold                                                 float64
	gray, images, privacy, verbose, recursive                    bool
	lossless, reduceBits, force, clip, inline, extended          bool
	imageCodecs, monoCodecs, strip, flatten                      string
	subsetFonts, mergeFonts, cffFonts, removeStandardFonts       bool
	passwordFile, encryptUserFile, encryptOwnerFile, permissions string
	password, encryptUser, encryptOwner                          string
	decrypt, bitmap, mrc                                         bool
	extract, outputIntent, pdfa                                  string
	renderDPI, backgroundDPI                                     int
	metadata, fontFiles                                          stringList
	classicXref                                                  bool // set per input that declares PDF/A-1
}

type stringList []string

func (s *stringList) String() string { return strings.Join(*s, "\n") }
func (s *stringList) Set(v string) error {
	*s = append(*s, v)
	return nil
}

type profile struct {
	Version  int               `json:"version"`
	Flags    map[string]string `json:"flags"`
	Metadata []string          `json:"metadata,omitempty"`
}

func parseFlags(flags *flag.FlagSet, args []string) ([]string, error) {
	var inputs []string
	for {
		if err := flags.Parse(args); err != nil {
			return nil, err
		}
		rest := flags.Args()
		consumed := len(args) - len(rest)
		if len(rest) == 0 || consumed > 0 && args[consumed-1] == "--" {
			return append(inputs, rest...), nil
		}
		inputs = append(inputs, rest[0])
		args = rest[1:]
	}
}

func profileFlag(name string) bool {
	switch name {
	case "o", "output", "profile", "save-profile", "password-file", "encrypt-user-file", "encrypt-owner-file", "metadata", "v", "version", "V", "verbose", "recursive", "collision", "extract", "output-intent", "font-file":
		return false
	}
	return true
}

func parseOptions(ctx context.Context, args []string, stderr io.Writer) (options, []string, bool, error) {
	opts := options{}
	f := flag.NewFlagSet("pdf-squeezer", flag.ContinueOnError)
	f.SetOutput(stderr)
	f.Usage = func() { fmt.Fprint(stderr, usage) }
	f.StringVar(&opts.engine, "engine", "pdfcpu", "")
	f.StringVar(&opts.quality, "quality", "", "")
	f.StringVar(&opts.output, "output", "", "")
	f.StringVar(&opts.output, "o", "", "")
	f.StringVar(&opts.collision, "collision", "error", "")
	f.StringVar(&opts.timestamps, "timestamps", "preserve", "")
	f.BoolVar(&opts.recursive, "recursive", false, "")
	f.BoolVar(&opts.verbose, "verbose", false, "")
	f.BoolVar(&opts.verbose, "V", false, "")
	f.BoolVar(&opts.images, "images", false, "")
	f.BoolVar(&opts.lossless, "lossless", false, "")
	f.BoolVar(&opts.reduceBits, "reduce-bit-depth", false, "")
	f.BoolVar(&opts.force, "force-recompression", false, "")
	f.BoolVar(&opts.gray, "gray", false, "")
	f.BoolVar(&opts.clip, "clip-images", false, "")
	f.BoolVar(&opts.inline, "inline-images", false, "")
	f.BoolVar(&opts.extended, "extended-images", false, "")
	f.IntVar(&opts.imageQuality, "image-quality", 75, "")
	f.IntVar(&opts.imageMemory, "image-memory", 512, "")
	f.StringVar(&opts.imageCodecs, "image-codecs", "flate,jpeg", "")
	f.StringVar(&opts.monoCodecs, "mono-codecs", "flate", "")
	f.IntVar(&opts.dpi, "dpi", 0, "")
	f.IntVar(&opts.grayDPI, "gray-dpi", 0, "")
	f.IntVar(&opts.monoDPI, "mono-dpi", 0, "")
	f.Float64Var(&opts.dpiThreshold, "dpi-threshold", 1.5, "")
	f.BoolVar(&opts.privacy, "privacy", false, "")
	f.Var(&opts.metadata, "metadata", "")
	f.StringVar(&opts.strip, "strip", "", "")
	f.StringVar(&opts.flatten, "flatten", "", "")
	f.BoolVar(&opts.subsetFonts, "subset-fonts", false, "")
	f.BoolVar(&opts.mergeFonts, "merge-fonts", false, "")
	f.BoolVar(&opts.cffFonts, "convert-fonts-cff", false, "")
	f.BoolVar(&opts.removeStandardFonts, "remove-standard-fonts", false, "")
	f.StringVar(&opts.passwordFile, "password-file", "", "")
	f.BoolVar(&opts.decrypt, "decrypt", false, "")
	f.StringVar(&opts.encryptUserFile, "encrypt-user-file", "", "")
	f.StringVar(&opts.encryptOwnerFile, "encrypt-owner-file", "", "")
	f.StringVar(&opts.permissions, "permissions", "all", "")
	f.StringVar(&opts.extract, "extract", "", "")
	f.BoolVar(&opts.bitmap, "bitmap", false, "")
	f.BoolVar(&opts.mrc, "mrc", false, "")
	f.IntVar(&opts.renderDPI, "render-dpi", 200, "")
	f.IntVar(&opts.backgroundDPI, "background-dpi", 72, "")
	f.StringVar(&opts.pdfa, "pdfa", "", "")
	f.StringVar(&opts.outputIntent, "output-intent", "", "")
	f.Var(&opts.fontFiles, "font-file", "")
	var profilePath, saveProfile string
	var version bool
	f.StringVar(&profilePath, "profile", "", "")
	f.StringVar(&saveProfile, "save-profile", "", "")
	f.BoolVar(&version, "version", false, "")
	f.BoolVar(&version, "v", false, "")
	inputs, err := parseFlags(f, args)
	if err != nil || version {
		return opts, inputs, version, err
	}
	visited := map[string]bool{}
	f.Visit(func(f *flag.Flag) { visited[f.Name] = true })
	if profilePath != "" {
		file, err := os.Open(profilePath)
		if err != nil {
			return opts, nil, false, err
		}
		decoder := json.NewDecoder(io.LimitReader(file, 1<<20))
		decoder.DisallowUnknownFields()
		var p profile
		err = decoder.Decode(&p)
		if err == nil && decoder.Decode(new(any)) != io.EOF {
			err = errors.New("profile has trailing data")
		}
		file.Close()
		if err != nil {
			return opts, nil, false, fmt.Errorf("profile: %w", err)
		}
		if p.Version != 1 {
			return opts, nil, false, errors.New("unsupported profile version; expected 1")
		}
		for name, value := range p.Flags {
			if !profileFlag(name) || f.Lookup(name) == nil {
				return opts, nil, false, fmt.Errorf("profile flag %q is not permitted", name)
			}
			if !visited[name] {
				if err := f.Set(name, value); err != nil {
					return opts, nil, false, fmt.Errorf("profile flag %s: %w", name, err)
				}
			}
		}
		// Explicit metadata wins per key, applied after the profile's metadata.
		opts.metadata = append(p.Metadata, opts.metadata...)
	}
	if opts.lossless && !visited["image-codecs"] && opts.imageCodecs == "flate,jpeg" {
		opts.imageCodecs = "flate"
	}
	if err := opts.validate(); err != nil {
		return opts, nil, false, err
	}
	if saveProfile != "" {
		p := profile{Version: 1, Flags: map[string]string{}, Metadata: opts.metadata}
		f.VisitAll(func(f *flag.Flag) {
			if profileFlag(f.Name) {
				p.Flags[f.Name] = f.Value.String()
			}
		})
		data, err := json.MarshalIndent(p, "", "  ")
		if err != nil {
			return opts, nil, false, err
		}
		if _, err := writeNew(ctx, saveProfile, strings.NewReader(string(data)+"\n")); err != nil {
			return opts, nil, false, err
		}
	}
	if len(inputs) == 0 && saveProfile == "" {
		return opts, nil, false, errors.New("provide at least one input PDF or directory")
	}
	for _, secretFile := range []struct {
		path   string
		target *string
	}{
		{opts.passwordFile, &opts.password}, {opts.encryptUserFile, &opts.encryptUser}, {opts.encryptOwnerFile, &opts.encryptOwner},
	} {
		if secretFile.path == "" {
			continue
		}
		secret, err := readPassword(secretFile.path)
		if err != nil {
			return opts, nil, false, err
		}
		*secretFile.target = secret
	}
	if opts.encryptOwnerFile != "" && (opts.encryptOwner == "" || opts.encryptOwner == opts.encryptUser) {
		return opts, nil, false, errors.New("output owner password must be nonempty and different from the user password")
	}
	return opts, inputs, false, nil
}

func readPassword(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("password file: %w", err)
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, 4097))
	if err != nil {
		return "", err
	}
	if len(data) > 4096 {
		return "", errors.New("password file exceeds 4096 bytes")
	}
	s := strings.TrimSuffix(strings.TrimSuffix(string(data), "\n"), "\r")
	if strings.ContainsAny(s, "\r\n\x00") {
		return "", errors.New("password file must contain one line without NUL")
	}
	return s, nil
}

func listContains(list, item string) bool {
	for _, s := range strings.Split(list, ",") {
		if s == item {
			return true
		}
	}
	return false
}

func validList(value, allowed string) bool {
	if value == "" {
		return true
	}
	seen := map[string]bool{}
	for _, s := range strings.Split(value, ",") {
		if s == "" || seen[s] || !listContains(allowed, s) {
			return false
		}
		seen[s] = true
	}
	return true
}

func (o *options) validate() error {
	if o.engine != "pdfcpu" && o.engine != "ghostscript" {
		return errors.New("--engine must be pdfcpu or ghostscript")
	}
	if o.quality != "" && (o.engine != "ghostscript" || !listContains("screen,ebook,printer,prepress", o.quality)) {
		return errors.New("--quality requires Ghostscript and screen, ebook, printer, or prepress")
	}
	if o.engine == "ghostscript" && o.quality == "" {
		o.quality = "ebook"
	}
	for _, dpi := range []int{o.dpi, o.grayDPI, o.monoDPI} {
		if dpi < 0 || dpi > 9600 {
			return errors.New("image DPI must be between 0 and 9600")
		}
	}
	if o.dpiThreshold <= 1 || o.dpiThreshold > 10 || math.IsNaN(o.dpiThreshold) {
		return errors.New("--dpi-threshold must be greater than 1 and at most 10")
	}
	if o.imageQuality < 1 || o.imageQuality > 100 {
		return errors.New("--image-quality must be between 1 and 100")
	}
	if o.imageMemory < 16 || o.imageMemory > 8192 {
		return errors.New("--image-memory must be between 16 and 8192 MiB")
	}
	if o.imageCodecs == "" || !validList(o.imageCodecs, "flate,jpeg") || o.monoCodecs == "" || !validList(o.monoCodecs, "flate,ccitt,jbig2") {
		return errors.New("invalid image or monochrome codec list")
	}
	if !validList(o.strip, "thumbnails,alternates,threads,tags,output-intents,piece-info,metadata,links,annotations,forms,images,actions,multimedia,hidden,xfa,attachments") {
		return errors.New("invalid --strip list")
	}
	if !validList(o.flatten, "forms,annotations,links,all") {
		return errors.New("invalid --flatten list")
	}
	if listContains(o.flatten, "all") && o.flatten != "all" {
		return errors.New("--flatten all cannot be combined with other categories")
	}
	for _, kind := range []string{"forms", "annotations", "links"} {
		if listContains(o.strip, kind) && (listContains(o.flatten, kind) || o.flatten == "all") {
			return fmt.Errorf("cannot both strip and flatten %s", kind)
		}
	}
	if o.collision != "error" && o.collision != "number" {
		return errors.New("--collision must be error or number")
	}
	if o.timestamps != "preserve" && o.timestamps != "now" {
		return errors.New("--timestamps must be preserve or now")
	}
	for _, setting := range o.metadata {
		key, value, ok := strings.Cut(setting, "=")
		if !ok || !listContains("Title,Author,Subject,Keywords,Creator,Producer,CreationDate,ModDate", key) {
			return errors.New("--metadata expects a supported KEY=VALUE")
		}
		if len(value) > 65536 || strings.ContainsRune(value, 0) {
			return errors.New("invalid metadata value")
		}
		if (key == "CreationDate" || key == "ModDate") && value != "" {
			if _, err := time.Parse("D:20060102150405Z", value); err != nil {
				return errors.New("metadata dates must be valid UTC dates in D:YYYYMMDDhhmmssZ form")
			}
		}
	}
	if o.privacy && len(o.metadata) > 0 {
		return errors.New("--privacy cannot be combined with --metadata")
	}
	if o.encryptUserFile != "" && o.encryptOwnerFile == "" {
		return errors.New("--encrypt-user-file requires --encrypt-owner-file")
	}
	if !listContains("all,print,none", o.permissions) {
		return errors.New("invalid --permissions policy")
	}
	if o.permissions != "all" && o.encryptOwnerFile == "" {
		return errors.New("--permissions requires output encryption")
	}
	if o.decrypt && o.encryptOwnerFile != "" {
		return errors.New("cannot combine --decrypt with output encryption")
	}
	if o.extract != "" && !listContains("images,text", o.extract) {
		return errors.New("--extract must be images or text")
	}
	if o.bitmap && o.mrc {
		return errors.New("choose --bitmap or --mrc")
	}
	if o.renderDPI < 36 || o.renderDPI > 1200 || o.backgroundDPI < 10 || o.backgroundDPI > o.renderDPI {
		return errors.New("render DPI must be 36..1200; background DPI must be 10..render DPI")
	}
	o.pdfa = strings.ToLower(o.pdfa)
	if o.pdfa != "" && !listContains("2b,3b,4", o.pdfa) {
		return errors.New("--pdfa must be 2b, 3b, or 4")
	}
	if conflict := o.breaksPDFA(true); o.pdfa != "" && conflict != "" {
		return fmt.Errorf("PDF/A requires metadata, color information, embedded fonts, and no encryption; %s conflicts with --pdfa", conflict)
	}
	if o.pdfa == "" && (o.outputIntent != "" || len(o.fontFiles) > 0) {
		return errors.New("--output-intent and --font-file require --pdfa")
	}
	for _, setting := range o.fontFiles {
		if name, file, ok := strings.Cut(setting, "="); !ok || name == "" || file == "" {
			return errors.New("--font-file expects NAME=PATH")
		}
	}
	if o.lossless && (o.engine != "pdfcpu" || o.reduceBits || o.gray || o.dpi > 0 || o.grayDPI > 0 || o.monoDPI > 0 || o.bitmap || o.mrc || o.cffFonts || listContains(o.imageCodecs, "jpeg")) {
		return errors.New("--lossless conflicts with lossy transforms or JPEG encoding")
	}
	if o.lossless || o.reduceBits || o.force || o.clip || o.extended || o.monoCodecs != "flate" || o.inline && o.extract == "" {
		o.images = true
	}
	if o.inline && o.extract == "text" {
		return errors.New("--inline-images applies to compression or image extraction")
	}
	if o.engine == "ghostscript" && o.images {
		return errors.New("--images and its encoding options require --engine pdfcpu")
	}
	if o.extract != "" && (o.images || o.engine != "pdfcpu" || o.gray || o.dpi != 0 || o.grayDPI != 0 || o.monoDPI != 0 || o.privacy || len(o.metadata) != 0 || o.strip != "" || o.flatten != "" || o.subsetFonts || o.mergeFonts || o.cffFonts || o.removeStandardFonts || o.bitmap || o.mrc || o.pdfa != "" || o.decrypt || o.encryptOwnerFile != "") {
		return errors.New("--extract cannot be combined with document transformations")
	}
	return nil
}

// requiredOutput prevents a size fallback from undoing requested document changes.
func (o options) requiredOutput() bool {
	return o.privacy || len(o.metadata) > 0 || o.strip != "" || o.flatten != "" || o.gray || o.force || o.decrypt || o.encryptOwnerFile != "" || o.bitmap || o.mrc || o.pdfa != "" || o.timestamps == "now" || o.removeStandardFonts
}

// breaksPDFA names the first option that removes something PDF/A requires, or "".
// With conversion the converter rebuilds metadata and color afterwards, so only
// options that keep taking things away conflict; without it, any option that
// rewrites the document or its metadata ends the input's declared conformance.
func (o options) breaksPDFA(conversion bool) string {
	switch {
	case o.encryptOwnerFile != "":
		return "--encrypt-owner-file"
	case o.privacy:
		return "--privacy"
	case listContains(o.strip, "metadata"):
		return "--strip metadata"
	case listContains(o.strip, "output-intents"):
		return "--strip output-intents"
	case o.removeStandardFonts:
		return "--remove-standard-fonts"
	case conversion:
		return ""
	case len(o.metadata) > 0:
		return "--metadata"
	case o.timestamps == "now":
		return "--timestamps now"
	case o.bitmap:
		return "--bitmap"
	case o.mrc:
		return "--mrc"
	case o.engine == "ghostscript" || o.cffFonts:
		return "a Ghostscript rewrite"
	}
	return ""
}

// pythonStrip reports --strip categories that the Python tools implement.
func (o options) pythonStrip() bool {
	for _, category := range []string{"actions", "multimedia", "hidden", "xfa", "attachments"} {
		if listContains(o.strip, category) {
			return true
		}
	}
	return false
}

func (o options) advancedImages() bool {
	return o.clip || o.extended || o.monoCodecs != "flate" || o.engine == "pdfcpu" && (o.dpi > 0 || o.grayDPI > 0 || o.monoDPI > 0)
}

type inputFile struct{ path, relative string }

func generatedPDF(name string) bool {
	name = strings.TrimSuffix(strings.ToLower(name), ".pdf")
	if strings.HasSuffix(name, ".squeezed") {
		return true
	}
	at := strings.LastIndex(name, ".squeezed.")
	if at < 0 {
		return false
	}
	_, err := strconv.ParseUint(name[at+len(".squeezed."):], 10, 32)
	return err == nil
}

func expandInputs(ctx context.Context, inputs []string, recursive bool) ([]inputFile, error) {
	var files []inputFile
	seen := map[string]bool{}
	add := func(path, relative string) error {
		abs, err := filepath.Abs(path)
		if err != nil {
			return err
		}
		resolved, err := filepath.EvalSymlinks(abs)
		if err != nil {
			return err
		}
		if !seen[resolved] {
			files = append(files, inputFile{abs, relative})
			seen[resolved] = true
		}
		return nil
	}
	for _, input := range inputs {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		info, err := os.Stat(input)
		if err != nil {
			return nil, err
		}
		if !info.IsDir() {
			if err := add(input, filepath.Base(input)); err != nil {
				return nil, err
			}
			continue
		}
		err = filepath.WalkDir(input, func(path string, d os.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if d.IsDir() {
				if path != input && !recursive {
					return filepath.SkipDir
				}
				return nil
			}
			if d.Type()&os.ModeSymlink != 0 || !strings.EqualFold(filepath.Ext(path), ".pdf") || strings.HasPrefix(d.Name(), ".") || generatedPDF(d.Name()) {
				return nil
			}
			rel, err := filepath.Rel(input, path)
			if err != nil {
				return err
			}
			return add(path, rel)
		})
		if err != nil {
			return nil, err
		}
	}
	sort.Slice(files, func(i, j int) bool { return files[i].path < files[j].path })
	return files, nil
}
