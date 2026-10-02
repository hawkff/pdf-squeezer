#!/usr/bin/env python3
"""Optional local PDF operations. Invoked by the Go CLI in a private directory."""

import array
import copy
import hashlib
import io
import json
import math
import re
import shutil
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

MAX_SAMPLES = 256 << 20
FULL_RESOLUTION_SIZE_RATIO = 3.5
IDENTITY = (1, 0, 0, 1, 0, 0)


def message(text):
    print(text, file=sys.stderr, flush=True)


def multiply(a, b):
    return (
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    )


def inverse(m):
    d = m[0] * m[3] - m[1] * m[2]
    if not math.isfinite(d) or abs(d) < 1e-12:
        raise ValueError("singular image transform")
    return (
        m[3] / d,
        -m[1] / d,
        -m[2] / d,
        m[0] / d,
        (m[2] * m[5] - m[3] * m[4]) / d,
        (m[1] * m[4] - m[0] * m[5]) / d,
    )


def bounds(m, rect):
    points = [
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
        for x in (rect[0], rect[2])
        for y in (rect[1], rect[3])
    ]
    if not all(math.isfinite(v) and abs(v) < 1e12 for p in points for v in p):
        raise ValueError("invalid content coordinates")
    return (
        min(p[0] for p in points),
        min(p[1] for p in points),
        max(p[0] for p in points),
        max(p[1] for p in points),
    )


def intersect(a, b):
    return (max(a[0], b[0]), max(a[1], b[1]), min(a[2], b[2]), min(a[3], b[3]))


def union(a, b):
    if a is None:
        return b
    return (min(a[0], b[0]), min(a[1], b[1]), max(a[2], b[2]), max(a[3], b[3]))


def visible(r):
    return r[2] > r[0] and r[3] > r[1]


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


def collect_images(obj, found, seen=None, depth=0):
    import pikepdf

    if depth > 100:
        raise ValueError("resource nesting exceeds depth limit")
    if seen is None:
        seen = set()
    if isinstance(obj, (pikepdf.Dictionary, pikepdf.Stream)):
        if obj.is_indirect:
            if obj.objgen in seen:
                return
            seen.add(obj.objgen)
        if obj.get("/Subtype") == pikepdf.Name.Image:
            found.add(obj.objgen)
        for key, value in obj.items():
            if str(key) not in ("/Parent", "/P", "/Pages", "/Root"):
                collect_images(value, found, seen, depth + 1)
    elif isinstance(obj, pikepdf.Array):
        for value in obj:
            collect_images(value, found, seen, depth + 1)


