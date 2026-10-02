"""Regression checks for the optional PDF operations; all documents are synthetic."""

import hashlib
import importlib.util
import io
import os
import random
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import pikepdf
import pymupdf
from PIL import Image, ImageDraw

spec = importlib.util.spec_from_file_location(
    "pdf_tools", Path(__file__).with_name("pdf_tools.py")
)
tools = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tools)


def options(**changes):
    result = {
        "dpi": 0,
        "gray_dpi": 0,
        "mono_dpi": 0,
        "threshold": 1.5,
        "quality": 75,
        "clip": False,
        "extended": False,
        "lossless": False,
        "reduce_bits": False,
        "force": False,
        "mono_codecs": "flate",
        "codecs": "flate,jpeg",
        "memory_mib": 512,
        "gray": False,
        "geometry": True,
        "flatten": "",
        "subset_fonts": False,
        "merge_fonts": False,
        "bitmap": False,
        "mrc": False,
        "render_dpi": 200,
        "background_dpi": 72,
        "verbose": True,
        "strip": "",
        "font_files": {},
        "output_intent": "",
        "srgb_icc": str(Path(__file__).with_name("sRGB2014.icc")),
    }
    result.update(changes)
    return result


def text_page(pdf, fonts, content, size=(200, 100)):
    page = pdf.add_blank_page(page_size=size)
    page.obj.Resources = pikepdf.Dictionary(Font=pikepdf.Dictionary(**fonts))
    page.obj.Contents = pdf.make_stream(content)
    return page


def simple_font(pdf, base_font, **extra):
    return pdf.make_indirect(
        pikepdf.Dictionary(
            Type=pikepdf.Name.Font,
            Subtype=pikepdf.Name.Type1,
            BaseFont=pikepdf.Name("/" + base_font),
            **extra,
        )
    )


def fontconfig_file(family):
    binary = shutil.which("fc-match")
    if not binary:
        return None
    result = subprocess.run(
        [binary, "-f", "%{family}|%{file}", family],
        capture_output=True,
        text=True,
        check=False,
    )
    matched, _, path = result.stdout.partition("|")
    return path if family.lower() in matched.lower() else None


def image_object(pdf, image):
    stream = pdf.make_stream(image.tobytes())
    stream.Type, stream.Subtype = pikepdf.Name.XObject, pikepdf.Name.Image
    stream.Width, stream.Height = image.size
    stream.BitsPerComponent = 1 if image.mode == "1" else 8
    stream.ColorSpace = {
        "1": pikepdf.Name.DeviceGray,
        "L": pikepdf.Name.DeviceGray,
        "RGB": pikepdf.Name.DeviceRGB,
        "CMYK": pikepdf.Name.DeviceCMYK,
    }[image.mode]
    return stream


def rendered(path):
    with pymupdf.open(path) as doc:
        return [
            (
                tuple(page.rect),
                hashlib.sha256(page.get_pixmap(alpha=False).samples).hexdigest(),
            )
            for page in doc
        ]


def rendered_samples(path, isolate_pages=False):
    """Pixels per page. isolate_pages renders each page from a one-page copy, which
    sidesteps MuPDF caching a resource-less Type 3 font with the first page's
    resources."""
    if isolate_pages:
        samples = []
        with pikepdf.Pdf.open(path) as source:
            for index in range(len(source.pages)):
                single = pikepdf.Pdf.new()
                single.pages.append(source.pages[index])
                buffer = io.BytesIO()
                single.save(buffer)
                with pymupdf.open(stream=buffer.getvalue(), filetype="pdf") as doc:
                    pixmap = doc[0].get_pixmap(alpha=False)
                    samples.append((pixmap.width, pixmap.height, bytes(pixmap.samples)))
        return samples
    with pymupdf.open(path) as doc:
        pixmaps = [page.get_pixmap(alpha=False) for page in doc]
        return [(p.width, p.height, bytes(p.samples)) for p in pixmaps]


def embedded_program(font):
    from fontTools.ttLib import TTFont

    descriptor = font.FontDescriptor
    key = "/FontFile2" if "/FontFile2" in descriptor else "/FontFile3"
    return TTFont(io.BytesIO(descriptor[key].read_bytes()))


class PDFToolsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        tools.WORK = self.directory.name
        self.source = str(Path(tools.WORK) / "source.pdf")
        self.output = str(Path(tools.WORK) / "output.pdf")

    def save_image(self, image, content=None, page_size=None):
        pdf = pikepdf.Pdf.new()
        page = pdf.add_blank_page(page_size=page_size or image.size)
        page.obj.Resources = pikepdf.Dictionary(
            XObject=pikepdf.Dictionary(Im=image_object(pdf, image))
        )
        page.obj.Contents = pdf.make_stream(
            content or f"q {image.width} 0 0 {image.height} 0 0 cm /Im Do Q".encode()
        )
        pdf.save(self.source, compress_streams=False)
        pdf.close()

    def test_crop_shared_form_and_page_image_preserves_rendering(self):
        pdf = pikepdf.Pdf.new()
        data = random.Random(19).randbytes(128 * 128 * 3)
        image = image_object(pdf, Image.frombytes("RGB", (128, 128), data))
        form = pdf.make_stream(b"q 128 0 0 128 0 0 cm /Im Do Q")
        form.Type, form.Subtype, form.BBox = (
            pikepdf.Name.XObject,
            pikepdf.Name.Form,
            pikepdf.Array([0, 0, 128, 128]),
        )
        form.Resources = pikepdf.Dictionary(XObject=pikepdf.Dictionary(Im=image))
        for size, x in ((64, -32), (80, -24)):
            page = pdf.add_blank_page(page_size=(size, size))
            page.obj.Resources = pikepdf.Dictionary(XObject=pikepdf.Dictionary(Fm=form))
            page.obj.Contents = pdf.make_stream(
                f"q 1 0 0 1 {x} -24 cm /Fm Do Q".encode()
            )
        pdf.save(self.source, compress_streams=False)
        pdf.close()
        before = rendered(self.source)
        tools.transform(
            self.source, self.output, options(clip=True, lossless=True, codecs="flate")
        )
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as result:
            images = [
                o
                for o in result.objects
                if isinstance(o, pikepdf.Stream)
                and o.get("/Subtype") == pikepdf.Name.Image
            ]
            self.assertEqual(len(images), 1)
            self.assertLess(int(images[0].Width), 128)
            self.assertGreater(int(images[0].Width), 80)

    def test_form_borrowing_page_resources_preserves_images(self):
        pdf = pikepdf.Pdf.new()
        form = pdf.make_stream(b"q 128 0 0 128 0 0 cm /Im Do Q")
        form.Type, form.Subtype, form.BBox = (
            pikepdf.Name.XObject,
            pikepdf.Name.Form,
            pikepdf.Array([0, 0, 128, 128]),
        )
        # One shared form, no /Resources of its own: /Im means a different
        # image on each page, so neither image may be cropped.
        for seed, x in ((19, -32), (23, -96)):
            data = random.Random(seed).randbytes(128 * 128 * 3)
            image = image_object(pdf, Image.frombytes("RGB", (128, 128), data))
            page = pdf.add_blank_page(page_size=(64, 64))
            page.obj.Resources = pikepdf.Dictionary(
                XObject=pikepdf.Dictionary(Fm=form, Im=image)
            )
            page.obj.Contents = pdf.make_stream(
                f"q 1 0 0 1 {x} -32 cm /Fm Do Q".encode()
            )
        pdf.save(self.source, compress_streams=False)
        pdf.close()
        before = rendered(self.source)
        tools.transform(
            self.source, self.output, options(clip=True, lossless=True, codecs="flate")
        )
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as result:
            widths = [int(page.Resources.XObject.Im.Width) for page in result.pages]
            self.assertEqual(widths, [128, 128])

    def test_downsampling_accounts_for_largest_shared_placement(self):
        pdf = pikepdf.Pdf.new()
        image = image_object(
            pdf, Image.frombytes("RGB", (300, 300), random.Random(7).randbytes(270000))
        )
        for size, unit in ((72, 1), (72, 2)):
            page = pdf.add_blank_page(page_size=(size, size))
            page.obj.UserUnit = unit
            page.obj.Resources = pikepdf.Dictionary(
                XObject=pikepdf.Dictionary(Im=image)
            )
            page.obj.Contents = pdf.make_stream(
                f"q {size} 0 0 {size} 0 0 cm /Im Do Q".encode()
            )
        pdf.save(self.source, compress_streams=False)
        pdf.close()
        tools.transform(self.source, self.output, options(dpi=72))
        with pikepdf.Pdf.open(self.output) as result:
            image = result.pages[0].Resources.XObject.Im
            self.assertEqual((int(image.Width), int(image.Height)), (144, 144))

    def test_rotated_and_sheared_clip_is_conservative(self):
        self.save_image(
            Image.frombytes("RGB", (100, 100), random.Random(3).randbytes(30000)),
            b"q 60 20 -20 60 30 -20 cm /Im Do Q",
            (60, 60),
        )
        before = rendered(self.source)
        tools.transform(
            self.source, self.output, options(clip=True, lossless=True, codecs="flate")
        )
        self.assertEqual(before, rendered(self.output))

    def test_monochrome_codecs_are_pixel_exact(self):
        image = Image.new("1", (256, 128), 1)
        draw = ImageDraw.Draw(image)
        draw.text((5, 5), "0123456789 repeated small text", fill=0)
        draw.rectangle((19, 31, 100, 59), outline=0)
        draw.line((0, 127, 255, 0), fill=0)
        for codec in ("ccitt", "jbig2"):
            with self.subTest(codec=codec):
                if codec == "jbig2" and not shutil.which("jbig2"):
                    if os.environ.get("PDF_SQUEEZER_INTEGRATION") == "1":
                        self.fail("jbig2 is required for integration tests")
                    continue
                self.save_image(image)
                before = rendered(self.source)
                tools.transform(
                    self.source, self.output, options(mono_codecs=codec, force=True)
                )
                self.assertEqual(before, rendered(self.output))
                with pikepdf.Pdf.open(self.output) as result:
                    filter_name = str(result.pages[0].Resources.XObject.Im.Filter)
                    self.assertEqual(
                        filter_name,
                        "/CCITTFaxDecode" if codec == "ccitt" else "/JBIG2Decode",
                    )

    def test_16_bit_resize_and_crop_keep_sample_precision(self):
        samples = bytes.fromhex("0102030405060708")
        cropped = tools.crop16(samples, 2, 2, 1, (1, 0, 2, 2))
        self.assertEqual(cropped, bytes.fromhex("03040708"))
        self.assertEqual(
            tools.resize16(samples, 2, 2, 1, (1, 1)), bytes.fromhex("0405")
        )
        self.assertEqual(tools.resize16(samples, 2, 2, 1, (2, 2)), samples)

    def test_cmyk_extended_reencoding_preserves_colors(self):
        image = Image.new("CMYK", (100, 100), (255, 0, 80, 10))
        self.save_image(image)
        before = rendered(self.source)
        tools.transform(
            self.source,
            self.output,
            options(extended=True, lossless=True, codecs="flate", force=True),
        )
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as result:
            self.assertEqual(
                result.pages[0].Resources.XObject.Im.ColorSpace, pikepdf.Name.DeviceCMYK
            )

    def test_cmyk_jpeg_decode_array(self):
        image = Image.new("CMYK", (64, 64), (200, 30, 100, 20))
        buffer = io.BytesIO()
        image.save(buffer, format="JPEG", quality=90)
        self.save_image(image)
        with pikepdf.Pdf.open(self.source, allow_overwriting_input=True) as pdf:
            stream = pdf.pages[0].Resources.XObject.Im
            stream.write(buffer.getvalue(), filter=pikepdf.Name.DCTDecode)
            stream.Decode = pikepdf.Array([1, 0] * 4)
            pdf.save(self.source)
        before = rendered(self.source)
        tools.transform(
            self.source, self.output, options(extended=True, codecs="flate", force=True)
        )
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as pdf:
            self.assertEqual(
                pdf.pages[0].Resources.XObject.Im.Filter, pikepdf.Name.FlateDecode
            )

    def test_encoded_images_keep_decode_semantics(self):
        for mode, fmt, decode in (
            ("L", "JPEG", [1, 0]),
            ("CMYK", "JPEG", None),
            ("CMYK", "JPEG2000", [1, 0] * 4),
        ):
            with self.subTest(mode=mode, fmt=fmt, decode=decode):
                image = Image.new(
                    mode, (64, 64), 40 if mode == "L" else (200, 30, 100, 20)
                )
                buffer = io.BytesIO()
                image.save(buffer, format=fmt)
                self.save_image(image)
                with pikepdf.Pdf.open(self.source, allow_overwriting_input=True) as pdf:
                    stream = pdf.pages[0].Resources.XObject.Im
                    stream.write(
                        buffer.getvalue(),
                        filter=pikepdf.Name.DCTDecode
                        if fmt == "JPEG"
                        else pikepdf.Name.JPXDecode,
                    )
                    if decode:
                        stream.Decode = pikepdf.Array(decode)
                    pdf.save(self.source)
                before = rendered(self.source)
                tools.transform(
                    self.source,
                    self.output,
                    options(extended=True, codecs="flate", force=True),
                )
                self.assertEqual(before, rendered(self.output))

    def test_fractional_axis_aligned_crop(self):
        self.save_image(
            Image.frombytes(
                "RGB", (257, 123), random.Random(9).randbytes(257 * 123 * 3)
            ),
            b"q 52.3 0 0 42.1 -10.4 -11.7 cm /Im Do Q",
            (30, 20),
        )
        before = rendered(self.source)
        tools.transform(
            self.source, self.output, options(clip=True, lossless=True, codecs="flate")
        )
        self.assertEqual(before, rendered(self.output))

    def test_flatten_borderless_links_and_reject_missing_appearance(self):
        with pymupdf.open() as document:
            page = document.new_page(width=200, height=100)
            page.insert_text((10, 30), "Example link")
            page.insert_link(
                {
                    "kind": pymupdf.LINK_URI,
                    "from": pymupdf.Rect(10, 10, 100, 35),
                    "uri": "https://example.invalid/",
                }
            )
            document.save(self.source)
        before = rendered(self.source)
        tools.transform(self.source, self.output, options(flatten="links"))
        self.assertEqual(before, rendered(self.output))
        with pymupdf.open(self.output) as document:
            self.assertEqual(document[0].get_links(), [])
        with pikepdf.Pdf.open(self.source, allow_overwriting_input=True) as pdf:
            pdf.pages[0].Annots[0].BS = pikepdf.Dictionary(W=2)
            pdf.save(self.source)
        with self.assertRaisesRegex(ValueError, "visible link border"):
            tools.transform(self.source, self.output, options(flatten="links"))

    def test_jpeg2000_decoding(self):
        image = Image.new("RGB", (64, 64), (30, 80, 180))
        buffer = io.BytesIO()
        image.save(buffer, format="JPEG2000", irreversible=False)
        self.save_image(image)
        with pikepdf.Pdf.open(self.source, allow_overwriting_input=True) as pdf:
            pdf.pages[0].Resources.XObject.Im.write(
                buffer.getvalue(), filter=pikepdf.Name.JPXDecode
            )
            pdf.save(self.source)
        tools.transform(
            self.source, self.output, options(extended=True, codecs="flate", force=True)
        )
        self.assertEqual(rendered(self.source), rendered(self.output))

    def test_subset_fonts_preserves_text_and_rendering(self):
        font = Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
        if not font.exists():
            self.skipTest("DejaVu font fixture is not installed")
        with pymupdf.open() as doc:
            page = doc.new_page(width=240, height=120)
            page.insert_font(fontname="Embedded", fontfile=str(font))
            page.insert_text(
                (15, 45), "Subset this font.", fontname="Embedded", fontsize=18
            )
            doc.save(self.source)
        before = rendered(self.source)
        tools.transform(self.source, self.output, options(subset_fonts=True))
        self.assertEqual(before, rendered(self.output))
        self.assertLess(
            Path(self.output).stat().st_size, Path(self.source).stat().st_size / 2
        )
        with pymupdf.open(self.output) as doc:
            self.assertIn("Subset this font.", doc[0].get_text())

    def test_merge_compatible_retained_gid_fonts(self):
        from fontTools import subset
        from fontTools.ttLib import TTFont

        font = Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
        if not font.exists():
            self.skipTest("DejaVu font fixture is not installed")
        with pymupdf.open() as doc:
            for text in ("ABC", "XYZ"):
                tt = TTFont(font)
                config = subset.Options()
                config.retain_gids = True
                subsetter = subset.Subsetter(options=config)
                subsetter.populate(text=text)
                subsetter.subset(tt)
                buffer = io.BytesIO()
                tt.save(buffer)
                page = doc.new_page(width=160, height=100)
                page.insert_font(fontname="Embedded", fontbuffer=buffer.getvalue())
                page.insert_text((10, 40), text, fontname="Embedded", fontsize=20)
            doc.save(self.source)
        before = rendered(self.source)
        tools.transform(self.source, self.output, options(merge_fonts=True))
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as doc:
            programs = {
                o.FontFile2.objgen
                for o in doc.objects
                if isinstance(o, pikepdf.Dictionary) and "/FontFile2" in o
            }
            self.assertEqual(len(programs), 1)

    def test_flatten_forms_keeps_visible_value(self):
        with pymupdf.open() as doc:
            page = doc.new_page(width=240, height=100)
            widget = pymupdf.Widget()
            widget.field_name, widget.field_value = "example", "VISIBLE"
            widget.field_type = pymupdf.PDF_WIDGET_TYPE_TEXT
            widget.rect = pymupdf.Rect(10, 10, 220, 50)
            widget.text_fontsize = 18
            page.add_widget(widget)
            doc.save(self.source)
        before = rendered(self.source)
        tools.transform(self.source, self.output, options(flatten="forms"))
        self.assertEqual(before, rendered(self.output))
        with pymupdf.open(self.output) as doc:
            self.assertFalse(doc.is_form_pdf)
            self.assertIn("VISIBLE", doc[0].get_text())

    def convert_pdfa(self, **changes):
        return tools.pdfa.convert(
            self.source, self.output, options(**changes), lambda _: None
        )

    def assert_rendering_close(
        self, before, after, mean_limit=2.0, changed_limit=0.02, isolate_pages=False
    ):
        """Pages must match within anti-aliasing noise: same size, few changed pixels."""
        pages_before = rendered_samples(before, isolate_pages)
        pages_after = rendered_samples(after, isolate_pages)
        self.assertEqual(len(pages_before), len(pages_after))
        for (width, height, a), (width_after, height_after, b) in zip(
            pages_before, pages_after
        ):
            self.assertEqual((width, height), (width_after, height_after))
            diffs = [abs(x - y) for x, y in zip(a, b)]
            self.assertLess(sum(diffs) / len(diffs), mean_limit)
            self.assertLess(sum(1 for d in diffs if d > 64) / len(diffs), changed_limit)

    def assert_pdfa(self, flavour):
        binary = shutil.which("verapdf")
        if not binary:
            if os.environ.get("PDF_SQUEEZER_INTEGRATION") == "1":
                self.fail("veraPDF is required for integration tests")
            return
        report = subprocess.run(
            [binary, "--format", "xml", "--flavour", flavour, self.output],
            capture_output=True,
            text=True,
            check=False,
        ).stdout
        self.assertIn('isCompliant="true"', report, report[:6000])

    def test_pdfa4_structure_metadata_and_color(self):
        pdf = pikepdf.Pdf.new()
        image = image_object(pdf, Image.new("RGB", (8, 8), (10, 20, 30)))
        image.Interpolate = True
        group = pdf.make_indirect(
            pikepdf.Dictionary(Type=pikepdf.Name.OCG, Name=pikepdf.String("Layer"))
        )
        page = pdf.add_blank_page(page_size=(100, 100))
        page.obj.Resources = pikepdf.Dictionary(
            ExtGState=pikepdf.Dictionary(
                G=pikepdf.Dictionary(
                    TR=pikepdf.Name.Identity, HTO=[0, 0], TR2=pikepdf.Name.Default
                )
            ),
            XObject=pikepdf.Dictionary(Im=image),
        )
        page.obj.Contents = pdf.make_stream(
            b"/G gs 0 0 1 rg 10 10 30 30 re f q 50 0 0 50 25 25 cm /Im Do Q"
        )
        pdf.Root.Requirements = pikepdf.Array()
        pdf.Root.Version = pikepdf.Name("/1.7")
        pdf.Root.OCProperties = pikepdf.Dictionary(
            OCGs=pikepdf.Array([group]), D=pikepdf.Dictionary(Order=pikepdf.Array())
        )
        pdf.docinfo["/Title"] = "Example title"
        pdf.docinfo["/Author"] = "Jane Example"
        pdf.save(self.source)
        self.assertEqual(self.convert_pdfa(), "4")
        with pikepdf.Pdf.open(self.output) as result:
            self.assertEqual(result.pdf_version, "2.0")
            self.assertNotIn("/Info", result.trailer)
            self.assertIn("/ID", result.trailer)
            meta = result.open_metadata()
            self.assertEqual(meta["pdfaid:part"], "4")
            self.assertEqual(meta["pdfaid:rev"], "2020")
            self.assertNotIn("pdfaid:conformance", meta)
            self.assertEqual(meta["dc:title"], "Example title")
            self.assertEqual(list(meta["dc:creator"]), ["Jane Example"])
            intent = result.Root.OutputIntents[0]
            self.assertEqual(intent.S, pikepdf.Name.GTS_PDFA1)
            self.assertEqual(int(intent.DestOutputProfile.N), 3)
            resources = result.pages[0].Resources
            state = resources.ExtGState.G
            self.assertNotIn("/TR", state)
            self.assertNotIn("/HTO", state)
            self.assertEqual(state.TR2, pikepdf.Name.Default)
            self.assertFalse(resources.XObject.Im.Interpolate)
            self.assertNotIn("/Requirements", result.Root)
            self.assertNotIn("/Version", result.Root)
            config = result.Root.OCProperties.D
            self.assertEqual(str(config.Name), "Default")
            self.assertEqual(
                config.Order[0].objgen, result.Root.OCProperties.OCGs[0].objgen
            )
        self.assert_pdfa("4")

    def test_pdfa4_embeds_standard_fonts(self):
        if not fontconfig_file("Nimbus Sans") and not fontconfig_file(
            "Liberation Sans"
        ):
            self.skipTest("no metric-compatible fonts are installed")
        pdf = pikepdf.Pdf.new()
        fonts = {
            "F1": simple_font(pdf, "Helvetica"),
            "F2": simple_font(pdf, "Times-Bold", Encoding=pikepdf.Name.WinAnsiEncoding),
            "F3": simple_font(pdf, "Symbol"),
            # A direct font dictionary, which has no object number of its own.
            "F4": pikepdf.Dictionary(
                Type=pikepdf.Name.Font,
                Subtype=pikepdf.Name.Type1,
                BaseFont=pikepdf.Name.Courier,
            ),
        }
        # The form draws with the font the page selected; it has no Tf of its own.
        form = pdf.make_stream(b"BT 0 0 Td (zq) Tj ET")
        form.Type, form.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        form.BBox = pikepdf.Array([0, 0, 200, 100])
        page = text_page(
            pdf,
            fonts,
            b"BT /F1 18 Tf 10 70 Td (Hello, archive! \xe9) Tj /F2 18 Tf 0 -25 Td (Bold \xe9t\xe9) Tj"
            b" /F3 18 Tf 0 -25 Td (abg) Tj /F4 12 Tf 0 -15 Td (mono) Tj ET"
            b" BT /F1 12 Tf ET q 1 0 0 1 150 10 cm /Fm Do Q",
        )
        page.obj.Resources.XObject = pikepdf.Dictionary(Fm=form)
        pdf.save(self.source)
        self.convert_pdfa()
        with pikepdf.Pdf.open(self.output) as result:
            for key in ("/F1", "/F2", "/F3", "/F4"):
                font = result.pages[0].Resources.Font[key]
                descriptor = font.FontDescriptor
                self.assertIn("/FontFile3", descriptor, key)
                self.assertEqual(descriptor.FontFile3.Subtype, pikepdf.Name.Type1C)
                self.assertEqual(
                    int(font.LastChar) - int(font.FirstChar) + 1, len(font.Widths)
                )
                self.assertTrue(
                    str(font.BaseFont).startswith("/") and "+" in str(font.BaseFont)
                )
            self.assertEqual(
                int(result.pages[0].Resources.Font.F3.FontDescriptor.Flags) & 4, 4
            )
            helvetica = result.pages[0].Resources.Font.F1
            for code in (ord("z"), ord("q")):
                self.assertGreater(
                    int(helvetica.Widths[code - int(helvetica.FirstChar)]), 0
                )
        self.assert_rendering_close(self.source, self.output)
        self.assert_pdfa("4")

    def test_pdfa4_font_widths_must_match_or_come_from_font_file(self):
        liberation = fontconfig_file("Liberation Sans")
        if not liberation:
            self.skipTest("Liberation Sans is not installed")
        pdf = pikepdf.Pdf.new()
        font = simple_font(
            pdf,
            "Arial",
            Encoding=pikepdf.Name.WinAnsiEncoding,
            FirstChar=65,
            LastChar=67,
            Widths=pikepdf.Array([500, 500, 500]),
        )
        text_page(pdf, {"F1": font}, b"BT /F1 18 Tf 10 50 Td (ABC) Tj ET")
        pdf.save(self.source)
        with self.assertRaisesRegex(tools.pdfa.ConversionError, "--font-file"):
            self.convert_pdfa()
        self.convert_pdfa(font_files={"Arial": liberation})
        with pikepdf.Pdf.open(self.output) as result:
            font = result.pages[0].Resources.Font.F1
            self.assertEqual(font.Subtype, pikepdf.Name.TrueType)
            self.assertIn("/FontFile2", font.FontDescriptor)
            self.assertEqual(font.Encoding.BaseEncoding, pikepdf.Name.WinAnsiEncoding)
            # The document's layout wins: its widths stay and the program is bent to them.
            self.assertEqual([int(w) for w in font.Widths], [500, 500, 500])
            program = embedded_program(font)
            scale = 1000 / program["head"].unitsPerEm
            for name in ("A", "B", "C"):
                self.assertAlmostEqual(program["hmtx"][name][0] * scale, 500, delta=1)
        self.assert_pdfa("4")

    def test_pdfa4_aligns_embedded_font_widths_without_moving_text(self):
        from fontTools.ttLib import TTFont

        liberation = fontconfig_file("Liberation Sans")
        if not liberation:
            self.skipTest("Liberation Sans is not installed")
        tt = TTFont(liberation)
        tools.pdfa.subset_font(tt, [".notdef", "A", "B", "C"])
        buffer = io.BytesIO()
        tt.save(buffer)
        scale = 1000 / tt["head"].unitsPerEm
        real = [round(tt["hmtx"][name][0] * scale) for name in ("A", "B", "C")]
        pdf = pikepdf.Pdf.new()
        program = pdf.make_stream(buffer.getvalue())
        program.Length1 = len(buffer.getvalue())
        descriptor = tools.pdfa.descriptor_for(pdf, tt, "LiberationSans", False, False)
        descriptor.FontFile2 = program
        font = pdf.make_indirect(
            pikepdf.Dictionary(
                Type=pikepdf.Name.Font,
                Subtype=pikepdf.Name.TrueType,
                BaseFont=pikepdf.Name.LiberationSans,
                FontDescriptor=descriptor,
                Encoding=pikepdf.Name.WinAnsiEncoding,
                FirstChar=65,
                LastChar=67,
                Widths=pikepdf.Array([w + 50 for w in real]),
            )
        )
        text_page(pdf, {"F1": font}, b"BT /F1 24 Tf 10 40 Td (ABC) Tj ET")
        pdf.save(self.source)
        self.convert_pdfa()
        self.assert_rendering_close(self.source, self.output)
        with pikepdf.Pdf.open(self.output) as result:
            font = result.pages[0].Resources.Font.F1
            self.assertEqual([int(w) for w in font.Widths], [w + 50 for w in real])
            program = embedded_program(font)
            scale = 1000 / program["head"].unitsPerEm
            for name, width in zip(("A", "B", "C"), real):
                self.assertAlmostEqual(
                    program["hmtx"][name][0] * scale, width + 50, delta=1
                )
        self.assert_pdfa("4")

    def test_pdfa4_cmyk_needs_a_cmyk_output_intent(self):
        self.save_image(
            Image.new("RGB", (8, 8), (0, 0, 0)), b"0 0 0 1 k 10 10 40 40 re f", (64, 64)
        )
        with self.assertRaisesRegex(tools.pdfa.ConversionError, "--output-intent"):
            self.convert_pdfa()
        profiles = sorted(
            Path("/usr/share/ghostscript").glob("*/iccprofiles/default_cmyk.icc")
        )
        if not profiles:
            self.skipTest("no CMYK ICC profile is available")
        self.convert_pdfa(output_intent=str(profiles[-1]))
        with pikepdf.Pdf.open(self.output) as result:
            self.assertEqual(int(result.Root.OutputIntents[0].DestOutputProfile.N), 4)
            self.assertIn("/DefaultRGB", result.pages[0].Resources.ColorSpace)
        self.assert_pdfa("4")

    def test_pdfa4_forbidden_features_need_strip(self):
        pdf = pikepdf.Pdf.new()
        page = pdf.add_blank_page(page_size=(100, 100))
        appearance = pdf.make_stream(b"1 0 0 RG 1 1 8 8 re S")
        appearance.Type, appearance.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        appearance.BBox = pikepdf.Array([0, 0, 10, 10])
        page.obj.Annots = pikepdf.Array(
            [
                pikepdf.Dictionary(
                    Type=pikepdf.Name.Annot,
                    Subtype=pikepdf.Name.Movie,
                    Rect=[0, 0, 10, 10],
                    F=4,
                ),
                pikepdf.Dictionary(
                    Type=pikepdf.Name.Annot,
                    Subtype=pikepdf.Name.Square,
                    Rect=[20, 20, 30, 30],
                    F=2,
                    AP=pikepdf.Dictionary(N=appearance),
                ),
                pikepdf.Dictionary(
                    Type=pikepdf.Name.Annot,
                    Subtype=pikepdf.Name.Link,
                    Rect=[40, 40, 50, 50],
                    F=4,
                    A=pikepdf.Dictionary(
                        S=pikepdf.Name.Launch, F=pikepdf.String("calc.exe")
                    ),
                ),
            ]
        )
        page.obj.Annots.append(
            pikepdf.Dictionary(
                Type=pikepdf.Name.Annot,
                Subtype=pikepdf.Name.Watermark,
                Rect=[60, 60, 90, 90],
                F=4,
            )
        )
        pdf.Root.AcroForm = pikepdf.Dictionary(
            Fields=pikepdf.Array(), XFA=pikepdf.Array()
        )
        pdf.save(self.source)
        with self.assertRaises(tools.pdfa.ConversionError) as caught:
            self.convert_pdfa()
        for category in ("multimedia", "hidden", "actions", "xfa", "annotations"):
            self.assertIn(f"--strip {category}", str(caught.exception))
        self.convert_pdfa(strip="actions,multimedia,hidden,xfa,annotations")
        with pikepdf.Pdf.open(self.output) as result:
            annots = list(result.pages[0].Annots)
            self.assertEqual([a.Subtype for a in annots], [pikepdf.Name.Link])
            self.assertNotIn("/A", annots[0])
            self.assertEqual(int(annots[0].F) & 4, 4)
            self.assertNotIn("/XFA", result.Root.AcroForm)
        self.assert_pdfa("4")

    def test_pdfa4_attachments_select_4f(self):
        pdf = pikepdf.Pdf.new()
        pdf.add_blank_page(page_size=(100, 100))
        pdf.attachments["notes.txt"] = pikepdf.AttachedFileSpec(
            pdf, b"hello", filename="notes.txt"
        )
        pdf.save(self.source)
        self.assertEqual(self.convert_pdfa(), "4f")
        with pikepdf.Pdf.open(self.output) as result:
            self.assertEqual(result.open_metadata()["pdfaid:conformance"], "F")
            spec = result.attachments["notes.txt"].obj
            self.assertEqual(spec.AFRelationship, pikepdf.Name.Unspecified)
            self.assertEqual(str(spec.UF), "notes.txt")
            self.assertEqual(str(spec.EF.F.Subtype), "/text/plain")
        self.assert_pdfa("4f")
        self.assertEqual(self.convert_pdfa(strip="attachments"), "4")
        with pikepdf.Pdf.open(self.output) as result:
            self.assertEqual(len(result.attachments), 0)
            self.assertNotIn("pdfaid:conformance", result.open_metadata())
        self.assert_pdfa("4")

    def test_pdfa4_clones_forms_shared_across_resource_contexts(self):
        pdf = pikepdf.Pdf.new()
        form = pdf.make_stream(b"q 40 0 0 40 0 0 cm /Im Do Q")
        form.Type, form.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        form.BBox = pikepdf.Array([0, 0, 40, 40])
        unused = pdf.make_stream(b"BT /F9 12 Tf 10 10 Td (never drawn) Tj ET")
        unused.Type, unused.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        unused.BBox = pikepdf.Array([0, 0, 40, 40])
        unused.Resources = pikepdf.Dictionary(
            Font=pikepdf.Dictionary(F9=simple_font(pdf, "Verdana-Rare"))
        )
        for color in ((200, 30, 30), (30, 30, 200)):
            page = pdf.add_blank_page(page_size=(100, 100))
            page.obj.Resources = pikepdf.Dictionary(
                XObject=pikepdf.Dictionary(
                    Im=image_object(pdf, Image.new("RGB", (8, 8), color)),
                    Fm=form,
                    Unused=unused,
                )
            )
            page.obj.Contents = pdf.make_stream(b"q 1 0 0 1 10 10 cm /Fm Do Q")
        pdf.save(self.source)
        before = rendered(self.source)
        self.convert_pdfa()
        self.assertEqual(before, rendered(self.output))
        with pikepdf.Pdf.open(self.output) as result:
            forms = [page.Resources.XObject.Fm for page in result.pages]
            self.assertNotEqual(forms[0].objgen, forms[1].objgen)
            for form, page in zip(forms, result.pages):
                self.assertEqual(
                    form.Resources.XObject.Im.objgen, page.Resources.XObject.Im.objgen
                )
        self.assert_pdfa("4")

    def test_pdfa4_keeps_shared_containers_private_per_context(self):
        if not fontconfig_file("Nimbus Sans") and not fontconfig_file(
            "Liberation Sans"
        ):
            self.skipTest("no metric-compatible fonts are installed")
        pdf = pikepdf.Pdf.new()
        # One indirect XObject dictionary shared by both pages holds a form that
        # borrows /F1, which each page binds to a different font.
        form = pdf.make_stream(b"BT /F1 24 Tf 5 5 Td (Ag) Tj ET")
        form.Type, form.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        form.BBox = pikepdf.Array([0, 0, 80, 40])
        xobjects = pdf.make_indirect(pikepdf.Dictionary(Fm=form))
        # One Type 3 font without resources whose glyph draws with the page's /F1.
        glyph = pdf.make_stream(b"100 0 d0 BT /F1 80 Tf 10 10 Td (A) Tj ET")
        type3 = pdf.make_indirect(
            pikepdf.Dictionary(
                Type=pikepdf.Name.Font,
                Subtype=pikepdf.Name.Type3,
                FontBBox=pikepdf.Array([0, 0, 100, 100]),
                FontMatrix=pikepdf.Array([0.01, 0, 0, 0.01, 0, 0]),
                CharProcs=pikepdf.Dictionary(square=glyph),
                Encoding=pikepdf.Dictionary(
                    Type=pikepdf.Name.Encoding,
                    Differences=pikepdf.Array([97, pikepdf.Name.square]),
                ),
                FirstChar=97,
                LastChar=97,
                Widths=pikepdf.Array([100]),
            )
        )
        for base_font in ("Helvetica", "Courier"):
            page = pdf.add_blank_page(page_size=(120, 120))
            page.obj.Resources = pikepdf.Dictionary(
                XObject=xobjects,
                Font=pikepdf.Dictionary(F1=simple_font(pdf, base_font), T3=type3),
            )
            page.obj.Contents = pdf.make_stream(
                b"q 1 0 0 1 10 70 cm /Fm Do Q BT /T3 40 Tf 10 10 Td (a) Tj ET"
            )
        pdf.save(self.source)
        self.convert_pdfa()
        self.assert_rendering_close(self.source, self.output, isolate_pages=True)
        with pikepdf.Pdf.open(self.output) as result:
            forms = [page.Resources.XObject.Fm for page in result.pages]
            self.assertNotEqual(forms[0].objgen, forms[1].objgen)
            fonts = [page.Resources.Font.T3 for page in result.pages]
            self.assertNotEqual(fonts[0].objgen, fonts[1].objgen)
            for page, form, font in zip(result.pages, forms, fonts):
                expected = page.Resources.Font.F1.objgen
                self.assertEqual(form.Resources.Font.F1.objgen, expected)
                self.assertEqual(
                    font.CharProcs.square.Resources.Font.F1.objgen, expected
                )
        self.assert_pdfa("4")

    def test_to_unicode_cleanup_keeps_codespace_and_valid_ranges(self):
        cmap = (
            "1 begincodespacerange\n<0000> <FFFE>\nendcodespacerange\n"
            "3 beginbfchar\n<0041> <0000>\n<0042> <0043>\n<0000> <0041>\nendbfchar\n"
            "3 beginbfrange\n<0050> <0052> <0000>\n<0060> <0060> <FEFF>\n"
            "<0070> <0071> [<0041> <0042>]\n<0080> <0081> [<0000> % old <0042>\n<0043>]\nendbfrange\n"
        )
        cleaned = tools.pdfa.clean_to_unicode(cmap)
        self.assertIn("<0000> <FFFE>\nendcodespacerange", cleaned)
        self.assertNotIn("<0041> <0000>", cleaned)
        self.assertIn("<0042> <0043>", cleaned)
        self.assertIn("<0000> <0041>", cleaned)
        self.assertIn("2 beginbfchar", cleaned)
        self.assertIn("<0051> <0052> <0001>", cleaned)
        self.assertNotIn("<0050>", cleaned)
        self.assertNotIn("<FEFF>", cleaned)
        self.assertIn("<0070> <0071> [<0041> <0042>]", cleaned)
        self.assertNotIn("<0080>", cleaned)
        self.assertIn("<0081> <0081> <0043>", cleaned)
        self.assertIn("3 beginbfrange", cleaned)

    def test_pdfa4_generates_appearances_and_shared_form_resources(self):
        pdf = pikepdf.Pdf.new()
        image = image_object(pdf, Image.new("RGB", (8, 8), (200, 30, 30)))
        form = pdf.make_stream(b"q 40 0 0 40 0 0 cm /Im Do Q")
        form.Type, form.Subtype = pikepdf.Name.XObject, pikepdf.Name.Form
        form.BBox = pikepdf.Array([0, 0, 40, 40])
        page = pdf.add_blank_page(page_size=(100, 100))
        page.obj.Resources = pikepdf.Dictionary(
            XObject=pikepdf.Dictionary(Im=image, Fm=form)
        )
        page.obj.Contents = pdf.make_stream(b"q 1 0 0 1 10 10 cm /Fm Do Q")
        page.obj.Annots = pikepdf.Array(
            [
                pikepdf.Dictionary(
                    Type=pikepdf.Name.Annot,
                    Subtype=pikepdf.Name.Square,
                    Rect=[60, 60, 90, 90],
                    C=pikepdf.Array([0, 0, 1]),
                    F=4,
                )
            ]
        )
        pdf.save(self.source)
        self.convert_pdfa()
        with pikepdf.Pdf.open(self.output) as result:
            page = result.pages[0]
            self.assertIsInstance(page.Annots[0].AP.N, pikepdf.Stream)
            self.assertIn("/Im", page.Resources.XObject.Fm.Resources.XObject)
        self.assert_pdfa("4")

    def test_bitmap_and_mrc(self):
        with pymupdf.open() as doc:
            page = doc.new_page(width=200, height=100)
            page.insert_text((15, 50), "Fine dark text 123", fontsize=12)
            doc.save(self.source)
        for mode in ("bitmap", "mrc"):
            with self.subTest(mode=mode):
                tools.transform(self.source, self.output, options(**{mode: True}))
                with pymupdf.open(self.output) as doc:
                    self.assertEqual(doc[0].get_text(), "")
                    self.assertTrue(
                        any(v < 20 for v in doc[0].get_pixmap(dpi=200).samples)
                    )
                with pikepdf.Pdf.open(self.output) as doc:
                    images = doc.pages[0].Resources.XObject
                    if mode == "mrc":
                        self.assertTrue(images.Foreground.ImageMask)
                        self.assertGreater(
                            int(images.Foreground.Width), int(images.Background.Width)
                        )
                    else:
                        self.assertEqual(len(images), 1)


if __name__ == "__main__":
    unittest.main()
