"""PDF/A-4 conversion: deterministic repairs. veraPDF judges the result.

Repairs never discard page content, forms, attachments, or tags. Features that
PDF/A-4 forbids are removed only through an explicit --strip category; otherwise
the conversion stops with a message naming the object and the option.
"""

import hashlib
import io
import mimetypes
import re
import shutil
import subprocess
import zlib

import pikepdf
from pikepdf import Array, Dictionary, Name, Stream, String


class ConversionError(Exception):
    """The document needs user action before it can become PDF/A-4."""


FORBIDDEN_ANNOTATIONS = {"/Sound", "/Screen", "/Movie", "/3D", "/RichMedia"}
ALLOWED_ANNOTATIONS = {
    "/Text", "/Link", "/FreeText", "/Line", "/Square", "/Circle", "/Polygon",
    "/PolyLine", "/Highlight", "/Underline", "/Squiggly", "/StrikeOut", "/Stamp",
    "/Caret", "/Ink", "/Popup", "/Widget", "/PrinterMark", "/TrapNet",
    "/Watermark", "/Redact", "/Projection", "/FileAttachment",
}  # fmt: skip
ALLOWED_ACTIONS = {
    "/GoTo", "/GoToR", "/GoToE", "/Thread", "/URI", "/Named", "/SubmitForm",
    "/JavaScript", "/RichMediaExecute", "/GoToDp",
}  # fmt: skip
ALLOWED_NAMED_ACTIONS = {"/NextPage", "/PrevPage", "/FirstPage", "/LastPage"}
ALLOWED_ADDITIONAL_ACTIONS = {"/E", "/X", "/D", "/U", "/Fo", "/Bl"}
APPEARANCE_EXEMPT = {"/Popup", "/Link", "/Projection"}
FLAG_INVISIBLE, FLAG_HIDDEN, FLAG_PRINT, FLAG_NOVIEW, FLAG_TOGGLE = 1, 2, 4, 32, 256
STRIP_CATEGORIES = ("actions", "multimedia", "hidden", "xfa", "attachments")
# Metric-compatible families for the standard 14 fonts and their common aliases.
METRIC_COMPATIBLE = {
    "helvetica": ["Nimbus Sans", "Liberation Sans", "Arimo", "Helvetica", "Arial"],
    "times": ["Nimbus Roman", "Liberation Serif", "Tinos", "Times", "Times New Roman"],
    "courier": [
        "Nimbus Mono PS",
        "Liberation Mono",
        "Cousine",
        "Courier",
        "Courier New",
    ],
    "symbol": ["Standard Symbols PS", "Symbol"],
    "zapfdingbats": ["D050000L", "Dingbats", "ZapfDingbats"],
}
FAMILY_ALIASES = {
    "arial": "helvetica",
    "timesnewroman": "times",
    "couriernew": "courier",
    "dingbats": "zapfdingbats",
}
# Built-in encoding of the standard Symbol font (ISO 32000-2 Annex D.5).
SYMBOL_ENCODING = (
    "32 space exclam universal numbersign existential percent ampersand suchthat"
    " parenleft parenright asteriskmath plus comma minus period slash zero one two"
    " three four five six seven eight nine colon semicolon less equal greater question"
    " congruent Alpha Beta Chi Delta Epsilon Phi Gamma Eta Iota theta1 Kappa Lambda Mu"
    " Nu Omicron Pi Theta Rho Sigma Tau Upsilon sigma1 Omega Xi Psi Zeta bracketleft"
    " therefore bracketright perpendicular underscore radicalex alpha beta chi delta"
    " epsilon phi gamma eta iota phi1 kappa lambda mu nu omicron pi theta rho sigma tau"
    " upsilon omega1 omega xi psi zeta braceleft bar braceright similar"
    " 160 Euro Upsilon1 minute lessequal fraction infinity florin club diamond heart"
    " spade arrowboth arrowleft arrowup arrowright arrowdown degree plusminus second"
    " greaterequal multiply proportional partialdiff bullet divide notequal equivalence"
    " approxequal ellipsis arrowvertex arrowhorizex carriagereturn aleph Ifraktur"
    " Rfraktur weierstrass circlemultiply circleplus emptyset intersection union"
    " propersuperset reflexsuperset notsubset propersubset reflexsubset element"
    " notelement angle gradient registerserif copyrightserif trademarkserif product"
    " radical dotmath logicalnot logicaland logicalor arrowdblboth arrowdblleft"
    " arrowdblup arrowdblright arrowdbldown lozenge angleleft registersans"
    " copyrightsans trademarksans summation parenlefttp parenleftex parenleftbt"
    " bracketlefttp bracketleftex bracketleftbt bracelefttp braceleftmid braceleftbt"
    " braceex 241 angleright integral integraltp integralex integralbt parenrighttp"
    " parenrightex parenrightbt bracketrighttp bracketrightex bracketrightbt"
    " bracerighttp bracerightmid bracerightbt"
)


def symbol_encoding():
    names, code = [".notdef"] * 256, 0
    for token in SYMBOL_ENCODING.split():
        if token.isdigit():
            code = int(token)
        else:
            names[code] = token
            code += 1
    return names


def inherited(obj, key, default=None):
    seen = set()
    for _ in range(100):
        if key in obj:
            return obj[key]
        identity = obj.objgen
        if identity in seen or "/Parent" not in obj:
            return default
        seen.add(identity)
        obj = obj.Parent
    raise ValueError("page inheritance exceeds depth limit")


GENERALIZED_FILTERS = {
    "/FlateDecode", "/LZWDecode", "/ASCIIHexDecode", "/ASCII85Decode", "/RunLengthDecode",
}  # fmt: skip


def objref(obj):
    if getattr(obj, "is_indirect", False):
        return f"object {obj.objgen[0]} {obj.objgen[1]}"
    return "(direct object)"


def pdf_name(value):
    return str(value) if isinstance(value, Name) else None


def strip_hint(category):
    return f"use --strip {category} to remove it"


# --- content traversal -----------------------------------------------------


def content_streams(pdf):
    """Yield (owner, resources, owns_resources) for every content stream a viewer runs.

    Streams are reached through the operators that invoke them, so unused
    resources are skipped. Resource dictionaries become indirect on the way. A
    stream without its own resources that is reached from a second, different
    resource context is cloned for that context, so binding resources to it
    never changes what another page draws.
    """
    bound = {}

    def visit(owner, resources, owns):
        yield owner, resources, owns
        if not isinstance(resources, Dictionary):
            return
        xobjects, patterns = resources.get("/XObject"), resources.get("/Pattern")
        for instruction in instructions(owner):
            op, args = str(instruction.operator), instruction.operands
            if op == "Do" and args and isinstance(xobjects, Dictionary):
                target = xobjects.get(args[0])
                if isinstance(target, Stream) and target.get("/Subtype") == Name.Form:
                    yield from visit_stream(target, resources, xobjects, args[0])
            elif op in ("scn", "SCN") and args and isinstance(patterns, Dictionary):
                target = patterns.get(args[-1]) if isinstance(args[-1], Name) else None
                if isinstance(target, Stream):
                    yield from visit_stream(target, resources, patterns, args[-1])
            elif op in ("Tf", "gs") and args:
                font = selected_font(resources, op, args[0])
                if isinstance(font, Dictionary) and font.get("/Subtype") == Name.Type3:
                    procs = font.get("/CharProcs")
                    glyph_resources = font.get("/Resources")
                    if (
                        isinstance(glyph_resources, Dictionary)
                        and not glyph_resources.is_indirect
                    ):
                        font.Resources = pdf.make_indirect(glyph_resources)
                        glyph_resources = font.Resources
                    if not isinstance(glyph_resources, Dictionary):
                        glyph_resources = resources
                    for name, glyph in (
                        procs.items() if isinstance(procs, Dictionary) else ()
                    ):
                        if isinstance(glyph, Stream):
                            yield from visit_stream(glyph, glyph_resources, procs, name)

    def visit_stream(stream, resources, container, name):
        own = stream.get("/Resources")
        if isinstance(own, Dictionary) and not own.is_indirect:
            stream.Resources = pdf.make_indirect(own)
            own = stream.Resources
        owns = isinstance(own, Dictionary)
        context = own if owns else resources
        context_key = context.objgen if getattr(context, "is_indirect", False) else None
        previous = bound.get(stream.objgen, False)
        if previous is not False:
            if owns or previous == context_key:
                return
            clone = pdf.make_stream(stream.read_raw_bytes())
            for key, value in stream.items():
                if key != "/Length":
                    clone[key] = value
            container[name] = clone
            stream = clone
        bound[stream.objgen] = context_key
        yield from visit(stream, context, owns)

    for page in pdf.pages:
        resources = inherited(page.obj, "/Resources", Dictionary())
        if not getattr(resources, "is_indirect", False):
            resources = pdf.make_indirect(resources)
        page.obj.Resources = resources
        yield from visit(page, resources, True)
        for annot in page.obj.get("/Annots") or []:
            appearance = annot.get("/AP") if isinstance(annot, Dictionary) else None
            if not isinstance(appearance, Dictionary):
                continue
            for state_name, state in appearance.items():
                if isinstance(state, Stream):
                    yield from visit_stream(state, resources, appearance, state_name)
                elif isinstance(state, Dictionary):
                    for sub_name, stream in state.items():
                        if isinstance(stream, Stream):
                            yield from visit_stream(stream, resources, state, sub_name)