def placements(pdf, lossless=False):
    """Conservative clipping bounds, unioned over every page/form use of an image."""
    import pikepdf

    uses, streams, blocked = {}, {}, set()
    visits = 0

    def walk(owner, resources, matrix, clip, active, depth=0):
        nonlocal visits
        visits += 1
        if depth > 64 or visits > 100000:
            raise ValueError("content traversal exceeds safety limit")
        obj = owner.obj if isinstance(owner, pikepdf.Page) else owner
        key = obj.objgen
        if key in active:
            raise ValueError("cyclic Form XObject")
        active = active | {key}
        instructions = pikepdf.parse_content_stream(owner)
        streams[key] = (owner, resources, instructions)
        stack, path, pending_clip = [], None, False
        for instruction in instructions:
            # Inline image objects are preserved by unparse_content_stream.
            if not hasattr(instruction, "operator"):
                continue
            op, args = str(instruction.operator), instruction.operands
            if op == "q":
                if len(stack) >= 256:
                    raise ValueError("graphics state nesting exceeds safety limit")
                stack.append((matrix, clip))
            elif op == "Q":
                if not stack:
                    raise ValueError("unbalanced graphics state")
                matrix, clip = stack.pop()
            elif op == "cm":
                if len(args) != 6:
                    raise ValueError("invalid transformation matrix")
                matrix = multiply(matrix, tuple(map(float, args)))
            elif op == "re":
                x, y, w, h = map(float, args)
                path = union(path, bounds(matrix, (x, y, x + w, y + h)))
            elif op in ("m", "l", "c", "v", "y"):
                values = list(map(float, args))
                if len(values) % 2:
                    raise ValueError("invalid path operands")
                for i in range(0, len(values), 2):
                    x, y = values[i : i + 2]
                    path = union(path, bounds(matrix, (x, y, x, y)))
            elif op in ("W", "W*"):
                pending_clip = True
            elif op in ("n", "S", "s", "f", "F", "f*", "B", "B*", "b", "b*"):
                if pending_clip:
                    clip = intersect(clip, path) if path is not None else (0, 0, 0, 0)
                path, pending_clip = None, False
            elif op == "Do":
                if len(args) != 1:
                    raise ValueError("invalid XObject invocation")
                target = resources.get("/XObject", {}).get(args[0])
                if target is None:
                    raise ValueError("missing XObject resource")
                subtype = target.get("/Subtype")
                if subtype == pikepdf.Name.Form:
                    fm = tuple(map(float, target.get("/Matrix", IDENTITY)))
                    combined = multiply(matrix, fm)
                    box = tuple(map(float, target.get("/BBox", (0, 0, 0, 0))))
                    walk(
                        target,
                        target.get("/Resources", resources),
                        combined,
                        intersect(clip, bounds(combined, box)),
                        active,
                        depth + 1,
                    )
                elif subtype == pikepdf.Name.Image:
                    nr = target.objgen
                    w, h = int(target.Width), int(target.Height)
                    if w <= 0 or h <= 0:
                        blocked.add(nr)
                        continue
                    try:
                        uv = intersect(bounds(inverse(matrix), clip), (0, 0, 1, 1))
                    except ValueError:
                        blocked.add(nr)
                        continue
                    if not visible(uv):
                        continue
                    # ponytail: only crop axis-aligned placements. General affine
                    # cropping can change a viewer's interpolation sample grid.
                    axis_aligned = (
                        abs(matrix[1]) < 1e-12 and abs(matrix[2]) < 1e-12
                    ) or (abs(matrix[0]) < 1e-12 and abs(matrix[3]) < 1e-12)
                    pixel_scales = (
                        math.hypot(matrix[0], matrix[1]) / w,
                        math.hypot(matrix[2], matrix[3]) / h,
                    )
                    integer_grid = all(
                        value >= 1 and abs(value - round(value)) < 1e-12
                        for value in pixel_scales
                    ) and all(
                        abs(value - round(value)) < 1e-12 for value in matrix[4:6]
                    )
                    # Lossless cropping also avoids changing the origin of a
                    # viewer's downsampling grid for fractional placements.
                    if not axis_aligned or lossless and not integer_grid:
                        uv = (0, 0, 1, 1)
                    # The largest singular value accounts for rotation and shear.
                    a, b, c, d = (
                        matrix[0] / w,
                        matrix[1] / w,
                        matrix[2] / h,
                        matrix[3] / h,
                    )
                    norm = a * a + b * b + c * c + d * d
                    sigma = math.sqrt(
                        (
                            norm
                            + math.sqrt(max(0, norm * norm - 4 * (a * d - b * c) ** 2))
                        )
                        / 2
                    )
                    if sigma <= 0 or not math.isfinite(sigma):
                        blocked.add(nr)
                        continue
                    previous = uses.get(nr)
                    uses[nr] = (
                        (union(previous[0], uv), min(previous[1], 72 / sigma))
                        if previous
                        else (uv, 72 / sigma)
                    )
        if stack:
            raise ValueError("unbalanced graphics state")

    for page in pdf.pages:
        unit = float(page.obj.get("/UserUnit", 1))
        if not math.isfinite(unit) or unit <= 0:
            raise ValueError("invalid page UserUnit")
        matrix = (unit, 0, 0, unit, 0, 0)
        box = inherited(page.obj, "/CropBox", inherited(page.obj, "/MediaBox"))
        resources = inherited(page.obj, "/Resources", pikepdf.Dictionary())
        walk(page, resources, matrix, bounds(matrix, tuple(map(float, box))), set())
        collect_images(page.obj.get("/Annots"), blocked)
    # Appearances, patterns, masks, and forms not reached above may reuse a page's
    # image at another scale. Preserve those images rather than guessing a use.
    for obj in pdf.objects:
        if not isinstance(obj, (pikepdf.Dictionary, pikepdf.Stream)):
            continue
        for key in ("/SMask", "/Mask", "/Pattern", "/Alternates"):
            collect_images(obj.get(key), blocked)
        if obj.get("/Subtype") == pikepdf.Name.Form and obj.objgen not in streams:
            collect_images(obj, blocked)
    collect_images(pdf.Root.get("/AcroForm"), blocked)
    for key in blocked:
        uses.pop(key, None)
    return uses, streams


def place_crops(pdf, streams, crops):
    import pikepdf

    if not crops:
        return
    for owner, resources, instructions in streams.values():
        rewritten = []
        changed = False
        for instruction in instructions:
            crop = None
            if hasattr(instruction, "operator") and str(instruction.operator) == "Do":
                target = resources.get("/XObject", {}).get(instruction.operands[0])
                if target is not None:
                    crop = crops.get(target.objgen)
            if crop is None:
                rewritten.append(instruction)
                continue
            left, top, right, bottom, width, height = crop
            transform = [
                (right - left) / width,
                0,
                0,
                (bottom - top) / height,
                left / width,
                1 - bottom / height,
            ]
            rewritten.extend(
                [
                    ([], pikepdf.Operator("q")),
                    (transform, pikepdf.Operator("cm")),
                    instruction,
                    ([], pikepdf.Operator("Q")),
                ]
            )
            changed = True
        if changed:
            data = pikepdf.unparse_content_stream(rewritten)
            if isinstance(owner, pikepdf.Page):
                owner.obj.Contents = pdf.make_stream(data)
            else:
                owner.write(data)


