package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const maxProfileBytes = 1 << 20

type profileBundle struct {
	Version  int       `json:"version"`
	Profiles []profile `json:"profiles"`
}

// Tier settings are defaults, below explicit profile values and command-line flags.
func compressionFlags(name string) (map[string]string, error) {
	quality, dpi := 0, 0
	switch name {
	case "light":
		quality = 90
	case "balanced":
		quality = 85
	case "medium":
		quality, dpi = 65, 150
	case "strong":
		quality, dpi = 45, 120
	case "heavy":
		quality, dpi = 30, 96
	default:
		return nil, errors.New("--compression must be light, balanced, medium, strong, or heavy")
	}
	return map[string]string{
		"images": "true", "image-quality": strconv.Itoa(quality),
		"reduce-bit-depth": strconv.FormatBool(name != "light"),
		"dpi":              strconv.Itoa(dpi), "clip-images": strconv.FormatBool(dpi > 0),
		"inline-images": strconv.FormatBool(dpi > 0),
		"merge-fonts":   strconv.FormatBool(name == "heavy"),
	}, nil
}

func decodeJSON(data []byte, target any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(target); err != nil {
		return err
	}
	if decoder.Decode(new(any)) != io.EOF {
		return errors.New("profile has trailing data")
	}
	return nil
}

func loadProfiles(ctx context.Context, path string, stderr io.Writer) ([]profile, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, maxProfileBytes+1))
	if err != nil {
		return nil, err
	}
	if len(data) > maxProfileBytes {
		return nil, errors.New("profile exceeds 1 MiB")
	}
	trimmed := bytes.TrimSpace(data)
	if strings.EqualFold(filepath.Ext(path), ".pdfscp") || bytes.HasPrefix(data, []byte("bplist")) || bytes.HasPrefix(trimmed, []byte("<")) {
		return loadLegacyProfiles(ctx, data, stderr)
	}
	var header struct {
		Version int `json:"version"`
	}
	if err := json.Unmarshal(data, &header); err != nil {
		return nil, fmt.Errorf("profile: %w", err)
	}
	var profiles []profile
	switch header.Version {
	case 1:
		var p profile
		if err := decodeJSON(data, &p); err != nil {
			return nil, fmt.Errorf("profile: %w", err)
		}
		profiles = []profile{p}
	case 2:
		var bundle profileBundle
		if err := decodeJSON(data, &bundle); err != nil {
			return nil, fmt.Errorf("profile bundle: %w", err)
		}
		profiles = bundle.Profiles
	default:
		return nil, errors.New("unsupported profile version; expected 1 or bundle version 2")
	}
	if len(profiles) == 0 {
		return nil, errors.New("profile bundle is empty")
	}
	for _, p := range profiles {
		if p.Version != 1 {
			return nil, errors.New("bundle entries must be version 1 profiles")
		}
	}
	return profiles, nil
}

func selectProfile(profiles []profile, selector string) (int, error) {
	if selector == "" {
		return 0, nil
	}
	if len(profiles) == 0 {
		return 0, errors.New("--profile-entry requires --profile")
	}
	selected := -1
	for i, p := range profiles {
		if p.Name == selector {
			if selected >= 0 {
				return 0, errors.New("profile name is ambiguous; select a 1-based index")
			}
			selected = i
		}
	}
	if selected >= 0 {
		return selected, nil
	}
	index, err := strconv.Atoi(selector)
	if err != nil || index < 1 || index > len(profiles) {
		return 0, fmt.Errorf("profile entry %q not found; choose a name or index 1..%d", selector, len(profiles))
	}
	return index - 1, nil
}