def selected_font(resources, op, name):
    """The font a Tf operand or a gs operand's ExtGState /Font entry selects."""
    if not isinstance(resources, Dictionary):
        return None
    if op == "Tf":
        font = (resources.get("/Font") or {}).get(name)
        return font if isinstance(font, Dictionary) else None
    state = (resources.get("/ExtGState") or {}).get(name)
    entry = state.get("/Font") if isinstance(state, Dictionary) else None
    if (
        isinstance(entry, Array)
        and len(entry) == 2
        and isinstance(entry[0], Dictionary)
    ):
        return entry[0]
    return None


def instructions(owner):
    try:
        return [
            i for i in pikepdf.parse_content_stream(owner) if hasattr(i, "operator")
        ]
    except (pikepdf.PdfError, ValueError, RuntimeError):
        return []


def font_usage(pdf):
    """Codes or CIDs drawn with each font outside rendering mode 3.

    A font's value of None means its usage could not be determined, so every
    code counts. The whole result is None when the document is too large to
    walk, which callers treat the same way for every font. Forms inherit the
    caller's font and rendering mode; patterns, glyphs, and appearances start
    with none.
    """
    used, visited, budget, truncated = {}, set(), [200000], []

    def record(font, text, render_mode):
        if font is None or not font.is_indirect or render_mode == 3:
            return
        key = font.objgen
        if font.get("/Subtype") == Name.Type0:
            encoding = font.get("/Encoding")
            if encoding not in (Name("/Identity-H"), Name("/Identity-V")):
                used[key] = None
                return
            raw = bytes(text)
            if len(raw) % 2:
                used[key] = None
                return
            codes = {
                int.from_bytes(raw[i : i + 2], "big") for i in range(0, len(raw), 2)
            }
        else:
            codes = set(bytes(text))
        current = used.get(key, set())
        if current is not None:
            current.update(codes)
            used[key] = current

    def walk(owner, resources, font, render_mode, depth):
        holder = owner.obj if isinstance(owner, pikepdf.Page) else owner
        state = (holder.objgen, font.objgen if font is not None else None, render_mode)
        if depth > 64:
            truncated.append(state)
            return
        if state in visited:
            return
        visited.add(state)
        xobjects = (
            resources.get("/XObject") if isinstance(resources, Dictionary) else None
        )
        patterns = (
            resources.get("/Pattern") if isinstance(resources, Dictionary) else None
        )
        stack = []
        for instruction in instructions(owner):
            budget[0] -= 1
            if budget[0] <= 0:
                return
            op, args = str(instruction.operator), instruction.operands
            if op == "q":
                stack.append((font, render_mode))
            elif op == "Q" and stack:
                font, render_mode = stack.pop()
            elif op in ("Tf", "gs") and args:
                chosen = selected_font(resources, op, args[0])
                if chosen is not None or op == "Tf":
                    font = chosen
                if (
                    isinstance(chosen, Dictionary)
                    and chosen.get("/Subtype") == Name.Type3
                ):
                    procs = chosen.get("/CharProcs")
                    for glyph in (
                        procs.values() if isinstance(procs, Dictionary) else ()
                    ):
                        if isinstance(glyph, Stream):
                            walk(
                                glyph,
                                chosen.get("/Resources", resources),
                                None,
                                0,
                                depth + 1,
                            )
            elif op == "Tr" and len(args) == 1:
                render_mode = int(args[0])
            elif op in ("Tj", "'", '"') and args:
                record(font, args[-1], render_mode)
            elif op == "TJ" and args:
                for value in args[0]:
                    if isinstance(value, String):
                        record(font, value, render_mode)
            elif op == "Do" and args and isinstance(xobjects, Dictionary):
                target = xobjects.get(args[0])
                if isinstance(target, Stream) and target.get("/Subtype") == Name.Form:
                    walk(
                        target,
                        target.get("/Resources", resources),
                        font,
                        render_mode,
                        depth + 1,
                    )
            elif op in ("scn", "SCN") and args and isinstance(patterns, Dictionary):
                target = patterns.get(args[-1]) if isinstance(args[-1], Name) else None
                if isinstance(target, Stream):
                    walk(
                        target, target.get("/Resources", resources), None, 0, depth + 1
                    )

    for page in pdf.pages:
        resources = inherited(page.obj, "/Resources", Dictionary())
        walk(page, resources, None, 0, 0)
        for annot in page.obj.get("/Annots") or []:
            appearance = annot.get("/AP") if isinstance(annot, Dictionary) else None
            if not isinstance(appearance, Dictionary):
                continue
            for state in appearance.values():
                streams = state.values() if isinstance(state, Dictionary) else [state]
                for stream in streams:
                    if isinstance(stream, Stream):
                        walk(stream, stream.get("/Resources", resources), None, 0, 1)
    return None if budget[0] <= 0 or truncated else used


def color_family(space, resources, depth=0):
    """Device family a color space resolves to, or None for device-independent."""
    if depth > 8:
        return None
    if isinstance(space, Name):
        name = str(space)
        if name in ("/DeviceGray", "/G"):
            return "Gray"
        if name in ("/DeviceRGB", "/RGB"):
            return "RGB"
        if name in ("/DeviceCMYK", "/CMYK"):
            return "CMYK"
        if name == "/Pattern":
            return None
        named = (resources.get("/ColorSpace") or {}).get(name) if resources else None
        return color_family(named, None, depth + 1) if named is not None else None
    if isinstance(space, Array) and len(space) > 0:
        kind = pdf_name(space[0])
        if kind in ("/Indexed", "/I") and len(space) > 1:
            return color_family(space[1], resources, depth + 1)
        if kind in ("/Separation", "/DeviceN") and len(space) > 2:
            return color_family(space[2], resources, depth + 1)
        if kind == "/Pattern" and len(space) > 1:
            return color_family(space[1], resources, depth + 1)
        if kind in ("/DeviceGray", "/DeviceRGB", "/DeviceCMYK"):
            return color_family(space[0], resources, depth + 1)
    return None


def device_color_families(pdf):
    families = set()
    operators = {
        "g": "Gray",
        "G": "Gray",
        "rg": "RGB",
        "RG": "RGB",
        "k": "CMYK",
        "K": "CMYK",
    }
    for owner, resources, _ in content_streams(pdf):
        defaults = set()
        for key, family in (
            ("/DefaultGray", "Gray"),
            ("/DefaultRGB", "RGB"),
            ("/DefaultCMYK", "CMYK"),
        ):
            if isinstance(resources, Dictionary) and key in (
                resources.get("/ColorSpace") or {}
            ):
                defaults.add(family)
        for instruction in instructions(owner):
            op, args = str(instruction.operator), instruction.operands
            family = operators.get(op)
            if family is None and op in ("cs", "CS") and args:
                family = color_family(args[0], resources)
            if family and family not in defaults:
                families.add(family)
        if isinstance(resources, Dictionary):
            for shading in (resources.get("/Shading") or {}).values():
                family = color_family(shading.get("/ColorSpace"), resources)
                if family and family not in defaults:
                    families.add(family)
    for obj in pdf.objects:
        image = isinstance(obj, Stream) and obj.get("/Subtype") == Name.Image
        if image or (isinstance(obj, Dictionary) and "/ShadingType" in obj):
            family = color_family(obj.get("/ColorSpace"), None)
            if family:
                families.add(family)
    return families


# --- forbidden features ----------------------------------------------------


def annotations(pdf):
    for page in pdf.pages:
        for annot in page.obj.get("/Annots") or []:
            if isinstance(annot, Dictionary):
                yield page, annot


def forbidden_action(action):
    """Describe why an action chain is not allowed, or return None.

    Every action reachable through /Next is checked once; cycles are skipped.
    """
    pending, seen = [action], set()
    while pending:
        current = pending.pop()
        if not isinstance(current, Dictionary):
            continue
        if current.is_indirect:
            if current.objgen in seen:
                continue
            seen.add(current.objgen)
        if len(seen) > 100000:
            return "action chain with more than 100000 links"
        kind = pdf_name(current.get("/S"))
        if kind not in ALLOWED_ACTIONS:
            return f"{kind or 'untyped'} action"
        if (
            kind == "/Named"
            and pdf_name(current.get("/N")) not in ALLOWED_NAMED_ACTIONS
        ):
            return f"named action {current.get('/N')}"
        chain = current.get("/Next")
        pending.extend(chain if isinstance(chain, Array) else [chain])
    return None


def outline_items(root):
    stack, seen = [(root.get("/Outlines") or {}).get("/First")], set()
    while stack:
        item = stack.pop()
        while (
            isinstance(item, Dictionary)
            and item.objgen not in seen
            and len(seen) < 100000
        ):
            seen.add(item.objgen)
            yield item
            if isinstance(item.get("/First"), Dictionary):
                stack.append(item.First)
            item = item.get("/Next")