def ccitt(image):
    from pikepdf import Dictionary, Name
    from PIL import Image

    buffer = io.BytesIO()
    image.save(
        buffer, format="TIFF", compression="group4", tiffinfo={278: image.height}
    )
    payload = buffer.getvalue()
    with Image.open(io.BytesIO(payload)) as tiff:
        offsets, lengths = tiff.tag_v2[273], tiff.tag_v2[279]
        if len(offsets) != 1:
            raise ValueError(
                "TIFF encoder emitted multiple strips instead of one CCITT stream"
            )
        data = payload[offsets[0] : offsets[0] + lengths[0]]
        # libtiff's Group 4 run polarity follows the TIFF photometric tag.
        black_is_one = tiff.tag_v2.get(262, 1) == 1
    return (
        data,
        Name.CCITTFaxDecode,
        Dictionary(K=-1, Columns=image.width, Rows=image.height, BlackIs1=black_is_one),
    )


def jbig2(image):
    from pikepdf import Name

    binary = shutil.which("jbig2")
    if not binary:
        raise RuntimeError(
            "lossless JBIG2 requires jbig2enc's jbig2 executable on PATH"
        )
    with tempfile.TemporaryDirectory(prefix="jbig2-", dir=WORK) as directory:
        source, target = Path(directory) / "image.pbm", Path(directory) / "image.jb2"
        image.save(source, format="PPM")
        with target.open("wb") as out:
            # Generic region encoding is lossless. Never use symbol substitution -s.
            result = subprocess.run(
                [binary, "-p", str(source)],
                stdout=out,
                stderr=subprocess.PIPE,
                timeout=120,
                check=False,
            )
        if result.returncode:
            raise RuntimeError(
                "JBIG2 encoder failed: " + result.stderr[:4096].decode(errors="replace")
            )
        if not target.stat().st_size or target.stat().st_size > MAX_SAMPLES:
            raise ValueError("invalid JBIG2 encoder output size")
        return target.read_bytes(), Name.JBIG2Decode, None


def mono_candidate(image, codecs):
    from pikepdf import Name

    candidates = []
    for codec in codecs:
        if codec == "flate":
            candidates.append((zlib.compress(image.tobytes()), Name.FlateDecode, None))
        elif codec == "ccitt":
            candidates.append(ccitt(image))
        elif codec == "jbig2":
            candidates.append(jbig2(image))
    return min(candidates, key=lambda candidate: len(candidate[0]))


def crop16(data, width, height, components, box):
    left, top, right, bottom = box
    stride = width * components * 2
    return b"".join(
        data[y * stride + left * components * 2 : y * stride + right * components * 2]
        for y in range(top, bottom)
    )


def resize16(data, width, height, components, size):
    from PIL import Image

    samples = array.array("H")
    samples.frombytes(data)
    if sys.byteorder == "little":
        samples.byteswap()
    channels = []
    for component in range(components):
        channel = Image.new("I", (width, height))
        channel.putdata(samples[component::components])
        channel = channel.resize(size, Image.Resampling.BOX)
        channels.append(
            array.array(
                "H",
                (max(0, min(65535, value)) for value in channel.get_flattened_data()),
            )
        )
    result = array.array("H", [0]) * (size[0] * size[1] * components)
    for component, channel in enumerate(channels):
        result[component::components] = channel
    if sys.byteorder == "little":
        result.byteswap()
    return result.tobytes()