// plistlib handles both binary and XML plists without executing archived objects.
// Imported files contain an ordered array of settings dictionaries.
func loadLegacyProfiles(ctx context.Context, data []byte, stderr io.Writer) ([]profile, error) {
	python := os.Getenv("PDF_SQUEEZER_PYTHON")
	if python == "" {
		python = "python3"
	}
	path, err := exec.LookPath(python)
	if err != nil {
		return nil, errors.New(".pdfscp import requires Python 3 with its standard-library plistlib; set PDF_SQUEEZER_PYTHON or add python3 to PATH")
	}
	ctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, path, "-I", "-c", "import json, plistlib, sys\ntry:\n json.dump(plistlib.loads(sys.stdin.buffer.read()), sys.stdout, allow_nan=False)\nexcept Exception:\n sys.exit(1)\n")
	cmd.Stdin = bytes.NewReader(data)
	var output, messages boundedBuffer
	cmd.Stdout, cmd.Stderr = &output, &messages
	configureCommand(cmd)
	if err := toolError(ctx, ".pdfscp property list", cmd.Run(), &messages, stderr); err != nil {
		return nil, err
	}
	if output.truncated {
		return nil, errors.New("decoded profile exceeds 1 MiB")
	}
	var dictionaries []map[string]json.RawMessage
	if err := decodeJSON(output.Bytes(), &dictionaries); err != nil || len(dictionaries) == 0 {
		return nil, errors.New(".pdfscp must contain a nonempty property-list array of settings dictionaries")
	}
	profiles := make([]profile, len(dictionaries))
	for i, settings := range dictionaries {
		p, err := legacyProfile(settings)
		if err != nil {
			return nil, fmt.Errorf(".pdfscp entry %d: %w", i+1, err)
		}
		profiles[i] = p
	}
	return profiles, nil
}