def strip_features(pdf, strip, problems):
    """Remove forbidden features whose --strip category was requested.

    Other forbidden features are appended to problems as actionable messages.
    Pass problems=None to strip silently outside a PDF/A conversion.
    """
    strip = set(strip)
    root = pdf.Root

    def report(text):
        if problems is not None:
            problems.append(text)

    def action_holder(holder, where):
        reason = forbidden_action(holder.get("/A"))
        if reason:
            if "actions" in strip:
                del holder["/A"]
            else:
                report(f"{where} has a {reason}; {strip_hint('actions')}")

    def additional_actions(holder, where, allowed=None):
        aa = holder.get("/AA")
        if not isinstance(aa, Dictionary):
            return
        bad = [
            key
            for key, value in aa.items()
            if (allowed is not None and key not in allowed) or forbidden_action(value)
        ]
        if not bad:
            return
        if "actions" in strip:
            for key in bad:
                del aa[key]
            if len(aa) == 0:
                del holder["/AA"]
        else:
            report(
                f"{where} has additional actions {', '.join(bad)}; {strip_hint('actions')}"
            )

    if isinstance(root.get("/OpenAction"), Dictionary):
        reason = forbidden_action(root.OpenAction)
        if reason and "actions" in strip:
            del root["/OpenAction"]
        elif reason:
            report(f"the document open action is a {reason}; {strip_hint('actions')}")
    additional_actions(root, "the document catalog", ALLOWED_ADDITIONAL_ACTIONS)
    for item in outline_items(root):
        action_holder(item, f"outline item {objref(item)}")
    for page in pdf.pages:
        additional_actions(
            page.obj, f"page {objref(page.obj)}", ALLOWED_ADDITIONAL_ACTIONS
        )
        kept, changed = [], False
        for annot in page.obj.get("/Annots") or []:
            if not isinstance(annot, Dictionary):
                changed = True
                continue
            subtype = pdf_name(annot.get("/Subtype"))
            where = f"{subtype or 'untyped'} annotation {objref(annot)}"
            if annotation_category(subtype) in strip:
                changed = True
                continue
            if subtype not in ALLOWED_ANNOTATIONS:
                category = (
                    "multimedia" if subtype in FORBIDDEN_ANNOTATIONS else "annotations"
                )
                if category in strip:
                    changed = True
                    continue
                report(f"{where} is not allowed in PDF/A-4; {strip_hint(category)}")
            if subtype == "/FileAttachment" and "attachments" in strip:
                changed = True
                continue
            if int(annot.get("/F", 0)) & (FLAG_HIDDEN | FLAG_NOVIEW):
                if "hidden" in strip:
                    changed = True
                    continue
                report(f"{where} is hidden; {strip_hint('hidden')}")
            if subtype == "/Widget":
                if "/A" in annot:
                    if "actions" in strip:
                        del annot["/A"]
                    else:
                        report(f"{where} has an action; {strip_hint('actions')}")
                additional_actions(annot, where)
            else:
                action_holder(annot, where)
                additional_actions(annot, where, ALLOWED_ADDITIONAL_ACTIONS)
            kept.append(annot)
        if changed:
            if kept:
                page.obj.Annots = Array(kept)
            else:
                del page.obj["/Annots"]
    acroform = root.get("/AcroForm")
    if isinstance(acroform, Dictionary):
        if "/XFA" in acroform:
            if "xfa" in strip:
                del acroform["/XFA"]
            else:
                report(f"the form has XFA data; {strip_hint('xfa')}")
        for field in form_fields(acroform):
            action_holder(field, f"form field {objref(field)}")
            additional_actions(field, f"form field {objref(field)}")
    if "attachments" in strip:
        names = root.get("/Names")
        if isinstance(names, Dictionary) and "/EmbeddedFiles" in names:
            del names["/EmbeddedFiles"]
        for holder in list(associated_file_holders(pdf)):
            del holder["/AF"]


def annotation_category(subtype):
    """The --strip category that removes an annotation, matching the Go CLI."""
    return {"/Widget": "forms", "/Link": "links"}.get(subtype, "annotations")


def form_fields(acroform):
    seen, stack = set(), [field for field in acroform.get("/Fields") or []]
    while stack:
        field = stack.pop()
        if (
            not isinstance(field, Dictionary)
            or field.objgen in seen
            or len(seen) > 100000
        ):
            continue
        seen.add(field.objgen)
        yield field
        stack.extend(field.get("/Kids") or [])


# --- structure -------------------------------------------------------------


def repair_structure(pdf, notes):
    root = pdf.Root
    for key in ("/Requirements", "/Version", "/NeedsRendering"):
        if key in root:
            del root[key]
    names = root.get("/Names")
    if isinstance(names, Dictionary) and "/AlternatePresentations" in names:
        del names["/AlternatePresentations"]
    for page in pdf.pages:
        if "/PresSteps" in page.obj:
            del page.obj["/PresSteps"]
    perms = root.get("/Perms")
    if isinstance(perms, Dictionary):
        for key in [k for k in perms if k != "/DocMDP"]:
            del perms[key]
        if len(perms) == 0:
            del root["/Perms"]
    acroform = root.get("/AcroForm")
    if isinstance(acroform, Dictionary) and "/NeedAppearances" in acroform:
        del acroform["/NeedAppearances"]
    repair_optional_content(root)
    recompressed = 0
    for obj in pdf.objects:
        if isinstance(obj, Stream):
            for key in ("/F", "/FFilter", "/FDecodeParms"):
                if key in obj:
                    del obj[key]
            filters = obj.get("/Filter")
            filters = [filters] if isinstance(filters, Name) else list(filters or [])
            # Only pipelines pikepdf fully decodes can be rewritten; an image codec
            # after LZW would be lost.
            if Name.LZWDecode in filters and all(
                str(f) in GENERALIZED_FILTERS for f in filters
            ):
                try:
                    data = obj.read_bytes()
                except pikepdf.PdfError:
                    continue
                obj.write(zlib.compress(data, 9), filter=Name.FlateDecode)
                recompressed += 1
            subtype = obj.get("/Subtype")
            if subtype == Name.Image:
                for key in ("/Alternates", "/OPI"):
                    if key in obj:
                        del obj[key]
                if obj.get("/Interpolate", False):
                    obj.Interpolate = False
            elif subtype == Name.Form:
                for key in ("/OPI", "/Ref"):
                    if key in obj:
                        del obj[key]
    if recompressed:
        notes.append(f"re-encoded {recompressed} LZW streams with Flate")
    # Traverse completely before binding: a stream reached from a second context
    # must still look resource-less so that it gets cloned.
    for owner, resources, owns in list(content_streams(pdf)):
        holder = owner.obj if isinstance(owner, pikepdf.Page) else owner
        if not owns and isinstance(resources, Dictionary):
            holder.Resources = resources
        if not isinstance(resources, Dictionary):
            continue
        for state in (resources.get("/ExtGState") or {}).values():
            if isinstance(state, Dictionary):
                repair_graphics_state(state)
        # Font repairs address fonts by object number, so direct ones become indirect.
        fonts = resources.get("/Font")
        if isinstance(fonts, Dictionary):
            for name, font in list(fonts.items()):
                if isinstance(font, Dictionary) and not font.is_indirect:
                    fonts[name] = pdf.make_indirect(font)


def repair_graphics_state(state):
    for key in ("/TR", "/HTO"):
        if key in state:
            del state[key]
    if "/TR2" in state and state.get("/TR2") != Name.Default:
        del state["/TR2"]
    halftone = state.get("/HT")
    if isinstance(halftone, Dictionary):
        valid = (
            int(halftone.get("/HalftoneType", 0)) in (1, 5)
            and "/HalftoneName" not in halftone
        )
    else:
        valid = halftone is None or isinstance(halftone, Name)
    if not valid:
        del state["/HT"]


def repair_optional_content(root):
    properties = root.get("/OCProperties")
    if not isinstance(properties, Dictionary):
        return
    groups = [g for g in properties.get("/OCGs") or [] if isinstance(g, Dictionary)]
    configs = []
    if isinstance(properties.get("/D"), Dictionary):
        configs.append(properties.D)
    configs.extend(
        c for c in properties.get("/Configs") or [] if isinstance(c, Dictionary)
    )
    names = set()
    for index, config in enumerate(configs):
        name = str(config.get("/Name", ""))
        if not name or name in names:
            name = "Default" if index == 0 else f"Configuration {index}"
            while name in names:
                name += " (copy)"
            config.Name = String(name)
        names.add(name)
        if "/AS" in config:
            del config["/AS"]
        if "/Order" in config:
            listed = set()
            collect_references(config.Order, listed)
            missing = [g for g in groups if g.objgen not in listed]
            if missing:
                config.Order = Array(list(config.Order) + missing)


def collect_references(item, found, depth=0):
    if depth > 32:
        return
    if isinstance(item, Array):
        for value in item:
            collect_references(value, found, depth + 1)
    elif isinstance(item, Dictionary) and item.is_indirect:
        found.add(item.objgen)


# --- embedded files --------------------------------------------------------


def file_specifications(pdf):
    """File specification dictionaries from the name tree, AF arrays, and annotations."""
    specs, seen = [], set()

    def add(spec):
        if isinstance(spec, Dictionary) and "/EF" in spec and spec.objgen not in seen:
            seen.add(spec.objgen)
            specs.append(spec)

    names = pdf.Root.get("/Names")
    if isinstance(names, Dictionary) and "/EmbeddedFiles" in names:
        for spec in pikepdf.NameTree(names.EmbeddedFiles).values():
            add(spec)
    for holder in associated_file_holders(pdf):
        files = holder.AF
        for spec in files if isinstance(files, Array) else [files]:
            add(spec)
    for _, annot in annotations(pdf):
        if annot.get("/Subtype") == Name.FileAttachment:
            add(annot.get("/FS"))
    return specs