def process_images(pdf, options):
    import pikepdf
    from PIL import Image, features

    codecs = options["mono_codecs"].split(",")
    if "jbig2" in codecs and not shutil.which("jbig2"):
        raise RuntimeError("--mono-codecs jbig2 requires jbig2enc's jbig2 executable")
    if "ccitt" in codecs and not features.check("libtiff"):
        raise RuntimeError("CCITT requires a Pillow build with libtiff")
    geometry = options["geometry"] and (
        options["clip"] or options["dpi"] or options["gray_dpi"] or options["mono_dpi"]
    )
    uses, streams = placements(pdf, options["lossless"]) if geometry else ({}, {})
    crops, changed, preserved = {}, 0, {}
    protected = set()
    for obj in pdf.objects:
        if (
            isinstance(obj, pikepdf.Stream)
            and obj.get("/Subtype") == pikepdf.Name.Image
        ):
            for key in ("/SMask", "/Mask"):
                mask = obj.get(key)
                if isinstance(mask, pikepdf.Stream):
                    protected.add(mask.objgen)
    for obj in list(pdf.objects):
        if (
            not isinstance(obj, pikepdf.Stream)
            or obj.get("/Subtype") != pikepdf.Name.Image
        ):
            continue
        reason = None
        if (
            obj.objgen in protected
            or obj.get("/ImageMask", False)
            or "/Mask" in obj
            or "/SMask" in obj
        ):
            reason = "mask or transparency"
        w, h, bits = (
            int(obj.get("/Width", 0)),
            int(obj.get("/Height", 0)),
            int(obj.get("/BitsPerComponent", 0)),
        )
        cs = obj.get("/ColorSpace")
        components = {"/DeviceGray": 1, "/DeviceRGB": 3, "/DeviceCMYK": 4}.get(
            str(cs), 0
        )
        if (
            isinstance(cs, pikepdf.Array)
            and len(cs) == 2
            and cs[0] == pikepdf.Name.ICCBased
        ):
            components = int(cs[1].get("/N", 0))
        if components not in (1, 3, 4) or bits not in (1, 8, 16):
            reason = "unsupported color space or bit depth"
        if (
            w <= 0
            or h <= 0
            or w * h * max(1, components) * max(1, bits // 8) > MAX_SAMPLES
            or w * h * max(1, components) * max(1, bits // 8) * 8
            > options["memory_mib"] * (1 << 20)
        ):
            reason = "memory budget"
        filters = obj.get("/Filter", pikepdf.Array())
        if not isinstance(filters, pikepdf.Array):
            filters = [filters]
        encoded = any(str(f) in ("/DCTDecode", "/JPXDecode") for f in filters)
        if options["lossless"] and encoded:
            reason = "lossy original"
        if reason:
            preserved[reason] = preserved.get(reason, 0) + 1
            continue
        mono = bits == 1 and components == 1
        placement = uses.get(obj.objgen)
        if (
            not placement
            and not (mono and codecs != ["flate"])
            and not options["extended"]
        ):
            continue
        decode = obj.get("/Decode")
        inversions = [False] * components
        if decode is not None:
            values = list(map(float, decode))
            if len(values) != 2 * components or any(
                values[2 * i : 2 * i + 2] not in ([0, 1], [1, 0])
                for i in range(components)
            ):
                preserved["non-standard Decode"] = (
                    preserved.get("non-standard Decode", 0) + 1
                )
                continue
            inversions = [values[2 * i] == 1 for i in range(components)]
        original = obj.read_raw_bytes()
        box = (0, 0, w, h)
        if placement and options["clip"]:
            uv, _ = placement
            # Retain a one-pixel border for interpolation at the clipping edge.
            box = (
                max(0, math.floor(uv[0] * w) - 1),
                max(0, math.floor((1 - uv[3]) * h) - 1),
                min(w, math.ceil(uv[2] * w) + 1),
                min(h, math.ceil((1 - uv[1]) * h) + 1),
            )
        target = (
            options["mono_dpi"]
            if mono
            else (options["gray_dpi"] or options["dpi"])
            if components == 1
            else options["dpi"]
        )
        scale = 1
        if placement and target > 0 and placement[1] > target * options["threshold"]:
            scale = min(1, target / placement[1])
        cw, ch = box[2] - box[0], box[3] - box[1]
        size = (max(1, round(cw * scale)), max(1, round(ch * scale)))
        cropped = box != (0, 0, w, h)
        resized = size != (cw, ch)
        if (
            not cropped
            and not resized
            and not (mono and codecs != ["flate"])
            and not options["extended"]
        ):
            continue
        if bits == 16 and not encoded:
            data = obj.read_bytes()
            if len(data) != w * h * components * 2:
                raise ValueError("16-bit image length does not match its dimensions")
            if any(inversions):
                normalized = bytearray(data)
                for i in range(0, len(normalized), 2):
                    if inversions[(i // 2) % components]:
                        normalized[i] ^= 255
                        normalized[i + 1] ^= 255
                data = bytes(normalized)
            data = crop16(data, w, h, components, box)
            if resized:
                data = resize16(data, cw, ch, components, size)
            candidate = zlib.compress(data), pikepdf.Name.FlateDecode, None
        else:
            try:
                image = pikepdf.PdfImage(obj).as_pil_image()
            except (pikepdf.UnsupportedImageTypeError, NotImplementedError) as exc:
                preserved[type(exc).__name__] = preserved.get(type(exc).__name__, 0) + 1
                continue
            if image.size != (w, h) or image.mode not in ("1", "L", "RGB", "CMYK"):
                preserved["unsupported decoded image"] = (
                    preserved.get("unsupported decoded image", 0) + 1
                )
                continue
            if image.mode != {1: "L", 3: "RGB", 4: "CMYK"}[components] and not (
                mono and image.mode == "1"
            ):
                preserved["color model mismatch"] = (
                    preserved.get("color model mismatch", 0) + 1
                )
                continue
            image = image.crop(box)
            if mono:
                full = mono_candidate(image, codecs)
                if resized:
                    # Nearest-neighbor cannot introduce new colors in a bitonal image.
                    small = image.resize(size, Image.Resampling.NEAREST)
                    downsampled = mono_candidate(small, codecs)
                    if len(full[0]) < len(original) and len(
                        full[0]
                    ) <= FULL_RESOLUTION_SIZE_RATIO * len(downsampled[0]):
                        candidate, size = full, image.size
                    else:
                        candidate = downsampled
                else:
                    candidate = full
            else:
                if resized:
                    image = image.resize(size, Image.Resampling.LANCZOS)
                candidates = []
                if "flate" in options["codecs"]:
                    candidates.append(
                        (zlib.compress(image.tobytes()), pikepdf.Name.FlateDecode, None)
                    )
                if not options["lossless"] and "jpeg" in options["codecs"]:
                    data = io.BytesIO()
                    image.save(data, format="JPEG", quality=options["quality"])
                    candidates.append((data.getvalue(), pikepdf.Name.DCTDecode, None))
                candidate = min(candidates, key=lambda item: len(item[0]))
                bits = 8
        if not options["force"] and len(candidate[0]) >= len(original):
            continue
        obj.write(candidate[0], filter=candidate[1], decode_parms=candidate[2])
        obj.Width, obj.Height, obj.BitsPerComponent = size[0], size[1], bits
        if "/Decode" in obj:
            del obj["/Decode"]
        if components == 4 and candidate[1] == pikepdf.Name.DCTDecode:
            obj.Decode = pikepdf.Array([1, 0] * 4)
        if cropped:
            crops[obj.objgen] = (*box, w, h)
        changed += 1
    place_crops(pdf, streams, crops)
    if options["verbose"]:
        message(f"advanced images: {changed} changed; preserved {preserved}")


def flatten_links(pdf):
    import pikepdf

    for page in pdf.pages:
        kept, commands = [], []
        for annot in page.obj.get("/Annots", []):
            if annot.get("/Subtype") != pikepdf.Name.Link:
                kept.append(annot)
                continue
            appearance = annot.get("/AP", {}).get("/N")
            if appearance is None:
                border = annot.get("/Border", [0, 0, 1])
                width = float(annot.get("/BS", {}).get("/W", border[2]))
                if width > 0 and not int(annot.get("/F", 0)) & (1 | 2 | 32):
                    raise ValueError(
                        "a visible link border has no appearance stream; preserve it or explicitly use --strip links"
                    )
                continue
            if not isinstance(appearance, pikepdf.Stream):
                state = annot.get("/AS")
                appearance = appearance.get(state) if state is not None else None
            if not isinstance(appearance, pikepdf.Stream):
                raise TypeError("link appearance state cannot be flattened")
            if int(annot.get("/F", 0)) & (8 | 16):
                raise ValueError(
                    "NoZoom/NoRotate link appearances cannot be flattened safely"
                )
            if int(annot.get("/F", 0)) & (1 | 2 | 32):
                continue
            rect = list(map(float, annot.Rect))
            box = bounds(
                tuple(map(float, appearance.get("/Matrix", IDENTITY))),
                list(map(float, appearance.BBox)),
            )
            if not visible(box) or not visible(rect):
                raise ValueError("invalid link appearance rectangle")
            sx, sy = (
                (rect[2] - rect[0]) / (box[2] - box[0]),
                (rect[3] - rect[1]) / (box[3] - box[1]),
            )
            resources = pikepdf.Dictionary(
                inherited(page.obj, "/Resources", pikepdf.Dictionary())
            )
            xobjects = pikepdf.Dictionary(
                resources.get("/XObject", pikepdf.Dictionary())
            )
            name = f"/FlattenedLink{annot.objgen[0]}"
            while name in xobjects:
                name += "X"
            xobjects[name] = appearance
            resources.XObject = xobjects
            page.obj.Resources = resources
            commands.append(
                f"q {sx:.12g} 0 0 {sy:.12g} {rect[0] - sx * box[0]:.12g} {rect[1] - sy * box[1]:.12g} cm {name} Do Q\n"
            )
        if commands:
            page.contents_add(b"q\n", prepend=True)
            page.contents_add(("Q\n" + "".join(commands)).encode("ascii"))
        if kept:
            page.obj.Annots = pikepdf.Array(kept)
        elif "/Annots" in page.obj:
            del page.obj["/Annots"]


def font_glyph_usage(pdf):
    """Collect exact glyph IDs for Identity-H/V fonts; exclude other consumers."""
    import pikepdf

    used, blocked, visited_forms = {}, set(), set()

    def block_graph(obj, seen=None, depth=0):
        if depth > 100:
            raise ValueError("font resource nesting exceeds safety limit")
        if seen is None:
            seen = set()
        if isinstance(obj, (pikepdf.Dictionary, pikepdf.Stream)):
            if obj.is_indirect:
                if obj.objgen in seen:
                    return
                seen.add(obj.objgen)
            if "/FontFile2" in obj:
                blocked.add(obj.FontFile2.objgen)
            for key, value in obj.items():
                if str(key) not in ("/Parent", "/P", "/Pages", "/Root"):
                    block_graph(value, seen, depth + 1)
        elif isinstance(obj, pikepdf.Array):
            for value in obj:
                block_graph(value, seen, depth + 1)

    def glyphs(font, text):
        descendants = font.get("/DescendantFonts", []) if font is not None else []
        if len(descendants) != 1 or str(font.get("/Encoding")) not in (
            "/Identity-H",
            "/Identity-V",
        ):
            block_graph(font)
            return
        descendant = descendants[0]
        descriptor = descendant.get("/FontDescriptor", {})
        if (
            descendant.get("/Subtype") != pikepdf.Name.CIDFontType2
            or "/FontFile2" not in descriptor
        ):
            block_graph(font)
            return
        program = descriptor["/FontFile2"].objgen
        raw = bytes(text)
        if len(raw) % 2:
            blocked.add(program)
            return
        mapping = descendant.get("/CIDToGIDMap", pikepdf.Name.Identity)
        if isinstance(mapping, pikepdf.Stream):
            mapping = mapping.read_bytes()
            if len(mapping) > 131072 or len(mapping) % 2:
                blocked.add(program)
                return
        elif mapping == pikepdf.Name.Identity:
            mapping = None
        else:
            blocked.add(program)
            return
        selected = used.setdefault(program, set())
        for i in range(0, len(raw), 2):
            cid = int.from_bytes(raw[i : i + 2], "big")
            if mapping is not None:
                if 2 * cid + 2 > len(mapping):
                    blocked.add(program)
                    return
                cid = int.from_bytes(mapping[2 * cid : 2 * cid + 2], "big")
            selected.add(cid)

    visits = 0

    def walk(owner, resources, font=None, active=None):
        nonlocal visits
        visits += 1
        if active is None:
            active = set()
        obj = owner.obj if isinstance(owner, pikepdf.Page) else owner
        if obj.objgen in active or len(active) > 64 or visits > 100000:
            raise ValueError("font content traversal exceeds safety limit")
        active = active | {obj.objgen}
        visited_forms.add(obj.objgen)
        stack = []
        for instruction in pikepdf.parse_content_stream(owner):
            if not hasattr(instruction, "operator"):
                continue
            op, args = str(instruction.operator), instruction.operands
            if op == "q":
                if len(stack) >= 256:
                    raise ValueError("graphics state nesting exceeds safety limit")
                stack.append(font)
            elif op == "Q":
                if not stack:
                    raise ValueError("unbalanced graphics state")
                font = stack.pop()
            elif op == "Tf":
                font = resources.get("/Font", {}).get(args[0])
            elif op in ("Tj", "'", '"'):
                glyphs(font, args[-1])
            elif op == "TJ":
                for value in args[0]:
                    if isinstance(value, pikepdf.String):
                        glyphs(font, value)
            elif op == "Do":
                target = resources.get("/XObject", {}).get(args[0])
                if target is not None and target.get("/Subtype") == pikepdf.Name.Form:
                    walk(target, target.get("/Resources", resources), font, active)
        if stack:
            raise ValueError("unbalanced graphics state")

    for page in pdf.pages:
        walk(page, inherited(page.obj, "/Resources", pikepdf.Dictionary()))
        block_graph(page.obj.get("/Annots"))
    block_graph(pdf.Root.get("/AcroForm"))
    for obj in pdf.objects:
        if not isinstance(obj, (pikepdf.Dictionary, pikepdf.Stream)):
            continue
        for key in ("/Pattern", "/SMask"):
            block_graph(obj.get(key))
        if obj.get("/Subtype") == pikepdf.Name.Form and obj.objgen not in visited_forms:
            block_graph(obj)
        if obj.get("/Type") == pikepdf.Name.Font and obj.get("/Subtype") not in (
            pikepdf.Name.Type0,
            pikepdf.Name.CIDFontType2,
        ):
            block_graph(obj)
    return used, blocked


def merged_font(fonts, used_gids):
    """Union compatible retained-GID glyf programs without rewriting PDF text."""
    from fontTools.ttLib import newTable
    from fontTools.ttLib.tables._c_m_a_p import CmapSubtable

    prototype = fonts[0]
    required = ("glyf", "hmtx", "head", "hhea", "maxp", "name")
    unsupported = (
        "fvar",
        "gvar",
        "vmtx",
        "VORG",
        "hdmx",
        "LTSH",
        "COLR",
        "SVG ",
        "sbix",
    )
    if any(
        any(key not in font for key in required)
        or any(key in font for key in unsupported)
        for font in fonts
    ):
        return None
    for font in fonts[1:]:
        if (
            font["head"].unitsPerEm != prototype["head"].unitsPerEm
            or font["head"].fontRevision != prototype["head"].fontRevision
        ):
            return None
        for tag in ("cvt ", "fpgm", "prep"):
            if (font.getTableData(tag) if tag in font else None) != (
                prototype.getTableData(tag) if tag in prototype else None
            ):
                return None
    count = max(len(font.getGlyphOrder()) for font in fonts)
    order = [".notdef"] + [f"glyph{n:05d}" for n in range(1, count)]
    glyphs, metrics, fingerprints, cmapping = {}, {}, {}, {}
    for font, selected in zip(fonts, used_gids):
        old_order = font.getGlyphOrder()
        if selected and max(selected) >= len(old_order):
            return None
        for gid, name in enumerate(old_order):
            glyph = font["glyf"][name]
            metric = font["hmtx"].metrics[name]
            if (
                gid
                and gid not in selected
                and glyph.numberOfContours == 0
                and metric == (0, 0)
            ):
                continue  # Unused placeholder left by a retained-GID subset.
            fingerprint = (glyph.compile(font["glyf"]), metric)
            if gid in fingerprints and fingerprints[gid] != fingerprint:
                return None
            fingerprints[gid] = fingerprint
            glyph = copy.deepcopy(glyph)
            if glyph.isComposite():
                for component in glyph.components:
                    component.glyphName = order[font.getGlyphID(component.glyphName)]
            glyphs[order[gid]], metrics[order[gid]] = glyph, metric
        for code, name in (font.getBestCmap() or {}).items():
            gid = font.getGlyphID(name)
            if code in cmapping and cmapping[code] != order[gid]:
                return None
            cmapping[code] = order[gid]
    merged = copy.deepcopy(prototype)
    from fontTools.ttLib.tables._g_l_y_f import Glyph

    for name in order:
        glyphs.setdefault(name, Glyph())
        metrics.setdefault(name, (0, 0))
    # Layout tables are not used for Identity-H/V PDF glyph selection and can
    # contain stale glyph names after the union. Preserve rendering tables only.
    for tag in list(merged.keys()):
        if tag in ("GSUB", "GPOS", "GDEF", "kern", "BASE", "JSTF", "DSIG"):
            del merged[tag]
    merged["glyf"].glyphs = glyphs
    merged["glyf"].glyphOrder = order
    merged["hmtx"].metrics = metrics
    merged.setGlyphOrder(order)
    for key, value in vars(merged["maxp"]).items():
        if key.startswith("max") and isinstance(value, int):
            setattr(
                merged["maxp"], key, max(getattr(font["maxp"], key) for font in fonts)
            )
    merged["maxp"].numGlyphs = count
    merged["hhea"].advanceWidthMax = max(width for width, _ in metrics.values())
    if "post" in merged:
        merged["post"].formatType = 3.0
    table = newTable("cmap")
    table.tableVersion = 0
    table.tables = []
    for fmt, encoding in ((4, 1), (12, 10)):
        sub = CmapSubtable.newSubtable(fmt)
        sub.platformID, sub.platEncID, sub.language = 3, encoding, 0
        sub.cmap = {
            code: name for code, name in cmapping.items() if fmt == 12 or code < 65535
        }
        table.tables.append(sub)
    merged["cmap"] = table
    data = io.BytesIO()
    merged.save(data)
    return data.getvalue()


def merge_fonts(pdf, verbose):
    import pikepdf
    from fontTools.ttLib import TTFont

    groups, seen = {}, set()
    used, blocked = font_glyph_usage(pdf)
    for obj in pdf.objects:
        if (
            not isinstance(obj, pikepdf.Dictionary)
            or obj.get("/Subtype") != pikepdf.Name.CIDFontType2
        ):
            continue
        descriptor = obj.get("/FontDescriptor")
        if descriptor is None or "/FontFile2" not in descriptor:
            continue
        stream = descriptor.FontFile2
        if stream.objgen in blocked:
            continue
        identity = (descriptor.objgen, stream.objgen)
        if identity in seen:
            continue
        seen.add(identity)
        data = stream.read_bytes()
        if len(data) > 32 << 20:
            continue
        font = TTFont(io.BytesIO(data), lazy=False)
        name = re.sub(r"^[A-Z]{6}\+", "", str(descriptor.get("/FontName", "")))
        if not name:
            continue
        groups.setdefault(name, []).append((obj, descriptor, stream, data, font))
    replacements, merged_count = {}, 0
    for name, entries in groups.items():
        unique = {}
        for entry in entries:
            unique.setdefault(entry[2].objgen, entry)
        if len(unique) < 2:
            continue
        data = merged_font(
            [entry[4] for entry in unique.values()],
            [used.get(entry[2].objgen, set()) for entry in unique.values()],
        )
        if data is None or len(zlib.compress(data)) >= sum(
            len(entry[2].read_raw_bytes()) for entry in unique.values()
        ):
            continue
        stream = pdf.make_stream(data)
        stream.Length1 = len(data)
        prefix = "".join(
            chr(65 + int(c, 16)) for c in hashlib.sha256(data).hexdigest()[:6]
        )
        merged_name = pikepdf.Name("/" + prefix + "+" + name.lstrip("/"))
        for font_obj, descriptor, _, _, _ in entries:
            descriptor.FontFile2 = stream
            descriptor.FontName = merged_name
            font_obj.BaseFont = merged_name
            replacements[font_obj.objgen] = merged_name
        merged_count += 1
    for obj in pdf.objects:
        if (
            isinstance(obj, pikepdf.Dictionary)
            and obj.get("/Subtype") == pikepdf.Name.Type0
        ):
            for descendant in obj.get("/DescendantFonts", []):
                if descendant.objgen in replacements:
                    obj.BaseFont = replacements[descendant.objgen]
    if verbose:
        message(f"fonts: {merged_count} compatible TrueType groups merged")


def rasterize(document, output, options):
    import pikepdf
    import pymupdf
    from PIL import Image, ImageFilter

    result = pikepdf.Pdf.new()
    for number, page in enumerate(document):
        dpi = options["render_dpi"]
        pixels = math.ceil(page.rect.width * dpi / 72) * math.ceil(
            page.rect.height * dpi / 72
        )
        if pixels * 24 > options["memory_mib"] * (1 << 20):
            raise ValueError(
                "rendered page exceeds the image memory budget; lower --render-dpi"
            )
        pixmap = page.get_pixmap(
            dpi=dpi, colorspace=pymupdf.csRGB, alpha=False, annots=True
        )
        image = Image.frombytes("RGB", (pixmap.width, pixmap.height), pixmap.samples)
        target = result.add_blank_page(page_size=(page.rect.width, page.rect.height))
        xobjects = pikepdf.Dictionary()
        if options["mrc"]:
            # ponytail: luminance segmentation suits document scans; photographs
            # need layout-aware segmentation instead of a single text threshold.
            mask = image.convert("L").point(
                lambda value: 0 if value < 96 else 255, mode="1"
            )
            background = image.copy()
            filled = image.filter(ImageFilter.MaxFilter(9))
            from PIL import ImageChops

            background.paste(filled, mask=ImageChops.invert(mask).convert("L"))
            ratio = min(1, options["background_dpi"] / dpi)
            background = background.resize(
                (
                    max(1, round(image.width * ratio)),
                    max(1, round(image.height * ratio)),
                ),
                Image.Resampling.LANCZOS,
            )
            payload, filter_name, parms = mono_candidate(
                mask, options["mono_codecs"].split(",")
            )
            foreground = result.make_stream(payload)
            foreground.Type, foreground.Subtype = (
                pikepdf.Name.XObject,
                pikepdf.Name.Image,
            )
            foreground.Width, foreground.Height, foreground.BitsPerComponent = (
                mask.width,
                mask.height,
                1,
            )
            foreground.ImageMask, foreground.Decode = True, pikepdf.Array([0, 1])
            foreground.Filter = filter_name
            if parms is not None:
                foreground.DecodeParms = parms
            xobjects.Foreground = foreground
        else:
            background = image
        data = io.BytesIO()
        background.save(data, format="JPEG", quality=options["quality"])
        stream = result.make_stream(data.getvalue())
        stream.Type, stream.Subtype, stream.ColorSpace = (
            pikepdf.Name.XObject,
            pikepdf.Name.Image,
            pikepdf.Name.DeviceRGB,
        )
        stream.Width, stream.Height, stream.BitsPerComponent = (
            background.width,
            background.height,
            8,
        )
        stream.Filter = pikepdf.Name.DCTDecode
        xobjects.Background = stream
        target.obj.Resources = pikepdf.Dictionary(XObject=xobjects)
        content = f"q {page.rect.width:.12g} 0 0 {page.rect.height:.12g} 0 0 cm /Background Do"
        if options["mrc"]:
            content += " 0 g /Foreground Do"
        target.obj.Contents = result.make_stream((content + " Q\n").encode("ascii"))
        if options["verbose"]:
            message(f"rendered page {number + 1}/{len(document)}")
    result.save(output, object_stream_mode=pikepdf.ObjectStreamMode.generate)
    result.close()


def transform(source, output, options):
    import pikepdf

    with pikepdf.Pdf.open(source) as pdf:
        if (
            options["geometry"]
            and (
                options["dpi"]
                or options["gray_dpi"]
                or options["mono_dpi"]
                or options["clip"]
            )
            or options["extended"]
            or options["mono_codecs"] != "flate"
        ):
            process_images(pdf, options)
        if options["merge_fonts"]:
            merge_fonts(pdf, options["verbose"])
        if options["flatten"] == "all" or "links" in options["flatten"].split(","):
            flatten_links(pdf)
        needs_mupdf = (
            options["subset_fonts"]
            or options["gray"]
            or options["bitmap"]
            or options["mrc"]
            or any(
                kind in options["flatten"].split(",")
                for kind in ("all", "annotations", "forms")
            )
        )
        intermediate = str(Path(WORK) / "prepared.pdf") if needs_mupdf else output
        pdf.save(intermediate, object_stream_mode=pikepdf.ObjectStreamMode.generate)
    if needs_mupdf:
        import pymupdf

        with pymupdf.open(intermediate) as document:
            if options["gray"]:
                document.recolor(1)
            flatten = options["flatten"].split(",")
            if "all" in flatten or "annotations" in flatten or "forms" in flatten:
                document.bake(
                    annots="all" in flatten or "annotations" in flatten,
                    widgets="all" in flatten or "forms" in flatten,
                )
            if options["bitmap"] or options["mrc"]:
                message(
                    "warning: raster conversion removes searchable text, forms, links, tags, and attachments"
                )
                rasterize(document, output, options)
            else:
                if options["subset_fonts"]:
                    document.subset_fonts()
                document.save(
                    output, garbage=4, deflate=True, deflate_fonts=True, use_objstms=1
                )


def extract_text(source, output, options):
    import pymupdf

    with pymupdf.open(source) as document:
        if document.needs_pass and not document.authenticate(
            options.get("password", "")
        ):
            raise ValueError("incorrect PDF password")
        with open(output, "w", encoding="utf-8") as target:
            for page in document:
                target.write(page.get_text())
                target.write("\f")


def main():
    global WORK
    operation, source, output = sys.argv[1:]
    WORK = str(Path(output).parent)
    options = json.loads(sys.stdin.read(1 << 20))
    if operation == "transform":
        transform(source, output, options)
    elif operation == "monochrome":
        import pikepdf

        options.update(geometry=False, extended=False, clip=False)
        with pikepdf.Pdf.open(source) as pdf:
            process_images(pdf, options)
            pdf.save(output, object_stream_mode=pikepdf.ObjectStreamMode.generate)
    elif operation == "text":
        extract_text(source, output, options)
    else:
        raise ValueError("unknown PDF tools operation")


if __name__ == "__main__":
    try:
        main()
    except ImportError as exc:
        message(
            f"Missing optional module {exc.name}. Install tools/requirements.txt in PDF_SQUEEZER_PYTHON's environment."
        )
        sys.exit(1)
    except Exception as exc:  # noqa: BLE001 -- redact exceptions at the process boundary
        # Do not include tracebacks or input options, which may contain passwords.
        message(f"PDF tools: {exc}")
        sys.exit(1)