func legacyProfile(settings map[string]json.RawMessage) (profile, error) {
	p := profile{Version: 1, Flags: map[string]string{}}
	bools := map[string]string{
		"optimizeImages": "images", "clipImages": "clip-images",
		"forceRecompression": "force-recompression", "convertToCFF": "convert-fonts-cff",
		"mergeEmbeddedFonts": "merge-fonts", "removeStandardFonts": "remove-standard-fonts",
		"subsetFonts": "subset-fonts", "createPDFA4": "pdfa",
	}
	strips := map[string]string{
		"stripMetadata": "metadata", "stripPieceInfo": "piece-info", "stripThreads": "threads",
		"stripThumbnails": "thumbnails", "stripSpiderInfo": "web-capture",
		"stripAlternateImages": "alternates", "stripDocumentStructureTree": "tags",
		"stripTheDocumentOutputIntents": "output-intents", "removeAllImages": "images",
	}
	flatten := map[string]string{
		"stripAndFlattenFormFields": "forms", "stripAndFlattenLinkAnnotations": "links",
		"stripAndFlattenAnnotationsExceptFormFieldsAndLinks": "annotations",
		"flattenRemainingAnnotations":                        "annotations",
	}
	values := map[string]bool{}
	text := map[string]string{}
	numbers := map[string]float64{}
	for key, raw := range settings {
		switch key {
		case "title", "uuid", "customTitle", "customAuthor", "customSubject", "customKeywords", "customCreator", "customProducer":
			var value string
			if bytes.Equal(raw, []byte("null")) || json.Unmarshal(raw, &value) != nil {
				return p, fmt.Errorf("%s must be a string", key)
			}
			text[key] = value
		case "imageQuality", "imageResolution", "colorConversion", "rank":
			var value float64
			if bytes.Equal(raw, []byte("null")) || json.Unmarshal(raw, &value) != nil || math.IsNaN(value) || math.IsInf(value, 0) {
				return p, fmt.Errorf("%s must be a finite number", key)
			}
			numbers[key] = value
		default:
			_, a := bools[key]
			_, b := strips[key]
			_, c := flatten[key]
			if !a && !b && !c && !listContains("reduceColorComplexity,downsampleMonochromeScans,optimizeResources,removeRedundantObjects,removeTitle,removeAuthor,removeSubject,removeKeywords,removeCreator,removeProducer,updateModificationDate,updateModificationAndCreationDate,hasBeenCreatedByApp", key) {
				return p, fmt.Errorf("unsupported profile field %q", key)
			}
			var value bool
			if bytes.Equal(raw, []byte("null")) {
				return p, fmt.Errorf("%s must be a boolean", key)
			}
			if json.Unmarshal(raw, &value) != nil {
				var n int
				if json.Unmarshal(raw, &n) != nil || n != 0 && n != 1 {
					return p, fmt.Errorf("%s must be a boolean or 0/1", key)
				}
				value = n == 1
			}
			values[key] = value
		}
	}
	p.Name = text["title"]
	for key, flag := range bools {
		if value, present := values[key]; present {
			if key == "createPDFA4" {
				if value {
					p.Flags[flag] = "4"
				}
			} else {
				p.Flags[flag] = strconv.FormatBool(value)
			}
		}
	}
	if _, present := values["optimizeImages"]; !present {
		values["optimizeImages"] = true
		p.Flags["images"] = "true"
	}
	if !values["optimizeImages"] {
		// Stored image settings are inactive when image optimization is disabled.
		p.Flags["clip-images"], p.Flags["force-recompression"] = "false", "false"
	}
	if values["optimizeImages"] {
		quality, qOK := numbers["imageQuality"]
		dpi, dOK := numbers["imageResolution"]
		if !qOK || !dOK || quality < 0 || quality > 1 || dpi < 0 || dpi > 9600 || math.Trunc(dpi) != dpi {
			return p, errors.New("image optimization requires imageQuality in 0..1 and integer imageResolution in 0..9600")
		}
		// Profile quality uses truncation, not rounding, with a minimum of one.
		p.Flags["image-quality"] = strconv.Itoa(max(1, int(quality*100)))
		p.Flags["dpi"] = strconv.Itoa(int(dpi))
		if values["downsampleMonochromeScans"] {
			p.Flags["mono-dpi"] = p.Flags["dpi"]
		}
	}
	if value, present := values["reduceColorComplexity"]; present {
		p.Flags["color-reduction"] = "preserve"
		if value {
			p.Flags["color-reduction"] = "exact"
		}
	}
	if conversion := numbers["colorConversion"]; conversion != 0 {
		return p, fmt.Errorf("colorConversion %g has no verified mapping; export with color conversion disabled, or use a native profile with --gray", conversion)
	}
	for _, key := range []string{"optimizeResources", "removeRedundantObjects"} {
		if value, present := values[key]; present && !value {
			return p, fmt.Errorf("%s=false is unsupported: structural optimization is always enabled", key)
		}
	}
	appendFlag := func(flag, item string) {
		if !listContains(p.Flags[flag], item) {
			if p.Flags[flag] != "" {
				p.Flags[flag] += ","
			}
			p.Flags[flag] += item
		}
	}
	// Fixed ordering keeps profile exports deterministic.
	for _, key := range []string{"stripMetadata", "stripPieceInfo", "stripThreads", "stripThumbnails", "stripSpiderInfo", "stripAlternateImages", "stripDocumentStructureTree", "stripTheDocumentOutputIntents", "removeAllImages"} {
		if values[key] {
			appendFlag("strip", strips[key])
		}
	}
	for _, key := range []string{"stripAndFlattenFormFields", "stripAndFlattenLinkAnnotations", "stripAndFlattenAnnotationsExceptFormFieldsAndLinks", "flattenRemainingAnnotations"} {
		if values[key] {
			appendFlag("flatten", flatten[key])
		}
	}
	for _, key := range []string{"Title", "Author", "Subject", "Keywords", "Creator", "Producer"} {
		if values["remove"+key] {
			if key == "Producer" && text["customProducer"] == "" {
				return p, errors.New("automatic Producer replacement is unsupported; supply a customProducer or use native metadata editing")
			}
			p.Metadata = append(p.Metadata, key+"="+text["custom"+key])
		}
	}
	p.Flags["timestamps"] = "preserve"
	if values["updateModificationDate"] {
		p.Flags["timestamps"] = "modified"
	}
	if values["updateModificationAndCreationDate"] {
		p.Flags["timestamps"] = "now"
	}
	return p, nil
}
