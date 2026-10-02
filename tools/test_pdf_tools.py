#!/usr/bin/env python3
"""Regression checks for the optional PDF operations; all documents are synthetic."""

import hashlib
import importlib.util
import io
import os
import random
import shutil
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
    }
    result.update(changes)
    return result


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
