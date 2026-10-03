package main

import (
	"bytes"
	"context"
	"embed"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
	"sync"

	"github.com/pdfcpu/pdfcpu/pkg/api"
	"github.com/pdfcpu/pdfcpu/pkg/pdfcpu/model"
)

// The Python helpers and the ICC sRGB profile (International Color Consortium,
// freely distributable) travel inside the binary and are unpacked per run.
//
//go:embed tools/pdf_tools.py tools/pdfa.py tools/sRGB2014.icc
var toolFiles embed.FS

// Bound diagnostic output without blocking a child on an unread pipe.
// imageError is tracked even when the retained diagnostic text is truncated.
type boundedBuffer struct {
	data                  bytes.Buffer
	truncated, imageError bool
	tail                  []byte
}

func (b *boundedBuffer) Bytes() []byte  { return b.data.Bytes() }
func (b *boundedBuffer) String() string { return b.data.String() }

func (b *boundedBuffer) Write(p []byte) (int, error) {
	combined := append(b.tail, p...)
	b.imageError = b.imageError || bytes.Contains(combined, []byte("recoverable image error"))
	b.tail = bytes.Clone(combined[max(0, len(combined)-64):])
	n := min(len(p), (1<<20)-b.data.Len())
	b.data.Write(p[:n])
	b.truncated = b.truncated || n != len(p)
	return len(p), nil
}

type lockedWriter struct {
	mu     sync.Mutex
	writer io.Writer
}

func (w *lockedWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.writer.Write(p)
}