def associated_file_holders(pdf):
    """Objects carrying /AF: catalog, pages, XObjects, structure elements, annotations."""
    seen = set()
    for holder in [pdf.Root, *(page.obj for page in pdf.pages), *pdf.objects]:
        if (
            isinstance(holder, (Dictionary, Stream))
            and "/AF" in holder
            and holder.objgen not in seen
        ):
            seen.add(holder.objgen)
            yield holder


def repair_embedded_files(pdf, notes):
    """Normalize file specifications. Returns True when the file needs PDF/A-4f."""
    specs = file_specifications(pdf)
    if not specs:
        return False
    if not isinstance(pdf.Root.get("/Names"), Dictionary):
        pdf.Root.Names = Dictionary()
    names = pdf.Root.Names
    tree = (
        pikepdf.NameTree(names.EmbeddedFiles)
        if "/EmbeddedFiles" in names
        else pikepdf.NameTree.new(pdf)
    )
    names.EmbeddedFiles = tree.obj
    listed = {spec.objgen for _, spec in tree.items() if isinstance(spec, Dictionary)}
    for index, spec in enumerate(specs):
        filename = (
            spec.get("/UF") or spec.get("/F") or String(f"attachment-{index + 1}")
        )
        if not isinstance(filename, String):
            filename = String(f"attachment-{index + 1}")
        spec.F, spec.UF = filename, filename
        if "/AFRelationship" not in spec:
            spec.AFRelationship = Name.Unspecified
        for stream in spec.EF.values():
            if isinstance(stream, Stream) and "/Subtype" not in stream:
                mime = (
                    mimetypes.guess_type(str(filename))[0] or "application/octet-stream"
                )
                stream.Subtype = Name("/" + mime)
        if spec.objgen not in listed:
            key = str(filename)
            while key in tree:
                key += " (copy)"
            tree[key] = spec
    notes.append(f"{len(specs)} embedded files; targeting PDF/A-4f")
    return True


# --- annotations -----------------------------------------------------------


def missing_appearances(pdf):
    found = []
    for _, annot in annotations(pdf):
        subtype = pdf_name(annot.get("/Subtype"))
        rect = annot.get("/Rect")
        if subtype in APPEARANCE_EXEMPT:
            continue
        if isinstance(rect, Array) and len(rect) == 4:
            values = [float(v) for v in rect]
            if values[0] == values[2] and values[1] == values[3]:
                continue
        appearance = annot.get("/AP")
        if not isinstance(appearance, Dictionary) or "/N" not in appearance:
            found.append(annot)
    return found


def generate_appearances(source, staged):
    """Create missing appearance streams with MuPDF. Returns failures."""
    import pymupdf

    failures = []
    # MuPDF reports annotations it cannot draw on stderr; those are reported later.
    pymupdf.TOOLS.mupdf_display_errors(False)
    with pymupdf.open(source) as document:
        regenerate = bool(document.need_appearances())
        for page in document:
            for annot in page.annots():
                # Exempt types need no appearance; forbidden types are reported later.
                if annot.type[1] in (
                    "Popup",
                    "Link",
                    "Projection",
                    "Movie",
                    "Sound",
                    "Screen",
                    "3D",
                    "RichMedia",
                ):
                    continue
                if document.xref_get_key(annot.xref, "AP/N")[0] != "null":
                    continue
                try:
                    annot.update()
                except (RuntimeError, ValueError) as exc:
                    failures.append(
                        f"{annot.type[1]} annotation on page {page.number + 1}: {exc}"
                    )
            for widget in page.widgets():
                if (
                    regenerate
                    or document.xref_get_key(widget.xref, "AP/N")[0] == "null"
                ):
                    try:
                        widget.update()
                    except (RuntimeError, ValueError) as exc:
                        failures.append(
                            f"form field {widget.field_name!r} on page {page.number + 1}: {exc}"
                        )
        if regenerate:
            document.need_appearances(False)
        document.save(staged, garbage=1, deflate=True)
    return failures


def repair_annotations(pdf, problems):
    for _, annot in annotations(pdf):
        flags = int(annot.get("/F", 0))
        annot.F = (flags | FLAG_PRINT) & ~(FLAG_INVISIBLE | FLAG_TOGGLE)
        appearance = annot.get("/AP")
        if not isinstance(appearance, Dictionary):
            continue
        for key in ("/D", "/R"):
            if key in appearance:
                del appearance[key]
        normal = appearance.get("/N")
        button = (
            annot.get("/Subtype") == Name.Widget
            and annot.get("/FT", inherited(annot, "/FT")) == Name.Btn
        )
        if isinstance(normal, Dictionary) and not button:
            state = annot.get("/AS")
            chosen = normal.get(state) if state is not None else None
            if chosen is None and len(normal) == 1:
                chosen = next(iter(normal.values()))
            if isinstance(chosen, Stream):
                appearance.N = chosen
                if "/AS" in annot:
                    del annot["/AS"]
            else:
                problems.append(
                    f"annotation {objref(annot)} has appearance states without a usable /AS; use --strip annotations or --flatten annotations"
                )
    for annot in missing_appearances(pdf):
        category = annotation_category(pdf_name(annot.get("/Subtype")))
        problems.append(
            f"{annot.get('/Subtype')} annotation {objref(annot)} has no appearance stream and MuPDF cannot generate one; use --flatten {category} or --strip {category}"
        )


# --- color -----------------------------------------------------------------


def icc_header(data):
    if len(data) < 128:
        raise ConversionError("ICC profile is truncated")
    device_class, space = data[12:16], data[16:20]
    if device_class not in (b"mntr", b"prtr"):
        raise ConversionError(
            f"ICC profile class {device_class.decode('ascii', 'replace')!r} cannot be a PDF/A output intent; use a display (mntr) or output (prtr) profile"
        )
    components = {b"GRAY": 1, b"RGB ": 3, b"CMYK": 4}.get(space)
    if components is None:
        raise ConversionError("output intent profile must be Gray, RGB, or CMYK")
    return space, components


def ensure_output_intent(pdf, options, notes):
    families = device_color_families(pdf)
    intents = [
        i
        for i in pdf.Root.get("/OutputIntents") or []
        if isinstance(i, Dictionary) and i.get("/S") == Name.GTS_PDFA1
    ]
    user_profile = options.get("output_intent")
    existing = None
    if intents and not user_profile:
        existing = intents[0]
        profile = existing.get("/DestOutputProfile")
        if isinstance(profile, Stream) and "/DestOutputProfileRef" not in existing:
            try:
                space, components = icc_header(profile.read_bytes())
                existing = (existing, space, profile)
            except (ConversionError, pikepdf.PdfError):
                existing = None
        else:
            existing = None
    if existing is None:
        path = user_profile or options["srgb_icc"]
        with open(path, "rb") as f:
            data = f.read((64 << 20) + 1)
        if len(data) > 64 << 20:
            raise ConversionError("the output intent ICC profile exceeds 64 MiB")
        space, components = icc_header(data)
        profile = pdf.make_stream(data)
        profile.N = components
        identifier = "sRGB IEC61966-2.1" if not user_profile else "Custom"
        intent = Dictionary(
            Type=Name.OutputIntent,
            S=Name.GTS_PDFA1,
            OutputConditionIdentifier=String(identifier),
            Info=String(identifier),
            DestOutputProfile=profile,
        )
        pdf.Root.OutputIntents = Array([pdf.make_indirect(intent)])
        notes.append(f"added {space.decode('ascii').strip()} output intent")
    else:
        intent, space, profile = existing
        pdf.Root.OutputIntents = Array([intent])
    if "CMYK" in families and space != b"CMYK":
        raise ConversionError(
            "the document draws with DeviceCMYK, which needs a CMYK output intent; pass --output-intent with a CMYK ICC profile (printer or press profile)"
        )
    if "RGB" in families and space != b"RGB ":
        with open(options["srgb_icc"], "rb") as f:
            srgb = pdf.make_stream(f.read())
        srgb.N = 3
        default = pdf.make_indirect(Array([Name.ICCBased, srgb]))
        for owner, resources, _ in content_streams(pdf):
            if isinstance(resources, Dictionary):
                if not isinstance(resources.get("/ColorSpace"), Dictionary):
                    resources.ColorSpace = Dictionary()
                if "/DefaultRGB" not in resources.ColorSpace:
                    resources.ColorSpace.DefaultRGB = default
        notes.append("added DefaultRGB color space for DeviceRGB content")


# --- fonts -----------------------------------------------------------------


def font_name_key(base_font):
    name = re.sub(r"^[A-Z]{6}\+", "", str(base_font).lstrip("/"))
    lower = name.lower()
    bold = any(word in lower for word in ("bold", "black", "heavy", "semibold"))
    italic = "italic" in lower or "oblique" in lower
    family = re.split(r"[-,]", lower)[0].replace(" ", "").replace("_", "")
    family = re.sub(r"(ps)?(mt)?$", "", family)
    family = FAMILY_ALIASES.get(family, family)
    return name, family, bold, italic