func toolError(ctx context.Context, name string, err error, output *boundedBuffer, stderr io.Writer, secrets ...string) error {
	text := output.String()
	for _, secret := range secrets {
		if secret != "" {
			text = strings.ReplaceAll(text, secret, "[redacted]")
			if output.truncated {
				for n := min(len(secret)-1, len(text)); n > 0; n-- {
					if strings.HasSuffix(text, secret[:n]) {
						text = text[:len(text)-n] + "[redacted]"
						break
					}
				}
			}
		}
	}
	if text != "" {
		fmt.Fprint(stderr, text)
		if !strings.HasSuffix(text, "\n") {
			fmt.Fprintln(stderr)
		}
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	if err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	return nil
}

func qpdf(ctx context.Context, args []string, password string, stderr io.Writer) error {
	path, err := exec.LookPath("qpdf")
	if err != nil {
		return errors.New("qpdf is required for inline images and encrypted input; install it and add it to PATH")
	}
	// qpdf's @- reads one argument per line. Passwords never enter argv or logs.
	var private, public, secrets []string
	if password != "" {
		private = append(private, "--password="+password)
		secrets = append(secrets, password)
	}
	for _, arg := range args {
		if strings.HasPrefix(arg, "--encryption-file-password=") {
			secret := strings.TrimPrefix(arg, "--encryption-file-password=")
			if strings.ContainsAny(secret, "\r\n\x00") {
				return errors.New("invalid encryption password")
			}
			private = append(private, arg)
			secrets = append(secrets, secret)
		} else {
			public = append(public, arg)
		}
	}
	if len(private) > 0 {
		public = append([]string{"@-"}, public...)
	}
	cmd := exec.CommandContext(ctx, path, public...)
	cmd.Stdin = strings.NewReader(strings.Join(private, "\n") + "\n")
	var messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &messages, &messages
	configureCommand(cmd)
	// A qpdf warning is a failure: requested transformations must not silently lose data.
	return toolError(ctx, "qpdf", cmd.Run(), &messages, stderr, secrets...)
}

func pythonPDF(ctx context.Context, operation, input, output string, opts options, stderr io.Writer) error {
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	path, err := exec.LookPath(python)
	if err != nil {
		return errors.New("Python 3 is required for this operation; set PDF_SQUEEZER_PYTHON to a Python environment with the optional PDF tools")
	}
	dir, err := os.MkdirTemp(filepath.Dir(output), "pdf-tools-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(dir)
	for _, name := range []string{"pdf_tools.py", "pdfa.py", "sRGB2014.icc"} {
		data, err := toolFiles.ReadFile("tools/" + name)
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(dir, name), data, 0600); err != nil {
			return err
		}
	}
	fontFiles := map[string]string{}
	for _, setting := range opts.fontFiles {
		name, file, _ := strings.Cut(setting, "=")
		fontFiles[name] = file
	}
	request := map[string]any{
		"dpi": opts.dpi, "gray_dpi": opts.grayDPI, "mono_dpi": opts.monoDPI, "threshold": opts.dpiThreshold,
		"quality": opts.imageQuality, "clip": opts.clip, "extended": opts.extended, "lossless": opts.lossless,
		"reduce_bits": opts.reduceBits, "force": opts.force, "mono_codecs": opts.monoCodecs,
		"codecs": opts.imageCodecs, "memory_mib": opts.imageMemory,
		"gray": opts.gray && opts.engine == "pdfcpu", "geometry": opts.engine == "pdfcpu",
		"flatten": opts.flatten, "subset_fonts": opts.subsetFonts, "merge_fonts": opts.mergeFonts,
		"bitmap": opts.bitmap, "mrc": opts.mrc, "render_dpi": opts.renderDPI, "background_dpi": opts.backgroundDPI,
		"verbose": opts.verbose, "strip": opts.strip, "font_files": fontFiles, "output_intent": opts.outputIntent,
		"srgb_icc": filepath.Join(dir, "sRGB2014.icc"), "pdfa": opts.pdfa, "object_streams": !opts.classicXref,
	}
	if operation == "text" {
		request["password"] = opts.password
	}
	data, err := json.Marshal(request)
	if err != nil {
		return err
	}
	cmd := exec.CommandContext(ctx, path, "-I", filepath.Join(dir, "pdf_tools.py"), operation, input, output)
	cmd.Stdin = bytes.NewReader(data)
	var messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &messages, &messages
	configureCommand(cmd)
	return toolError(ctx, "PDF tools", cmd.Run(), &messages, stderr, opts.password)
}

// pdfaIdentification reads the PDF/A part and conformance a document declares in
// its XMP, as veraPDF flavour names: "2b", "4", "4f". Undeclared documents give "".
func pdfaIdentification(pdf *model.Context) (string, error) {
	metadata, _, err := pdf.DereferenceStreamDict(pdf.RootDict["Metadata"])
	if err != nil || metadata == nil {
		return "", nil
	}
	if err := metadata.Decode(); err != nil {
		return "", err
	}
	part := xmpProperty(metadata.Content, "part")
	conformance := strings.ToLower(xmpProperty(metadata.Content, "conformance"))
	switch {
	case part == "":
		return "", nil
	case part == "4" && listContains(",e,f", conformance):
		return part + conformance, nil
	case listContains("1,2,3", part) && listContains("a,b,u", conformance):
		return part + conformance, nil
	}
	return "", fmt.Errorf("unsupported PDF/A identification part %q conformance %q", part, conformance)
}

// xmpProperty finds a PDF/A identification property by namespace and local
// name, whether it is written as an attribute or as an element, with any prefix.
func xmpProperty(xmp []byte, name string) string {
	const pdfaid = "http://www.aiim.org/pdfa/ns/id/"
	decoder := xml.NewDecoder(bytes.NewReader(xmp))
	for {
		token, err := decoder.Token()
		if err != nil {
			return ""
		}
		element, ok := token.(xml.StartElement)
		if !ok {
			continue
		}
		for _, attr := range element.Attr {
			if attr.Name.Space == pdfaid && attr.Name.Local == name {
				return strings.TrimSpace(attr.Value)
			}
		}
		if element.Name.Space == pdfaid && element.Name.Local == name {
			var value string
			if err := decoder.DecodeElement(&value, &element); err != nil {
				return ""
			}
			return strings.TrimSpace(value)
		}
	}
}

// declaredFlavour reads the identification from a file on disk.
func declaredFlavour(ctx context.Context, path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	pdf, err := api.ReadContext(ctx, f, configuration(""))
	if err != nil {
		return "", err
	}
	return pdfaIdentification(pdf)
}

var veraObject = regexp.MustCompile(`\((\d+) \d+ obj [A-Za-z]+\)|\(([^()]+)\)$`)

// veraPDF clause families, longest prefix first. Hints name the option that resolves them.
var pdfaCategories = []struct{ prefix, category, hint string }{
	{"6.2.10", "fonts", "embed the font in the source document or pass --font-file NAME=PATH"},
	{"6.2.11", "fonts", "embed the font in the source document or pass --font-file NAME=PATH"},
	{"6.2.3", "color", "pass --output-intent with a valid ICC output or display profile"},
	{"6.2.4", "color", "pass --output-intent with an ICC profile for the color spaces the document uses"},
	{"6.2.9", "color", "pass --output-intent with an ICC profile for the color spaces the document uses"},
	{"6.2", "graphics", "the converter should repair this; report it with the object numbers"},
	{"6.3", "annotations", "use --flatten annotations, or --strip annotations, hidden, or multimedia"},
	{"6.4", "forms", "use --strip xfa or --strip actions"},
	{"6.5", "actions", "use --strip actions"},
	{"6.6", "actions or metadata", "use --strip actions; metadata should be repaired by the converter, report it"},
	{"6.7", "metadata", "the converter should repair this; report it with the object numbers"},
	{"6.8", "attachments", "use --strip attachments, or --pdfa 3b for arbitrary attachments"},
	{"6.9", "attachments", "use --strip attachments, or embed only PDF/A attachments"},
	{"6.1", "structure", "the converter should repair this; report it with the object numbers"},
	{"6", "structure", "the converter should repair this; report it with the object numbers"},
}

type veraReport struct {
	Jobs []struct {
		Item struct {
			Name string `xml:"name"`
		} `xml:"item"`
		Validation *struct {
			Profile   string `xml:"profileName,attr"`
			Compliant bool   `xml:"isCompliant,attr"`
			Rules     []struct {
				Clause      string `xml:"clause,attr"`
				Test        string `xml:"testNumber,attr"`
				Status      string `xml:"status,attr"`
				Description string `xml:"description"`
				Checks      []struct {
					Status  string `xml:"status,attr"`
					Context string `xml:"context"`
				} `xml:"check"`
			} `xml:"details>rule"`
		} `xml:"validationReport"`
	} `xml:"jobs>job"`
}

var errNoPDFAValidator = errors.New("PDF/A validation requires veraPDF on PATH")

// validatePDFA runs veraPDF once over every path with one flavour and returns an
// error per path: nil for compliant files, diagnostics grouped by what resolves
// them otherwise. The second result reports a failure to run or read veraPDF.
func validatePDFA(ctx context.Context, flavour string, paths []string, stderr io.Writer) (map[string]error, error) {
	binary, err := exec.LookPath("verapdf")
	if err != nil {
		return nil, errNoPDFAValidator
	}
	absolute := make([]string, len(paths))
	for i, path := range paths {
		if absolute[i], err = filepath.Abs(path); err != nil {
			return nil, err
		}
	}
	args := append([]string{"--format", "xml", "--flavour", flavour, "--maxfailuresdisplayed", "3"}, absolute...)
	if runtime.GOOS == "windows" && strings.EqualFold(filepath.Ext(binary), ".bat") {
		// Launch the installed CLI jar directly: cmd.exe would reinterpret paths
		// containing shell metacharacters or environment-variable expansions.
		resolved, err := filepath.EvalSymlinks(binary)
		if err != nil {
			return nil, err
		}
		jars, err := filepath.Glob(filepath.Join(filepath.Dir(resolved), "bin", "cli-*.jar"))
		if err != nil || len(jars) != 1 {
			return nil, errors.New("cannot locate veraPDF's CLI jar next to its Windows launcher")
		}
		binary, err = exec.LookPath("java")
		if err != nil {
			return nil, errors.New("veraPDF requires Java on PATH")
		}
		args = append([]string{"-jar", jars[0]}, args...)
	}
	cmd := exec.CommandContext(ctx, binary, args...)
	var report, messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &report, &messages
	configureCommand(cmd)
	// veraPDF exits nonzero for noncompliant files; the report decides.
	runErr := cmd.Run()
	var kept bytes.Buffer
	for _, line := range bytes.SplitAfter(messages.Bytes(), []byte("\n")) {
		// JVM banners such as "Picked up JAVA_TOOL_OPTIONS" are not diagnostics.
		if !bytes.HasPrefix(line, []byte("Picked up ")) {
			kept.Write(line)
		}
	}
	messages.data = kept
	if len(report.Bytes()) == 0 {
		if err := toolError(ctx, "veraPDF", runErr, &messages, stderr); err != nil {
			return nil, err
		}
		return nil, errors.New("veraPDF produced no report")
	}
	if err := toolError(ctx, "veraPDF", nil, &messages, stderr); err != nil {
		return nil, err
	}
	if report.truncated {
		return nil, errors.New("veraPDF report exceeded its size limit")
	}
	var parsed veraReport
	if err := xml.Unmarshal(report.Bytes(), &parsed); err != nil {
		return nil, fmt.Errorf("veraPDF report: %w", err)
	}
	wantProfile := "PDF/A-" + strings.ToUpper(flavour)
	results := map[string]error{}
	for _, job := range parsed.Jobs {
		if job.Validation == nil {
			continue
		}
		result := job.Validation
		if result.Compliant && strings.Contains(strings.ToUpper(result.Profile), wantProfile) {
			results[job.Item.Name] = nil
			continue
		}
		lines := []string{fmt.Sprintf("veraPDF did not confirm %s compliance; output was not published", wantProfile)}
		for _, rule := range result.Rules {
			if rule.Status != "failed" {
				continue
			}
			category, hint := "structure", pdfaCategories[len(pdfaCategories)-1].hint
			for _, c := range pdfaCategories {
				if rule.Clause == c.prefix || strings.HasPrefix(rule.Clause, c.prefix+".") {
					category, hint = c.category, c.hint
					break
				}
			}
			var objects []string
			for _, check := range rule.Checks {
				// Keep the innermost object number and the trailing label, e.g. "object 14 Helvetica".
				var object, label string
				for _, m := range veraObject.FindAllStringSubmatch(check.Context, -1) {
					if m[1] != "" {
						object = "object " + m[1]
					} else {
						label = m[2]
					}
				}
				if text := strings.TrimSpace(object + " " + label); text != "" {
					objects = append(objects, text)
				}
			}
			description := strings.Join(strings.Fields(rule.Description), " ")
			if len(description) > 160 {
				description = description[:157] + "..."
			}
			line := fmt.Sprintf("  %s %s-%s: %s", category, rule.Clause, rule.Test, description)
			if len(objects) > 0 {
				line += " [" + strings.Join(objects, "; ") + "]"
			}
			lines = append(lines, line, "    "+hint)
		}
		results[job.Item.Name] = errors.New(strings.Join(lines, "\n"))
	}
	for i, path := range paths {
		if _, ok := results[absolute[i]]; !ok {
			results[path] = errors.New("veraPDF did not report on this file")
		} else if absolute[i] != path {
			results[path] = results[absolute[i]]
		}
	}
	return results, nil
}

func extract(ctx context.Context, input, output string, opts options, stderr io.Writer) (err error) {
	f, err := os.Open(input)
	if err != nil {
		return err
	}
	defer f.Close()
	if err := checkPDF(f); err != nil {
		return err
	}
	work, err := os.MkdirTemp(filepath.Dir(output), ".pdf-squeezer-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(work)
	if opts.extract == "text" {
		staged := filepath.Join(work, "text.txt")
		if err := pythonPDF(ctx, "text", input, staged, opts, stderr); err != nil {
			return err
		}
		source, err := os.Open(staged)
		if err != nil {
			return err
		}
		defer source.Close()
		_, err = writeNew(ctx, output, source)
		return err
	}
	if opts.inline {
		staged := filepath.Join(work, "inline.pdf")
		if err := qpdf(ctx, []string{"--externalize-inline-images", "--ii-min-bytes=0", input, staged}, opts.password, stderr); err != nil {
			return err
		}
		prepared, err := os.Open(staged)
		if err != nil {
			return err
		}
		defer prepared.Close()
		f = prepared
	}
	imagesDir := filepath.Join(work, "images")
	if err := os.Mkdir(imagesDir, 0700); err != nil {
		return err
	}
	seen := map[int]bool{}
	err = api.ExtractImages(ctx, f, nil, func(img model.Image, _ bool, objNr int) error {
		if seen[objNr] {
			return nil
		}
		seen[objNr] = true
		if !listContains("png,jpg,jpeg,tif,tiff,jp2,jpx,pbm", img.FileType) {
			return fmt.Errorf("unsupported extracted image format %q", img.FileType)
		}
		name := fmt.Sprintf("image-%06d.%s", objNr, img.FileType)
		_, err := writeNew(ctx, filepath.Join(imagesDir, name), img.Reader)
		return err
	}, configuration(opts.password))
	if err != nil {
		return err
	}
	entries, err := os.ReadDir(imagesDir)
	if err != nil {
		return err
	}
	if err := os.Mkdir(output, 0700); err != nil {
		return err
	}
	defer func() {
		if err != nil {
			err = errors.Join(err, os.RemoveAll(output))
		}
	}()
	for _, entry := range entries {
		source, openErr := os.Open(filepath.Join(imagesDir, entry.Name()))
		if openErr != nil {
			return openErr
		}
		_, copyErr := writeNew(ctx, filepath.Join(output, entry.Name()), source)
		if err := errors.Join(copyErr, source.Close()); err != nil {
			return err
		}
	}
	return nil
}