def fontconfig_match(family, bold, italic):
    binary = shutil.which("fc-match")
    if not binary:
        raise ConversionError(
            "fontconfig's fc-match is required to locate substitute fonts"
        )
    pattern = f"{family}:weight={200 if bold else 80}:slant={100 if italic else 0}"
    result = subprocess.run(
        [binary, "-f", "%{family}|%{file}|%{fontformat}|%{index}", pattern],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    parts = result.stdout.strip().split("|")
    if result.returncode or len(parts) != 4:
        return None
    matched, path, fmt, index = parts
    if (
        family.lower() not in matched.lower()
        or fmt not in ("TrueType", "CFF")
        or index not in ("", "0")
    ):
        return None
    return path


def user_font_file(name, family, options):
    """A --font-file entry matching the full name, its family stem, or the family key."""
    plain = name.lower().replace(" ", "")
    stem = re.sub(r"(ps)?(mt)?$", "", re.split(r"[-,]", plain)[0])
    for key, path in (options.get("font_files") or {}).items():
        if key.lower().replace(" ", "") in (plain, stem, family.replace(" ", "")):
            return path
    return None


def substitute_path(base_font, options):
    name, family, bold, italic = font_name_key(base_font)
    path = user_font_file(name, family, options)
    if path:
        return path, True
    for candidate in METRIC_COMPATIBLE.get(family, []):
        path = fontconfig_match(candidate, bold, italic)
        if path:
            return path, False
    raise ConversionError(
        f"font {name} is not embedded and has no metric-compatible substitute installed; install fonts-urw-base35 or fonts-liberation, or pass --font-file '{name}=/path/to/font.ttf'"
    )


def standard_encodings():
    from fontTools import agl
    from fontTools.encodings.MacRoman import MacRoman
    from fontTools.encodings.StandardEncoding import StandardEncoding

    winansi = [".notdef"] * 256
    for code in range(32, 256):
        try:
            char = bytes([code]).decode("cp1252")
        except UnicodeDecodeError:
            char = "\u2022"
        if code == 0xA0:
            char = " "
        elif code == 0xAD:
            char = "-"
        elif code == 0x7F:
            continue
        winansi[code] = agl.UV2AGL.get(ord(char), f"uni{ord(char):04X}")
    return {
        "/WinAnsiEncoding": winansi,
        "/MacRomanEncoding": list(MacRoman),
        "/StandardEncoding": list(StandardEncoding),
    }


def simple_encoding(font, builtin):
    """Glyph name per code for a simple font, from its /Encoding over a builtin table."""
    tables = standard_encodings()
    names = list(builtin)
    encoding = font.get("/Encoding")
    if isinstance(encoding, Name) and str(encoding) in tables:
        names = list(tables[str(encoding)])
    elif isinstance(encoding, Dictionary):
        base = encoding.get("/BaseEncoding")
        if isinstance(base, Name) and str(base) in tables:
            names = list(tables[str(base)])
        code = 0
        for item in encoding.get("/Differences") or []:
            if isinstance(item, Name):
                if 0 <= code < 256:
                    names[code] = str(item)[1:]
                code += 1
            else:
                code = int(item)
    return [name if name and name != ".notdef" else None for name in names]


def glyph_for_name(tt, name):
    from fontTools import agl

    if name in tt.getGlyphOrder():
        return name
    unicode = agl.toUnicode(name)
    if len(unicode) == 1:
        return tt.getBestCmap().get(ord(unicode))
    return None


def subset_tag(data):
    return "".join(chr(65 + int(c, 16)) for c in hashlib.sha256(data).hexdigest()[:6])


def descriptor_for(pdf, tt, ps_name, symbolic, bold):
    head, hhea, post = tt["head"], tt["hhea"], tt["post"]
    os2 = tt.get("OS/2")
    scale = 1000 / head.unitsPerEm
    flags = 4 if symbolic else 32
    if post.isFixedPitch:
        flags |= 1
    if post.italicAngle:
        flags |= 64
    ascent = os2.sTypoAscender if os2 and os2.sTypoAscender else hhea.ascent
    descent = os2.sTypoDescender if os2 and os2.sTypoDescender else hhea.descent
    cap_height = getattr(os2, "sCapHeight", 0) if os2 else 0
    return pdf.make_indirect(
        Dictionary(
            Type=Name.FontDescriptor,
            FontName=Name("/" + ps_name),
            Flags=flags,
            FontBBox=Array(
                [round(v * scale) for v in (head.xMin, head.yMin, head.xMax, head.yMax)]
            ),
            ItalicAngle=post.italicAngle,
            Ascent=round(ascent * scale),
            Descent=round(descent * scale),
            CapHeight=round((cap_height or ascent * 0.7) * scale),
            StemV=120 if bold else 80,
            MissingWidth=0,
        )
    )


def load_font_file(path):
    from fontTools.ttLib import TTFont, TTLibError

    try:
        return TTFont(path, lazy=False)
    except (TTLibError, OSError, ValueError) as exc:
        raise ConversionError(f"cannot read font file {path}: {exc}") from exc


def subset_font(tt, glyph_names, keep_gids=False):
    from fontTools import subset

    options = subset.Options()
    options.glyph_names = True
    options.notdef_outline = True
    options.retain_gids = keep_gids
    options.drop_tables += ["GSUB", "GPOS", "GDEF", "kern", "DSIG", "FFTM"]
    options.layout_features = []
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(glyphs=glyph_names)
    subsetter.subset(tt)


def program_stream(pdf, tt):
    """Serialize a font program. Returns (stream, descriptor key, font subtype)."""
    if "CFF " in tt:
        data = tt["CFF "].compile(tt)
        stream = pdf.make_stream(data)
        stream.Subtype = Name.Type1C
        return stream, "/FontFile3", Name.Type1
    buffer = io.BytesIO()
    tt.save(buffer)
    data = buffer.getvalue()
    stream = pdf.make_stream(data)
    stream.Length1 = len(data)
    return stream, "/FontFile2", Name.TrueType


def embed_simple_font(pdf, font, codes, options, notes):
    from fontTools import agl

    path, user_supplied = substitute_path(font.get("/BaseFont", "/Unknown"), options)
    tt = load_font_file(path)
    name, family, bold, _ = font_name_key(font.get("/BaseFont", "/Unknown"))
    descriptor = font.get("/FontDescriptor")
    symbolic = family in ("symbol", "zapfdingbats") or (
        isinstance(descriptor, Dictionary)
        and int(descriptor.get("/Flags", 0)) & 4
        and not int(descriptor.get("/Flags", 0)) & 32
    )
    builtin = standard_encodings()["/StandardEncoding"]
    if family == "symbol":
        builtin = symbol_encoding()
    elif "CFF " in tt and isinstance(tt["CFF "].cff[0].Encoding, list):
        builtin = tt["CFF "].cff[0].Encoding
    names = simple_encoding(font, builtin)
    wanted = sorted(codes) if codes is not None else [c for c in range(256) if names[c]]
    mapping, missing = {}, []
    for code in wanted:
        glyph = glyph_for_name(tt, names[code]) if names[code] else None
        if glyph is None:
            missing.append(code)
        else:
            mapping[code] = glyph
    if missing and codes is not None:
        raise ConversionError(
            f"substitute font {path} lacks glyphs for {len(missing)} codes used by {name} (first: {missing[:5]}); pass --font-file '{name}=/path/to/the/original/font'"
        )
    scale = 1000 / tt["head"].unitsPerEm
    widths = {
        code: round(tt["hmtx"][glyph][0] * scale) for code, glyph in mapping.items()
    }
    first, last = int(font.get("/FirstChar", 0)), int(font.get("/LastChar", -1))
    existing = font.get("/Widths")
    if isinstance(existing, Array) and last >= first:
        document = {
            code: float(existing[code - first])
            for code in mapping
            if first <= code <= last and code - first < len(existing)
        }
        mismatched = {
            code: width
            for code, width in document.items()
            if abs(width - widths[code]) > 1
        }
        if mismatched and not user_supplied:
            raise ConversionError(
                f"the document's widths for {name} differ from the metric-compatible substitute at {len(mismatched)} codes (first: {sorted(mismatched)[:5]}); pass --font-file '{name}=/path/to/the/original/font'"
            )
        if mismatched:
            # The document's widths define the layout; bend the program to them.
            targets = {}
            for code, width in mismatched.items():
                glyph = mapping[code]
                if targets.get(glyph, width) != width:
                    raise ConversionError(
                        f"{path} draws glyph {glyph} for codes with different widths in {name}; the document was laid out for another font"
                    )
                targets[glyph] = width
            align_program_widths(tt, targets)
            widths.update(mismatched)
            notes.append(
                f"aligned {len(targets)} glyph widths of {path.rsplit('/', 1)[-1]} to {name}"
            )
    subset_font(tt, sorted(set(mapping.values()) | {".notdef"}))
    stream, key, subtype = program_stream(pdf, tt)
    ps_name = (
        subset_tag(stream.read_raw_bytes())
        + "+"
        + (tt["name"].getDebugName(6) or family).replace(" ", "")
    )
    if not isinstance(descriptor, Dictionary):
        descriptor = font.FontDescriptor = descriptor_for(
            pdf, tt, ps_name, symbolic and subtype == Name.Type1, bold
        )
    for old in ("/FontFile", "/FontFile2", "/FontFile3"):
        if old in descriptor:
            del descriptor[old]
    descriptor[key] = stream
    descriptor.FontName = Name("/" + ps_name)
    descriptor.MissingWidth = 0
    if subtype == Name.TrueType:
        descriptor.Flags = (int(descriptor.get("/Flags", 0)) | 32) & ~4
    font.BaseFont = Name("/" + ps_name)
    font.Subtype = subtype
    if subtype == Name.TrueType:
        base = standard_encodings()["/WinAnsiEncoding"]
        differences = {}
        for code, glyph in mapping.items():
            glyph_name = names[code]
            if glyph_name not in agl.AGL2UV and not re.fullmatch(
                r"uni[0-9A-F]{4}", glyph_name
            ):
                unicode = agl.toUnicode(glyph_name)
                glyph_name = (
                    agl.UV2AGL.get(ord(unicode), f"uni{ord(unicode):04X}")
                    if len(unicode) == 1
                    else None
                )
            if glyph_name and glyph_name != base[code]:
                differences[code] = glyph_name
        font.Encoding = Dictionary(
            Type=Name.Encoding,
            BaseEncoding=Name.WinAnsiEncoding,
            Differences=differences_array(differences),
        )
    else:
        font.Encoding = Dictionary(
            Type=Name.Encoding, Differences=differences_array(mapping)
        )
    span = range(min(mapping), max(mapping) + 1) if mapping else range(0)
    font.FirstChar, font.LastChar = span.start, span.stop - 1
    font.Widths = Array([widths.get(code, 0) for code in span])
    notes.append(f"embedded {path.rsplit('/', 1)[-1]} for {name}")


def align_program_widths(tt, targets):
    """Set advance widths (in 1/1000 em) of glyphs in a fontTools font."""
    from fontTools.pens.basePen import NullPen

    scale = tt["head"].unitsPerEm / 1000
    if "CFF " in tt:
        cff = tt["CFF "].cff
        top = cff[0]
        for glyph, width in targets.items():
            if glyph in top.CharStrings:
                charstring = top.CharStrings[glyph]
                charstring.draw(NullPen())
                top.CharStrings[glyph] = charstring_with_width(
                    charstring, round(width * scale), top.Private, cff.GlobalSubrs
                )
    for glyph, width in targets.items():
        if glyph in tt["hmtx"].metrics:
            tt["hmtx"][glyph] = (round(width * scale), tt["hmtx"][glyph][1])


def differences_array(mapping):
    items, previous = [], None
    for code in sorted(mapping):
        if previous is None or code != previous + 1:
            items.append(code)
        items.append(Name("/" + mapping[code]))
        previous = code
    return Array(items)


def cid_widths(descendant):
    widths, default = {}, int(descendant.get("/DW", 1000))
    items = list(descendant.get("/W") or [])
    index = 0
    while index < len(items):
        start = int(items[index])
        if index + 1 < len(items) and isinstance(items[index + 1], Array):
            for offset, width in enumerate(items[index + 1]):
                widths[start + offset] = float(width)
            index += 2
        elif index + 2 < len(items):
            for cid in range(start, int(items[index + 1]) + 1):
                widths[cid] = float(items[index + 2])
            index += 3
        else:
            break
    return widths, default


def embed_cid_font(pdf, font, cids, options, notes):
    name, family, _, _ = font_name_key(font.get("/BaseFont", "/Unknown"))
    descendants = font.get("/DescendantFonts") or []
    descendant = (
        descendants[0]
        if len(descendants) == 1 and isinstance(descendants[0], Dictionary)
        else None
    )
    path = user_font_file(name, family, options)
    if path is None:
        raise ConversionError(
            f"composite font {name} is not embedded; its glyph identifiers only match the original font file, so pass --font-file '{name}=/path/to/the/original/font.ttf'"
        )
    encoding = font.get("/Encoding")
    if (
        descendant is None
        or descendant.get("/Subtype") != Name.CIDFontType2
        or encoding not in (Name("/Identity-H"), Name("/Identity-V"))
    ):
        raise ConversionError(
            f"composite font {name} uses a CIDFont or CMap that cannot be embedded from a font file; re-export the document with embedded fonts"
        )
    mapping = descendant.get("/CIDToGIDMap", Name.Identity)
    if mapping != Name.Identity:
        raise ConversionError(
            f"composite font {name} has a CIDToGIDMap stream; only Identity mappings can be embedded"
        )
    tt = load_font_file(path)
    if "CFF " in tt:
        raise ConversionError(
            f"{path} has CFF outlines; a CIDFontType2 needs a TrueType font file"
        )
    count = len(tt.getGlyphOrder())
    if cids is None:
        cids = set(range(count))
    if cids and max(cids) >= count:
        raise ConversionError(
            f"{path} has {count} glyphs but {name} draws glyph {max(cids)}; this is not the font the document was made with"
        )
    scale = 1000 / tt["head"].unitsPerEm
    order = tt.getGlyphOrder()
    widths, default = cid_widths(descendant)
    for cid in sorted(cids):
        advance = round(tt["hmtx"][order[cid]][0] * scale)
        if abs(widths.get(cid, default) - advance) > 1:
            raise ConversionError(
                f"{path} glyph widths differ from the document's W array (CID {cid}: {widths.get(cid, default)} vs {advance}); this is not the font the document was made with"
            )
    subset_font(tt, [order[cid] for cid in sorted(cids)], keep_gids=True)
    buffer = io.BytesIO()
    tt.save(buffer)
    data = buffer.getvalue()
    stream = pdf.make_stream(data)
    stream.Length1 = len(data)
    descriptor = descendant.get("/FontDescriptor")
    ps_name = (
        subset_tag(data) + "+" + (tt["name"].getDebugName(6) or family).replace(" ", "")
    )
    if not isinstance(descriptor, Dictionary):
        descriptor = descendant.FontDescriptor = descriptor_for(
            pdf, tt, ps_name, True, False
        )
    for old in ("/FontFile", "/FontFile2", "/FontFile3"):
        if old in descriptor:
            del descriptor[old]
    descriptor.FontFile2 = stream
    descriptor.FontName = Name("/" + ps_name)
    descendant.BaseFont = font.BaseFont = Name("/" + ps_name)
    descendant.CIDToGIDMap = Name.Identity
    notes.append(f"embedded {path.rsplit('/', 1)[-1]} for composite font {name}")


def program_of(descriptor):
    if not isinstance(descriptor, Dictionary):
        return None, None
    for key in ("/FontFile2", "/FontFile3", "/FontFile"):
        stream = descriptor.get(key)
        if isinstance(stream, Stream):
            return key, stream
    return None, None


def repair_fonts(pdf, options, notes):
    usage = font_usage(pdf)
    for obj in list(pdf.objects):
        if not isinstance(obj, Dictionary) or "/BaseFont" not in obj:
            continue
        subtype = obj.get("/Subtype")
        if obj.get("/Type", Name.Font) != Name.Font or subtype in (
            Name.Type3,
            Name.CIDFontType0,
            Name.CIDFontType2,
        ):
            continue
        if usage is not None and obj.objgen not in usage:
            continue  # not drawn, or drawn only in rendering mode 3
        used = usage[obj.objgen] if usage is not None else None  # None: every code
        if subtype == Name.Type0:
            descendants = obj.get("/DescendantFonts") or []
            descendant = (
                descendants[0]
                if len(descendants) == 1 and isinstance(descendants[0], Dictionary)
                else None
            )
            key, program = program_of(
                descendant.get("/FontDescriptor") if descendant else None
            )
            if program is None:
                embed_cid_font(pdf, obj, used, options, notes)
            else:
                repair_embedded_cid_font(descendant, key, program, used, notes)
            continue
        key, program = program_of(obj.get("/FontDescriptor"))
        if program is None:
            embed_simple_font(pdf, obj, used, options, notes)
        else:
            repair_embedded_simple_font(pdf, obj, key, program, used, notes)


def repair_embedded_cid_font(descendant, key, program, cids, notes):
    if (
        descendant.get("/Subtype") == Name.CIDFontType2
        and "/CIDToGIDMap" not in descendant
    ):
        descendant.CIDToGIDMap = Name.Identity
    if key == "/FontFile3" and "/Subtype" not in program:
        program.Subtype = (
            Name.OpenType if program.read_bytes()[:4] == b"OTTO" else Name.CIDFontType0C
        )
    if cids == set() or key != "/FontFile2":
        return
    try:
        tt = load_font_file(io.BytesIO(program.read_bytes()))
    except ConversionError:
        return
    scale = 1000 / tt["head"].unitsPerEm
    order = tt.getGlyphOrder()
    if cids is None:
        cids = range(len(order))  # usage unknown: align every glyph
    widths, default = cid_widths(descendant)
    mapping = descendant.get("/CIDToGIDMap", Name.Identity)
    table = mapping.read_bytes() if isinstance(mapping, Stream) else None
    patched = 0
    for cid in cids:
        gid = cid
        if table is not None:
            if 2 * cid + 2 > len(table):
                continue
            gid = int.from_bytes(table[2 * cid : 2 * cid + 2], "big")
        if gid >= len(order):
            continue
        wanted = widths.get(cid, default)
        if abs(tt["hmtx"][order[gid]][0] * scale - wanted) > 1:
            tt["hmtx"][order[gid]] = (round(wanted / scale), tt["hmtx"][order[gid]][1])
            patched += 1
    if patched:
        buffer = io.BytesIO()
        tt.save(buffer)
        program.write(buffer.getvalue())
        program.Length1 = len(buffer.getvalue())
        notes.append(f"aligned {patched} glyph widths in {descendant.get('/BaseFont')}")


def repair_embedded_simple_font(pdf, font, key, program, codes, notes):
    descriptor = font.FontDescriptor
    data = program.read_bytes()
    if key == "/FontFile3" and "/Subtype" not in program:
        program.Subtype = Name.OpenType if data[:4] == b"OTTO" else Name.Type1C
    if key == "/FontFile2" and font.get("/Subtype") != Name.TrueType:
        font.Subtype = Name.TrueType
    if key == "/FontFile" and font.get("/Subtype") != Name.Type1:
        font.Subtype = Name.Type1
    if (
        key == "/FontFile3"
        and program.get("/Subtype") == Name.Type1C
        and font.get("/Subtype") != Name.Type1
    ):
        font.Subtype = Name.Type1
    flags = int(descriptor.get("/Flags", 0))
    symbolic = bool(flags & 4) and not flags & 32
    if font.get("/Subtype") == Name.TrueType and flags & 4 and flags & 32:
        # Both flag bits set: decide by the program's cmap, then keep one bit.
        symbolic = has_cmap(data, (3, 0))
        descriptor.Flags = (flags | (4 if symbolic else 32)) & ~(32 if symbolic else 4)
    if font.get("/Subtype") == Name.TrueType:
        if symbolic and "/Encoding" in font:
            del font["/Encoding"]
            notes.append(
                f"removed the encoding of symbolic font {font.get('/BaseFont')}"
            )
        if not symbolic:
            repair_truetype_encoding(font)
        repair_truetype_cmap(program, symbolic, notes)
    if codes is None or codes:
        repair_simple_widths(pdf, font, key, program, codes, notes)
    repair_to_unicode(font, notes)


def has_cmap(data, platform):
    try:
        tt = load_font_file(io.BytesIO(data))
    except ConversionError:
        return False
    cmap = tt.get("cmap")
    return cmap is not None and any(
        (t.platformID, t.platEncID) == platform for t in cmap.tables
    )


def repair_truetype_encoding(font):
    tables = standard_encodings()
    encoding = font.get("/Encoding")
    if isinstance(encoding, Name) and str(encoding) in (
        "/WinAnsiEncoding",
        "/MacRomanEncoding",
    ):
        return
    if isinstance(encoding, Dictionary) and pdf_name(encoding.get("/BaseEncoding")) in (
        "/WinAnsiEncoding",
        "/MacRomanEncoding",
    ):
        return
    differences = {}
    if isinstance(encoding, Dictionary):
        code = 0
        for item in encoding.get("/Differences") or []:
            if isinstance(item, Name):
                differences[code] = str(item)[1:]
                code += 1
            else:
                code = int(item)
    base_name = (
        str(encoding)
        if isinstance(encoding, Name) and str(encoding) in tables
        else "/StandardEncoding"
    )
    base, winansi = tables[base_name], tables["/WinAnsiEncoding"]
    for code in range(256):
        if (
            code not in differences
            and base[code] != ".notdef"
            and base[code] != winansi[code]
        ):
            differences[code] = base[code]
    font.Encoding = Dictionary(
        Type=Name.Encoding,
        BaseEncoding=Name.WinAnsiEncoding,
        Differences=differences_array(differences),
    )


def repair_truetype_cmap(program, symbolic, notes):
    from fontTools.ttLib.tables._c_m_a_p import CmapSubtable

    try:
        tt = load_font_file(io.BytesIO(program.read_bytes()))
    except ConversionError:
        return
    cmap = tt.get("cmap")
    if cmap is None:
        return
    tables = {(t.platformID, t.platEncID): t for t in cmap.tables}
    wanted = (3, 0) if symbolic else (3, 1)
    if wanted in tables or (symbolic and (1, 0) in tables):
        return
    source = (
        tables.get((1, 0))
        or tables.get((3, 1))
        or tables.get((0, 3))
        or tables.get((0, 4))
    )
    if source is None:
        return
    mapping = {}
    if symbolic:
        mapping = {code: name for code, name in source.cmap.items() if code < 256}
    elif (source.platformID, source.platEncID) == (1, 0):
        for code, name in source.cmap.items():
            try:
                mapping[ord(bytes([code]).decode("mac_roman"))] = name
            except (UnicodeDecodeError, ValueError):
                continue
    else:
        mapping = dict(source.cmap)
    if not mapping:
        return
    subtable = CmapSubtable.newSubtable(4)
    subtable.platformID, subtable.platEncID, subtable.language = wanted[0], wanted[1], 0
    subtable.cmap = {code: name for code, name in mapping.items() if code <= 0xFFFF}
    cmap.tables.append(subtable)
    buffer = io.BytesIO()
    tt.save(buffer)
    program.write(buffer.getvalue())
    program.Length1 = len(buffer.getvalue())
    notes.append(
        f"added a ({wanted[0]},{wanted[1]}) cmap subtable to {tt['name'].getDebugName(6)}"
    )


def truetype_glyph(tt, code, name, symbolic):
    """Glyph a PDF consumer selects for a code, following ISO 32000-2 9.6.6.4."""
    from fontTools import agl

    tables = (
        {(t.platformID, t.platEncID): t for t in tt["cmap"].tables}
        if "cmap" in tt
        else {}
    )
    order = tt.getGlyphOrder()
    if symbolic or name is None:
        for platform in ((3, 0), (1, 0)):
            table = tables.get(platform)
            if table is None:
                continue
            for candidate in (code, 0xF000 + code, 0xF100 + code, 0xF200 + code):
                if candidate in table.cmap:
                    return table.cmap[candidate]
        if name is None:
            return order[code] if code < len(order) else None
    unicode = agl.toUnicode(name)
    table = tables.get((3, 1))
    if table is not None and len(unicode) == 1 and ord(unicode) in table.cmap:
        return table.cmap[ord(unicode)]
    table = tables.get((1, 0))
    if table is not None:
        try:
            mac = unicode.encode("mac_roman")[0] if len(unicode) == 1 else None
        except UnicodeEncodeError:
            mac = None
        if mac is not None and mac in table.cmap:
            return table.cmap[mac]
    if name in order:
        return name
    match = re.fullmatch(r"(?:g|glyph|index)(\d+)", name)
    if match and int(match.group(1)) < len(order):
        return order[int(match.group(1))]
    return None


def repair_simple_widths(pdf, font, key, program, codes, notes):
    first = int(font.get("/FirstChar", 0))
    widths = font.get("/Widths")
    if not isinstance(widths, Array):
        return
    descriptor = font.FontDescriptor
    missing_width = float(descriptor.get("/MissingWidth", 0))

    def wanted(code):
        index = code - first
        return float(widths[index]) if 0 <= index < len(widths) else missing_width

    if codes is None:
        codes = set(range(first, first + len(widths)))
    data = program.read_bytes()
    flags = int(descriptor.get("/Flags", 0))
    symbolic = bool(flags & 4) and not flags & 32
    if key == "/FontFile2":
        try:
            tt = load_font_file(io.BytesIO(data))
        except ConversionError:
            return
        scale = 1000 / tt["head"].unitsPerEm
        names = simple_encoding(font, standard_encodings()["/StandardEncoding"])
        targets, patched = {}, 0
        for code in codes:
            glyph = truetype_glyph(
                tt, code, names[code] if code < 256 else None, symbolic
            )
            if glyph is None or glyph not in tt["hmtx"].metrics:
                continue
            if abs(tt["hmtx"][glyph][0] * scale - wanted(code)) > 1:
                targets.setdefault(glyph, set()).add(round(wanted(code) / scale))
        for glyph, values in targets.items():
            if len(values) == 1:
                tt["hmtx"][glyph] = (values.pop(), tt["hmtx"][glyph][1])
                patched += 1
        if patched:
            buffer = io.BytesIO()
            tt.save(buffer)
            program.write(buffer.getvalue())
            program.Length1 = len(buffer.getvalue())
            notes.append(f"aligned {patched} glyph widths in {font.get('/BaseFont')}")
        return
    if key == "/FontFile3" and program.get("/Subtype") == Name.Type1C:
        repair_cff_widths(font, program, data, codes, wanted, notes)
    elif key == "/FontFile":
        repair_type1_widths(pdf, font, descriptor, program, data, codes, wanted, notes)


def charstring_with_width(glyph, width, private, global_subrs):
    from fontTools.pens.t2CharStringPen import T2CharStringPen

    pen = T2CharStringPen(width, None)
    glyph.draw(pen)
    return pen.getCharString(private=private, globalSubrs=global_subrs)


def repair_cff_widths(font, program, data, codes, wanted, notes):
    from fontTools.cffLib import CFFFontSet

    cff = CFFFontSet()
    try:
        cff.decompile(io.BytesIO(data), None)
    except Exception:  # noqa: BLE001 -- unreadable programs are left for the validator
        return
    top = cff[0]
    if hasattr(top, "ROS"):
        return
    from fontTools.pens.basePen import NullPen

    glyph_set = top.CharStrings
    names = simple_encoding(
        font,
        top.Encoding
        if isinstance(top.Encoding, list)
        else standard_encodings()["/StandardEncoding"],
    )
    patched = 0
    for code in codes:
        name = names[code] if code < 256 else None
        if name is None or name not in glyph_set:
            continue
        charstring = glyph_set[name]
        charstring.draw(NullPen())
        if abs(charstring.width - wanted(code)) <= 1:
            continue
        glyph_set[name] = charstring_with_width(
            charstring, wanted(code), top.Private, cff.GlobalSubrs
        )
        patched += 1
    if patched:
        buffer = io.BytesIO()
        cff.compile(buffer, None)
        program.write(buffer.getvalue())
        notes.append(f"aligned {patched} glyph widths in {font.get('/BaseFont')}")


def repair_type1_widths(pdf, font, descriptor, program, data, codes, wanted, notes):
    """Rebuild a Type 1 program as CFF when its widths disagree with /Widths."""
    import os
    import tempfile

    from fontTools import t1Lib
    from fontTools.fontBuilder import FontBuilder
    from fontTools.pens.basePen import NullPen
    from fontTools.pens.t2CharStringPen import T2CharStringPen

    with tempfile.NamedTemporaryFile(suffix=".pfa", delete=False) as handle:
        handle.write(data)
        path = handle.name
    try:
        t1 = t1Lib.T1Font(path)
        glyph_set = t1.getGlyphSet()
        for glyph in glyph_set.values():
            glyph.draw(NullPen())
    except Exception:  # noqa: BLE001 -- unreadable programs are left for the validator
        return
    finally:
        os.remove(path)
    encoding = t1.font.get("Encoding")
    builtin = (
        encoding
        if isinstance(encoding, list) and len(encoding) == 256
        else standard_encodings()["/StandardEncoding"]
    )
    names = simple_encoding(font, builtin)
    mismatched = {}
    for code in codes:
        name = names[code] if code < 256 else None
        if name in glyph_set and abs(glyph_set[name].width - wanted(code)) > 1:
            mismatched[name] = wanted(code)
    if not mismatched:
        return
    charstrings = {}
    for name in glyph_set:
        pen = T2CharStringPen(mismatched.get(name, glyph_set[name].width), None)
        glyph_set[name].draw(pen)
        charstrings[name] = pen.getCharString()
    if ".notdef" not in charstrings:
        charstrings[".notdef"] = T2CharStringPen(0, None).getCharString()
    info = t1.font.get("FontInfo", {})
    builder = FontBuilder(1000, isTTF=False)
    builder.setupGlyphOrder(
        [".notdef"] + sorted(n for n in charstrings if n != ".notdef")
    )
    builder.setupCFF(
        str(font.get("/BaseFont", "/Font"))[1:].split("+")[-1],
        {
            "FullName": info.get("FullName", ""),
            "FontMatrix": t1.font.get("FontMatrix", [0.001, 0, 0, 0.001, 0, 0]),
        },
        charstrings,
        {},
    )
    data = builder.font["CFF "].compile(builder.font)
    stream = pdf.make_stream(data)
    stream.Subtype = Name.Type1C
    del descriptor["/FontFile"]
    descriptor.FontFile3 = stream
    font.Encoding = Dictionary(
        Type=Name.Encoding,
        Differences=differences_array(
            {c: names[c] for c in codes if c < 256 and names[c] in charstrings}
        ),
    )
    notes.append(
        f"rebuilt Type 1 program of {font.get('/BaseFont')} as CFF to align {len(mismatched)} widths"
    )


INVALID_UNICODE = {"0000", "FEFF", "FFFE"}


HEX = r"<[0-9A-Fa-f]+>"
BFCHAR_ENTRY = re.compile(rf"({HEX})\s*({HEX})\s*")
BFRANGE_ENTRY = re.compile(rf"({HEX})\s*({HEX})\s*({HEX}|\[[^\]]*\])\s*")


def clean_to_unicode(text):
    """Drop bfchar entries and trim bfranges that map to U+0000, U+FEFF, or U+FFFE.

    Only the mapping sections change and their entry counts are recomputed;
    codespace ranges and other CMap syntax stay.
    """

    def invalid(token):
        return token.strip("<>").upper() in INVALID_UNICODE

    def bfchar_entry(match):
        return "" if invalid(match.group(2)) else match.group(0)

    def bfrange_entry(match):
        low, high, target = match.groups()
        low_value, high_value = int(low.strip("<>"), 16), int(high.strip("<>"), 16)
        width = len(low) - 2
        if target.startswith("["):
            # One destination per code: keep the valid ones as single-code ranges.
            elements = re.findall(HEX, target)
            if not any(invalid(element) for element in elements):
                return match.group(0)
            kept = []
            for offset, element in enumerate(elements):
                code = low_value + offset
                if code <= high_value and not invalid(element):
                    kept.append(f"<{code:0{width}X}> <{code:0{width}X}> {element}\n")
            return "".join(kept)
        if not invalid(target):
            return match.group(0)
        if low_value >= high_value:
            return ""
        target_value = int(target.strip("<>"), 16)
        return f"<{low_value + 1:0{width}X}> {high} <{target_value + 1:0{len(target) - 2}X}>\n"

    def section(text, kind, entry, fix):
        def rewrite(match):
            # Comments carry no mappings; drop them so they are neither counted nor parsed.
            body = entry.sub(fix, re.sub(r"%[^\r\n]*", "", match.group(3)))
            count = len(entry.findall(body))
            if count == 0:
                return ""
            return f"{count}{match.group(2)}{body}{match.group(4)}"

        pattern = re.compile(rf"(\d+)(\s+begin{kind}\s*)(.*?)(end{kind})", re.DOTALL)
        return pattern.sub(rewrite, text)

    text = section(text, "bfchar", BFCHAR_ENTRY, bfchar_entry)
    return section(text, "bfrange", BFRANGE_ENTRY, bfrange_entry)


def repair_to_unicode(font, notes):
    stream = font.get("/ToUnicode")
    if not isinstance(stream, Stream):
        return
    try:
        text = stream.read_bytes().decode("latin-1")
    except pikepdf.PdfError:
        return
    fixed = clean_to_unicode(text)
    if fixed != text:
        stream.write(fixed.encode("latin-1"))
        notes.append(f"removed invalid Unicode mappings from {font.get('/BaseFont')}")


# --- metadata --------------------------------------------------------------


def write_metadata(pdf, attachments):
    """Identify the file as PDF/A-4 (or 4f) in XMP and drop the Info dictionary."""
    info = pdf.trailer.get("/Info")
    try:
        identify(pdf, info, attachments)
    except Exception:  # noqa: BLE001 -- unreadable XMP is replaced by a fresh packet
        if "/Metadata" in pdf.Root:
            del pdf.Root["/Metadata"]
        identify(pdf, info, attachments)
    if "/Info" in pdf.trailer:
        del pdf.trailer["/Info"]


def identify(pdf, info, attachments):
    with pdf.open_metadata(
        set_pikepdf_as_editor=False, update_docinfo=False, strict=False
    ) as meta:
        if isinstance(info, Dictionary):
            meta.load_from_docinfo(info, raise_failure=False)
        meta["pdfaid:part"] = "4"
        meta["pdfaid:rev"] = "2020"
        if attachments:
            meta["pdfaid:conformance"] = "F"
        elif "pdfaid:conformance" in meta:
            del meta["pdfaid:conformance"]
        for key in ("pdfaid:amd", "pdfaid:corr"):
            if key in meta:
                del meta[key]


# --- entry point -----------------------------------------------------------


def convert(source, output, options, message):
    import os

    work = os.path.dirname(output)
    strip = [s for s in options.get("strip", "").split(",") if s]
    problems, notes = [], []
    staged = source
    with pikepdf.open(source) as probe:
        # MuPDF only sees indirect annotation dictionaries.
        for page in probe.pages:
            annots = page.obj.get("/Annots")
            if isinstance(annots, Array) and any(
                isinstance(a, Dictionary) and not a.is_indirect for a in annots
            ):
                page.obj.Annots = Array(
                    [
                        probe.make_indirect(a) if isinstance(a, Dictionary) else a
                        for a in annots
                    ]
                )
        needs_appearances = bool(missing_appearances(probe)) or bool(
            isinstance(probe.Root.get("/AcroForm"), Dictionary)
            and probe.Root.AcroForm.get("/NeedAppearances", False)
        )
        if needs_appearances:
            staged = os.path.join(work, "indirect.pdf")
            probe.save(staged)
    if needs_appearances:
        source = staged
        staged = os.path.join(work, "appearances.pdf")
        failures = generate_appearances(source, staged)
        if failures:
            problems.extend(
                f"{failure}; use --flatten annotations or --strip annotations"
                for failure in failures
            )
    with pikepdf.open(staged) as pdf:
        strip_features(pdf, strip, problems)
        repair_structure(pdf, notes)
        attachments = (
            repair_embedded_files(pdf, notes) if "attachments" not in strip else False
        )
        repair_fonts(pdf, options, notes)
        ensure_output_intent(pdf, options, notes)
        repair_annotations(pdf, problems)
        if problems:
            raise ConversionError("cannot produce PDF/A-4:\n  " + "\n  ".join(problems))
        write_metadata(pdf, attachments)
        pdf.save(
            output,
            min_version="2.0",
            object_stream_mode=pikepdf.ObjectStreamMode.generate,
        )
    if options.get("verbose"):
        for note in notes:
            message("PDF/A-4: " + note)
    return "4f" if attachments else "4"
