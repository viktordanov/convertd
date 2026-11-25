"""Generates the rs-convert test corpus (ledger item 005, plan sections 2 and 3).

Usage (Python 3.13, in a virtualenv made from requirements.txt, run with -I):

    python -I corpus/generate.py corpus/out [--hostile]
        Write the corpus into corpus/out. --hostile adds the hostile files
        (out/hostile/) and the network canaries (out/canary/).
    python -I corpus/generate.py --check
        Generate everything, hostile files included, into a temporary
        directory and compare it with corpus/expected.json: the same ids
        and paths, and the same SHA-256 for every hashed file.
    python -I corpus/generate.py --write-hashes
        The same generation, then write the SHA-256 values and the
        "unhashed" reasons into corpus/expected.json.
    python -I corpus/generate.py --spike-layout DIR
        Write the seven M0 spike files under their old flat names
        (small.docx, long.docx, aptos.docx, deck.pptx, sheet.xlsx,
        encrypted.docx, photos.pptx). spikes/corpus/make_corpus.py calls this.

Every run checks its own output (`self_check`): each file opens in its
library, and the facts the expectations rely on are read back from the
saved bytes. Hostile files are never parsed.

Determinism. Every zip goes through `normalize_zip`. It keeps the entry
order and the part bytes, sets a fixed time stamp and fixed attributes,
and deflates every entry at level 6, as Office does (the ODF mimetype
stays stored). Noise comes from a seeded `random.Random`, not from
`Image.effect_noise`, which uses C `rand()`. JPEG files come from
Pillow's bundled libjpeg-turbo. Only `docx-encrypted` has no hash
(`UNHASHED`). Deflate output depends on the zlib build, so the hashes
hold for the environment recorded under "generator" in expected.json;
--check prints any difference in that environment before it compares.

What the templates write (recorded for plan step 1, on python-docx 1.2.0,
python-pptx 1.0.2 and openpyxl 3.1.5). The generator keeps all of it:

- python-docx `docProps/app.xml`: `<Pages>1</Pages>`,
  `<Application>Microsoft Macintosh Word</Application>`,
  `<AppVersion>14.0000</AppVersion>`, `<Template>Normal.dotm</Template>`
  and zero words. python-docx never rewrites the file.
- python-docx `docProps/core.xml`: creator `python-docx`, created and
  modified `2013-12-23T23:15:00Z`. These are fixed, not the save time.
- python-docx theme: major Latin font Calibri, minor Latin font Cambria.
  `docDefaults` uses `minorHAnsi`, so Cambria. Normal has no `rFonts`.
  Heading 1 to 9 and Title use `majorHAnsi`, so Calibri. MacroText names
  Courier. `fontTable.xml` lists Arial, Calibri, Cambria, Courier,
  Symbol, Times New Roman, MS Gothic and MS Mincho. `font.name = X` adds
  `w:ascii` and `w:hAnsi` but keeps `w:asciiTheme`, which wins in Word,
  so `set_literal_font` drops the theme attributes.
- python-pptx `docProps/app.xml`: `<Slides>0</Slides>`,
  `<HiddenSlides>0</HiddenSlides>`,
  `<Application>Microsoft Macintosh PowerPoint</Application>`, never
  updated. `core.xml` is fixed at 2013-01-27.
- python-pptx theme: major and minor Latin font Calibri. The master uses
  `+mj-lt` and `+mn-lt`. The layouts name no font.
- openpyxl: `xl/styles.xml` has one font, Calibri with `scheme="minor"`.
  The theme has major Cambria and minor Calibri. `core.xml` carries the
  creation time unless `properties.created` is set, and always the save
  time as `dcterms:modified`, so `workbook_bytes` rewrites that value.
- All three libraries stamp zip entries with the current time.
"""

import argparse
import datetime
import hashlib
import html
import io
import json
import platform
import random
import re
import struct
import sys
import tempfile
import xml.etree.ElementTree as ElementTree
import zipfile
import zlib
from pathlib import Path

import docx
import msoffcrypto
import openpyxl
from docx.opc.constants import RELATIONSHIP_TYPE as RT
from docx.oxml import parse_xml
from docx.oxml.ns import nsdecls, qn
from docx.shared import Pt
from PIL import Image, ImageFilter
from pptx import Presentation
from pptx.util import Inches

CORPUS_DIR = Path(__file__).resolve().parent
EXPECTED = CORPUS_DIR / "expected.json"
LISTENER = "127.0.0.1:47123"
ZIP_TIME = (2024, 1, 1, 0, 0, 0)
FIXED_DATETIME = datetime.datetime(2024, 1, 1, 0, 0, 0)
LOREM = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor "
         "incididunt ut labore et dolore magna aliqua. ") * 6
ZIP_BOMB_BYTES = 2 << 30
HOSTILE_MAX_FILE = 5 << 20

ZIP_LEVEL = 6
UNHASHED = {"docx-encrypted": "msoffcrypto draws a random salt and key on every run"}
# Size bounds that keep the corpus close to the plan (the spike's small.docx is 36 KB).
SIZE_BOUNDS = {"docx-small": (20_000, 100_000), "pptx-photos60": (25 << 20, 45 << 20)}


# --- zip helpers ----------------------------------------------------------------

def zip_info(name, deflate=True, level=ZIP_LEVEL):
    info = zipfile.ZipInfo(name, ZIP_TIME)
    info.compress_type = zipfile.ZIP_DEFLATED if deflate else zipfile.ZIP_STORED
    info.compress_level = level if deflate else None
    info.create_system = 0
    info.external_attr = 0
    return info


def normalize_zip(data, edits=None, additions=None):
    """Rewrites a zip with fixed metadata, deflating every entry. `edits` maps a part name to a
    function from its bytes to new bytes; every edit must find its part. `additions` maps new
    part names to their bytes, written after the existing parts."""
    edits = dict(edits or {})
    src = zipfile.ZipFile(io.BytesIO(data))
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as dst:
        for info in src.infolist():
            body = src.read(info.filename)
            edit = edits.pop(info.filename, None)
            if edit is not None:
                body = edit(body)
            dst.writestr(zip_info(info.filename), body)
        for name, body in (additions or {}).items():
            if name in src.namelist():
                raise KeyError(f"failed to add {name}: the part exists")
            dst.writestr(zip_info(name), body)
    if edits:
        raise KeyError(f"failed to edit missing parts {sorted(edits)}")
    return out.getvalue()


def sub_once(pattern, repl):
    """An edit that replaces exactly one regex match in a UTF-8 part."""
    def edit(body):
        text, count = re.subn(pattern, repl, body.decode("utf-8"), count=1, flags=re.S)
        if count != 1:
            raise ValueError(f"failed to find {pattern!r}")
        return text.encode("utf-8")
    return edit


def replace_with(new_body):
    return lambda _body: new_body


def theme_latin(slot, family):
    """An edit that sets the Latin typeface of a:majorFont or a:minorFont in a theme part."""
    return sub_once(rf'(<a:{slot}>\s*<a:latin typeface=")[^"]*(")', rf"\g<1>{family}\g<2>")


def chain(*edits):
    def edit(body):
        for one in edits:
            body = one(body)
        return body
    return edit


# --- DOCX -----------------------------------------------------------------------

def docx_bytes(doc, edits=None):
    buf = io.BytesIO()
    doc.save(buf)
    return normalize_zip(buf.getvalue(), edits)


def spike_document(paragraphs, font="Calibri"):
    """The spike's make_docx: Normal in a literal font, a heading every 12 paragraphs, a table."""
    d = docx.Document()
    style = d.styles["Normal"]
    style.font.name = font
    style.font.size = Pt(11)
    for i in range(paragraphs):
        if i % 12 == 0:
            d.add_heading(f"Section {i // 12 + 1}", level=1)
        d.add_paragraph(LOREM)
    table = d.add_table(rows=6, cols=4)
    for r, row in enumerate(table.rows):
        for c, cell in enumerate(row.cells):
            cell.text = f"r{r}c{c}"
    return d


def set_literal_font(style, family):
    """Sets w:ascii and w:hAnsi and drops the theme attributes, as Word does."""
    style.font.name = family
    rfonts = style.element.rPr.rFonts
    for attr in ("w:asciiTheme", "w:hAnsiTheme"):
        rfonts.attrib.pop(qn(attr), None)


def docx_small(_ctx):
    return docx_bytes(spike_document(4))


def docx_long(_ctx):
    return docx_bytes(spike_document(160))


def docx_breaks12(_ctx):
    d = docx.Document()
    for page in range(1, 13):
        d.add_paragraph(f"Page {page}. {LOREM[:200]}")
        if page < 12:
            d.add_page_break()
    return docx_bytes(d)


def docx_empty_page(_ctx):
    d = docx.Document()
    d.add_paragraph(LOREM)
    d.add_paragraph(LOREM)
    d.add_page_break()  # page 2 holds only the end of this paragraph
    return docx_bytes(d)


def docx_aptos_theme(_ctx):
    d = docx.Document()
    for _ in range(4):
        run = d.add_paragraph().add_run(LOREM)
        rfonts = run._r.get_or_add_rPr().get_or_add_rFonts()
        rfonts.set(qn("w:asciiTheme"), "minorHAnsi")
        rfonts.set(qn("w:hAnsiTheme"), "minorHAnsi")
    return docx_bytes(d, {"word/theme/theme1.xml": theme_latin("minorFont", "Aptos")})


def docx_headings(_ctx):
    d = docx.Document()
    set_literal_font(d.styles["Normal"], "Calibri")
    set_literal_font(d.styles["Heading 2"], "Calibri Light")
    d.add_heading("Heading one in the theme major font", level=1)
    d.add_paragraph(LOREM)
    d.add_heading("Heading two in Calibri Light", level=2)
    d.add_paragraph(LOREM)
    return docx_bytes(d, {"word/theme/theme1.xml": theme_latin("majorFont", "Aptos Display")})


def encrypt(plain, password="secret"):
    office = msoffcrypto.OfficeFile(io.BytesIO(plain))
    out = io.BytesIO()
    office.encrypt(password, out)
    return out.getvalue()


def docx_encrypted(ctx):
    return encrypt(ctx["docx-small"])


def docm_novba(_ctx):
    d = docx.Document()
    d.add_paragraph("A macro-enabled document with no VBA project.")
    content_type = sub_once(
        r"application/vnd\.openxmlformats-officedocument\.wordprocessingml\.document\.main\+xml",
        "application/vnd.ms-word.document.macroEnabled.main+xml")
    return docx_bytes(d, {"[Content_Types].xml": content_type})


def docx_remote_image_canary(_ctx):
    d = docx.Document()
    d.add_paragraph("The picture below is linked to the canary listener, not embedded.")
    rid = d.part.relate_to(f"http://{LISTENER}/docx-image.png", RT.IMAGE, is_external=True)
    drawing = parse_xml(
        f'<w:r {nsdecls("w", "wp", "a", "pic", "r")}><w:drawing>'
        '<wp:inline distT="0" distB="0" distL="0" distR="0">'
        '<wp:extent cx="914400" cy="914400"/><wp:docPr id="1" name="Remote picture"/>'
        '<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">'
        '<pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="docx-image.png"/><pic:cNvPicPr/></pic:nvPicPr>'
        f'<pic:blipFill><a:blip r:link="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>'
        '<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>'
        '</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>')
    d.add_paragraph()._p.append(drawing)
    return docx_bytes(d)


def spike_aptos(_ctx):
    """The spike's aptos.docx: Aptos as a literal Normal font (real files take it from the theme)."""
    return docx_bytes(spike_document(4, font="Aptos"))


# --- PPTX -----------------------------------------------------------------------

def pptx_bytes(pres, edits=None):
    buf = io.BytesIO()
    pres.save(buf)
    return normalize_zip(buf.getvalue(), edits)


def text_slide(pres, title):
    slide = pres.slides.add_slide(pres.slide_layouts[1])
    slide.shapes.title.text = title
    slide.placeholders[1].text = "First point\nSecond point\nThird point"
    return slide


def pptx_deck20(_ctx):
    p = Presentation()
    for i in range(20):
        s = text_slide(p, f"Slide {i + 1}")
        s.shapes.add_shape(1, Inches(6), Inches(5), Inches(2), Inches(1))
    return pptx_bytes(p)


HIDDEN_SLIDES = (1, 3)


def pptx_hidden(_ctx):
    p = Presentation()
    for i in range(5):
        s = text_slide(p, f"Slide {i + 1}" + (" (hidden)" if i in HIDDEN_SLIDES else ""))
        if i in HIDDEN_SLIDES:
            s._element.set("show", "0")
    return pptx_bytes(p)


def pptx_pages_mismatch(_ctx):
    p = Presentation()
    for i in range(4):
        text_slide(p, f"Slide {i + 1}")
    not_a_slide = ("<?xml version='1.0' encoding='UTF-8' standalone='yes'?>\n"
                   '<p:notASlide xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>')
    return pptx_bytes(p, {"ppt/slides/slide4.xml": replace_with(not_a_slide.encode())})


UNREACHED_LAYOUT = 10


def pptx_aptos_theme(_ctx):
    p = Presentation()
    for i in range(3):
        text_slide(p, f"Slide {i + 1}")
    frame = p.slide_layouts[UNREACHED_LAYOUT].placeholders[0].text_frame
    frame.text = "A layout that no slide uses"
    frame.paragraphs[0].runs[0].font.name = "Zzyzx Grotesk"
    theme = chain(theme_latin("majorFont", "Aptos"), theme_latin("minorFont", "Aptos"))
    return pptx_bytes(p, {"ppt/theme/theme1.xml": theme})


def photo_jpeg(index, width, height, seed):
    """A smooth gradient with seeded noise: the hard case for PNG and lossless WebP."""
    rng = random.Random(seed * 1000 + index)
    img = Image.radial_gradient("L").resize((width, height)).convert("RGB")
    noise = Image.frombytes("L", (width, height), rng.randbytes(width * height)).convert("RGB")
    img = Image.blend(img, noise, 0.35).filter(ImageFilter.GaussianBlur(1))
    r, g, b = img.split()
    img = Image.merge("RGB", (r, g.point(lambda v: (v * 3 + index * 20) % 256),
                              b.point(lambda v: 255 - v)))
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=90)
    buf.seek(0)
    return buf


def photo_deck(count, width, height, seed):
    p = Presentation()
    p.slide_width, p.slide_height = Inches(13.333), Inches(7.5)
    for i in range(count):
        slide = p.slides.add_slide(p.slide_layouts[6])
        slide.shapes.add_picture(photo_jpeg(i, width, height, seed), 0, 0, p.slide_width, p.slide_height)
    return pptx_bytes(p)


def pptx_photos10(_ctx):
    return photo_deck(10, 1600, 900, seed=7)


def pptx_photos60(_ctx):
    return photo_deck(60, 1366, 768, seed=60)


def potx(_ctx):
    content_type = sub_once(
        r"application/vnd\.openxmlformats-officedocument\.presentationml\.presentation\.main\+xml",
        "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml")
    return pptx_bytes(Presentation(), {"[Content_Types].xml": content_type})


# --- XLSX -----------------------------------------------------------------------

def workbook_bytes(wb):
    wb.properties.created = FIXED_DATETIME
    buf = io.BytesIO()
    wb.save(buf)
    # openpyxl writes the save time as dcterms:modified, whatever properties.modified says.
    modified = sub_once(r"(<dcterms:modified [^>]*>)[^<]*(</dcterms:modified>)",
                        r"\g<1>2024-01-01T00:00:00Z\g<2>")
    return normalize_zip(buf.getvalue(), {"docProps/core.xml": modified})


def xlsx_sheet3(_ctx):
    wb = openpyxl.Workbook()
    for n in range(3):
        ws = wb.active if n == 0 else wb.create_sheet()
        ws.title = f"Sheet{n + 1}"
        for r in range(1, 201):
            for c in range(1, 11):
                ws.cell(row=r, column=c, value=r * c)
    return workbook_bytes(wb)


def xlsx_100k(_ctx):
    wb = openpyxl.Workbook(write_only=True)
    ws = wb.create_sheet("Data")
    for r in range(100_000):
        ws.append([r * 10 + c for c in range(10)])
    return workbook_bytes(wb)


def xlsx_webservice_canary(_ctx):
    wb = openpyxl.Workbook()
    ws = wb.active
    ws["A1"] = "Fetched from the canary listener:"
    # Excel stores functions newer than 2007 with the _xlfn. prefix.
    ws["A2"] = f'=_xlfn.WEBSERVICE("http://{LISTENER}/ws")'
    return workbook_bytes(wb)


# --- ODF ------------------------------------------------------------------------

ODF_NS = {
    "office": "urn:oasis:names:tc:opendocument:xmlns:office:1.0",
    "style": "urn:oasis:names:tc:opendocument:xmlns:style:1.0",
    "text": "urn:oasis:names:tc:opendocument:xmlns:text:1.0",
    "table": "urn:oasis:names:tc:opendocument:xmlns:table:1.0",
    "draw": "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0",
    "presentation": "urn:oasis:names:tc:opendocument:xmlns:presentation:1.0",
    "fo": "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0",
    "svg": "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0",
    "script": "urn:oasis:names:tc:opendocument:xmlns:script:1.0",
    "xlink": "http://www.w3.org/1999/xlink",
    "meta": "urn:oasis:names:tc:opendocument:xmlns:meta:1.0",
}
XML_DECL = '<?xml version="1.0" encoding="UTF-8"?>\n'


def odf_ns():
    return " ".join(f'xmlns:{k}="{v}"' for k, v in ODF_NS.items())


def font_decls(family, generic):
    # LibreOffice quotes a family name that holds a space.
    return ("<office:font-face-decls>"
            f'<style:font-face style:name="{family}" svg:font-family="&apos;{family}&apos;" '
            f'style:font-family-generic="{generic}" style:font-pitch="variable"/>'
            "</office:font-face-decls>")


def odf_bytes(mimetype, content, styles=None, extra=None):
    """An ODF package: mimetype first and stored, then the manifest and the parts."""
    parts = {"content.xml": content,
             "styles.xml": styles or f'{XML_DECL}<office:document-styles {odf_ns()} office:version="1.3"/>',
             "meta.xml": (f'{XML_DECL}<office:document-meta {odf_ns()} office:version="1.3"><office:meta>'
                          "<meta:generator>rs-convert corpus/generate.py</meta:generator>"
                          "</office:meta></office:document-meta>")}
    parts.update(extra or {})
    entries = [f'<manifest:file-entry manifest:full-path="/" manifest:version="1.3" '
               f'manifest:media-type="{mimetype}"/>']
    entries += [f'<manifest:file-entry manifest:full-path="{name}" manifest:media-type="text/xml"/>'
                for name in parts]
    manifest = (f'{XML_DECL}<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:'
                f'manifest:1.0" manifest:version="1.3">{"".join(entries)}</manifest:manifest>')
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        z.writestr(zip_info("mimetype", deflate=False), mimetype)
        z.writestr(zip_info("META-INF/manifest.xml"), manifest)
        for name, body in parts.items():
            z.writestr(zip_info(name), body)
    return out.getvalue()


def odt_content(paragraphs, scripts=""):
    """paragraphs: (text, page_break_before) pairs."""
    body = "".join(f'<text:p text:style-name="{"P2" if brk else "P1"}">{html.escape(t)}</text:p>'
                   for t, brk in paragraphs)
    return (f'{XML_DECL}<office:document-content {odf_ns()} office:version="1.3">{scripts}'
            f'{font_decls("Liberation Serif", "roman")}<office:automatic-styles>'
            '<style:style style:name="P1" style:family="paragraph"><style:text-properties '
            'style:font-name="Liberation Serif" fo:font-size="12pt"/></style:style>'
            '<style:style style:name="P2" style:family="paragraph"><style:paragraph-properties '
            'fo:break-before="page"/><style:text-properties style:font-name="Liberation Serif" '
            'fo:font-size="12pt"/></style:style></office:automatic-styles>'
            f"<office:body><office:text>{body}</office:text></office:body></office:document-content>")


def odt(_ctx):
    return odf_bytes("application/vnd.oasis.opendocument.text",
                     odt_content([(LOREM, False), (LOREM, True)]))


BASIC_MACRO = f"""Sub Main
  On Error Resume Next
  Dim access As Object
  access = createUnoService("com.sun.star.ucb.SimpleFileAccess")
  access.openFileRead("http://{LISTENER}/odt-basic")
End Sub
"""


def odt_basic_canary(_ctx):
    scripts = ("<office:scripts><office:event-listeners>"
               '<script:event-listener script:language="ooo:script" script:event-name="dom:load" '
               'xlink:href="vnd.sun.star.script:Standard.Module1.Main?language=Basic&amp;location=document" '
               'xlink:type="simple"/></office:event-listeners></office:scripts>')
    dtd = '<!DOCTYPE {0} PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "{1}">\n'
    basic = {
        "Basic/script-lc.xml": (
            XML_DECL + dtd.format("library:libraries", "libraries.dtd")
            + '<library:libraries xmlns:library="http://openoffice.org/2000/library" '
              'xmlns:xlink="http://www.w3.org/1999/xlink">'
              '<library:library library:name="Standard" library:link="false"/></library:libraries>'),
        "Basic/Standard/script-lb.xml": (
            XML_DECL + dtd.format("library:library", "library.dtd")
            + '<library:library xmlns:library="http://openoffice.org/2000/library" '
              'library:name="Standard" library:readonly="false" library:passwordprotected="false">'
              '<library:element library:name="Module1"/></library:library>'),
        "Basic/Standard/Module1.xml": (
            XML_DECL + dtd.format("script:module", "module.dtd")
            + '<script:module xmlns:script="http://openoffice.org/2000/script" script:name="Module1" '
              f'script:language="StarBasic" script:moduleType="normal">{html.escape(BASIC_MACRO)}'
              "</script:module>"),
    }
    content = odt_content([("A document with a Basic macro bound to the load event.", False)], scripts)
    return odf_bytes("application/vnd.oasis.opendocument.text", content, extra=basic)


def odp(_ctx):
    pages = "".join(
        f'<draw:page draw:name="Slide {n}" draw:master-page-name="Default">'
        '<draw:frame svg:x="2cm" svg:y="2cm" svg:width="24cm" svg:height="3cm"><draw:text-box>'
        f'<text:p><text:span text:style-name="T1">Slide {n}</text:span></text:p>'
        "</draw:text-box></draw:frame></draw:page>" for n in range(1, 4))
    content = (f'{XML_DECL}<office:document-content {odf_ns()} office:version="1.3">'
               f'{font_decls("Liberation Sans", "swiss")}<office:automatic-styles>'
               '<style:style style:name="T1" style:family="text"><style:text-properties '
               'style:font-name="Liberation Sans" fo:font-size="32pt"/></style:style>'
               "</office:automatic-styles><office:body><office:presentation>"
               f"{pages}</office:presentation></office:body></office:document-content>")
    styles = (f'{XML_DECL}<office:document-styles {odf_ns()} office:version="1.3">'
              '<office:automatic-styles><style:page-layout style:name="PM1">'
              '<style:page-layout-properties fo:margin-top="0cm" fo:margin-bottom="0cm" '
              'fo:margin-left="0cm" fo:margin-right="0cm" fo:page-width="28cm" fo:page-height="15.75cm" '
              'style:print-orientation="landscape"/></style:page-layout></office:automatic-styles>'
              '<office:master-styles><style:master-page style:name="Default" '
              'style:page-layout-name="PM1"/></office:master-styles></office:document-styles>')
    return odf_bytes("application/vnd.oasis.opendocument.presentation", content, styles)


def ods(_ctx):
    rows = "".join(
        "<table:table-row>" + "".join(
            f'<table:table-cell office:value-type="float" office:value="{r * c}">'
            f"<text:p>{r * c}</text:p></table:table-cell>" for c in range(1, 4))
        + "</table:table-row>" for r in range(1, 21))
    content = (f'{XML_DECL}<office:document-content {odf_ns()} office:version="1.3">'
               f'{font_decls("Liberation Sans", "swiss")}<office:body><office:spreadsheet>'
               '<table:table table:name="Sheet1"><table:table-column table:number-columns-repeated="3"/>'
               f"{rows}</table:table></office:spreadsheet></office:body></office:document-content>")
    return odf_bytes("application/vnd.oasis.opendocument.spreadsheet", content)


# --- RTF, PDF, images -------------------------------------------------------------

def rtf(_ctx):
    return (r"{\rtf1\ansi\deff0{\fonttbl{\f0\froman Times New Roman;}}\f0\fs24 "
            r"Page one of an RTF file.\par\page Page two of an RTF file.\par}" "\n").encode("ascii")


def pdf_bytes(contents, media=(595, 842)):
    """An A4 PDF with one page per content stream, Helvetica as /F1, and an exact xref."""
    n = len(contents)
    page_ids = [4 + 2 * i for i in range(n)]
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [" + b" ".join(b"%d 0 R" % i for i in page_ids) + b"] /Count %d >>" % n,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    for i, stream in enumerate(contents):
        objects.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 %d %d] "
                       b"/Resources << /Font << /F1 3 0 R >> >> /Contents %d 0 R >>"
                       % (media[0], media[1], page_ids[i] + 1))
        objects.append(b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream")
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % number + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    out += b"".join(b"%010d 00000 n \n" % off for off in offsets)
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objects) + 1, xref)
    return bytes(out)


def pdf_text3(_ctx):
    return pdf_bytes([b"BT /F1 24 Tf 72 760 Td (Page %d of 3) Tj ET\n"
                      b"BT /F1 11 Tf 72 720 Td (A hand-written PDF page with one line of text.) Tj ET"
                      % page for page in range(1, 4)])


def pdf_white_rect(_ctx):
    return pdf_bytes([b"1 1 1 rg 72 72 451 698 re f"])


def pdf_tiny_text(_ctx):
    return pdf_bytes([b"BT /F1 6 Tf 72 770 Td (tiny) Tj ET"])


def png_chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
IMAGE_SIZE = (640, 480)


def gradient_rows():
    w, h = IMAGE_SIZE
    return [bytes(v for x in range(w) for v in (x * 255 // w, y * 255 // h, 128)) for y in range(h)]


def png(_ctx):
    w, h = IMAGE_SIZE
    raw = b"".join(b"\x00" + row for row in gradient_rows())
    return (PNG_SIGNATURE + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + png_chunk(b"IDAT", zlib.compress(raw, 9)) + png_chunk(b"IEND", b""))


def jpeg(_ctx):
    img = Image.frombytes("RGB", IMAGE_SIZE, b"".join(gradient_rows()))
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=85)
    return buf.getvalue()


# --- hostile files ----------------------------------------------------------------

DOCX_CONTENT_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/>'
    '<Override PartName="/word/document.xml" '
    'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
    "</Types>")
DOCX_RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" '
    'Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" '
    'Target="word/document.xml"/></Relationships>')


def zip_ratio(_ctx):
    """A docx whose word/document.xml is 2 GiB of zero bytes, deflated to about 2 MB."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as z:
        z.writestr(zip_info("[Content_Types].xml"), DOCX_CONTENT_TYPES)
        z.writestr(zip_info("_rels/.rels"), DOCX_RELS)
        info = zip_info("word/document.xml", level=9)
        chunk = bytes(1 << 20)
        with z.open(info, "w", force_zip64=True) as entry:
            for _ in range(ZIP_BOMB_BYTES // len(chunk)):
                entry.write(chunk)
    return out.getvalue()


def xml_entities(ctx):
    """docx-small with a billion-laughs DTD in word/document.xml."""
    entities = ['<!ENTITY lol0 "lol">'] + [
        f'<!ENTITY lol{n} "{f"&lol{n - 1};" * 10}">' for n in range(1, 10)]
    document = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
                "<!DOCTYPE w:document [\n" + "\n".join(entities) + "\n]>\n"
                f'<w:document {nsdecls("w")}><w:body><w:p><w:r><w:t>&lol9;</w:t></w:r></w:p>'
                "<w:sectPr/></w:body></w:document>")
    return normalize_zip(ctx["docx-small"], {"word/document.xml": replace_with(document.encode())})


PNG_BOMB_SIDE = 100_000


def png_bomb(_ctx):
    """A complete 1-bit grayscale PNG of 100,000 x 100,000 pixels, about 1.2 MB on disk."""
    rows = bytes(1 + PNG_BOMB_SIDE // 8) * 1000
    deflater = zlib.compressobj(9)
    idat = bytearray()
    for _ in range(PNG_BOMB_SIDE // 1000):
        idat += deflater.compress(rows)
    idat += deflater.flush()
    ihdr = struct.pack(">IIBBBBB", PNG_BOMB_SIDE, PNG_BOMB_SIDE, 1, 0, 0, 0, 0)
    return (PNG_SIGNATURE + png_chunk(b"IHDR", ihdr) + png_chunk(b"IDAT", bytes(idat))
            + png_chunk(b"IEND", b""))


def truncated(ctx):
    data = ctx["docx-long"]
    return data[: len(data) // 2]


def html_as_doc(_ctx):
    return (b'<!DOCTYPE html>\n<html><head><meta charset="utf-8"><title>Not a Word file</title>'
            b"</head><body><p>This is HTML saved under a .doc name.</p></body></html>\n")


# --- VBA projects from the committed fixtures --------------------------------------

FIXTURES_DIR = CORPUS_DIR / "fixtures"
VBA_CONTENT_TYPE = "application/vnd.ms-office.vbaProject"
VBA_RELATIONSHIP = "http://schemas.microsoft.com/office/2006/relationships/vbaProject"
# host: (fixture file, part, the main part's rels, plain main content type, macro-enabled one)
VBA_HOSTS = {
    "word": ("vbaProject-word.bin", "word/vbaProject.bin", "word/_rels/document.xml.rels",
             "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
             "application/vnd.ms-word.document.macroEnabled.main+xml"),
    "xl": ("vbaProject-xl.bin", "xl/vbaProject.bin", "xl/_rels/workbook.xml.rels",
           "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
           "application/vnd.ms-excel.sheet.macroEnabled.main+xml"),
    "ppt": ("vbaProject-ppt.bin", "ppt/vbaProject.bin", "ppt/_rels/presentation.xml.rels",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
            "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml"),
}


def fixture(name):
    """A committed fixture, refused unless its SHA-256 matches its expected.json entry."""
    path = FIXTURES_DIR / name
    entries = {f["path"]: f for f in json.loads(EXPECTED.read_text(encoding="utf-8"))["files"]}
    entry = entries.get(f"fixtures/{name}")
    if entry is None or entry.get("origin") != "fixture":
        raise SystemExit(f"failed to use fixture {name}: expected.json does not list it")
    data = path.read_bytes()
    if sha256(data) != entry.get("sha256"):
        raise SystemExit(f"failed to use fixture {name}: sha256 {sha256(data)}, "
                         f"expected.json says {entry.get('sha256')}")
    return data


def with_vba(package, host):
    """Makes an OOXML package macro-enabled and adds the host's VBA project from the fixtures.
    The part gets an Override, because the pptx template already maps .bin to printer settings."""
    name, part, rels, plain_type, macro_type = VBA_HOSTS[host]
    content_types = chain(
        sub_once(re.escape(plain_type), macro_type),
        sub_once(r"</Types>", f'<Override PartName="/{part}" ContentType="{VBA_CONTENT_TYPE}"/></Types>'))
    relationship = sub_once(
        r"</Relationships>",
        f'<Relationship Id="rIdVbaProject" Type="{VBA_RELATIONSHIP}" Target="vbaProject.bin"/>'
        "</Relationships>")
    return normalize_zip(package, {"[Content_Types].xml": content_types, rels: relationship},
                         additions={part: fixture(name)})


def saved(document):
    buf = io.BytesIO()
    document.save(buf)
    return buf.getvalue()


def docm_vba(_ctx):
    d = docx.Document()
    d.add_paragraph("A macro-enabled document with a VBA project. The macro must not run.")
    return with_vba(saved(d), "word")


def xlsm_vba(_ctx):
    wb = openpyxl.Workbook()
    wb.active["A1"] = "A macro-enabled workbook with a VBA project. The macro must not run."
    return with_vba(workbook_bytes(wb), "xl")


def pptm_vba(_ctx):
    p = Presentation()
    text_slide(p, "A macro-enabled deck with a VBA project")
    return with_vba(saved(p), "ppt")


# --- registry -------------------------------------------------------------------

# (id, path under out/, builder, hostile). A builder may read an earlier entry's bytes.
ENTRIES = [
    ("docx-small", "docx/small.docx", docx_small, False),
    ("docx-long", "docx/long.docx", docx_long, False),
    ("docx-breaks12", "docx/breaks12.docx", docx_breaks12, False),
    ("docx-empty-page", "docx/empty-page.docx", docx_empty_page, False),
    ("docx-aptos-theme", "docx/aptos-theme.docx", docx_aptos_theme, False),
    ("docx-headings", "docx/headings.docx", docx_headings, False),
    ("docx-encrypted", "docx/encrypted.docx", docx_encrypted, False),
    ("docm-novba", "docx/novba.docm", docm_novba, False),
    ("docm-vba", "docx/vba.docm", docm_vba, False),
    ("xlsm-vba", "xlsx/vba.xlsm", xlsm_vba, False),
    ("pptm-vba", "pptx/vba.pptm", pptm_vba, False),
    ("rtf", "rtf/two-pages.rtf", rtf, False),
    ("odt", "odf/two-pages.odt", odt, False),
    ("pptx-deck20", "pptx/deck20.pptx", pptx_deck20, False),
    ("pptx-hidden", "pptx/hidden.pptx", pptx_hidden, False),
    ("pptx-pages-mismatch", "pptx/pages-mismatch.pptx", pptx_pages_mismatch, False),
    ("pptx-aptos-theme", "pptx/aptos-theme.pptx", pptx_aptos_theme, False),
    ("pptx-photos10", "pptx/photos10.pptx", pptx_photos10, False),
    ("pptx-photos60", "pptx/photos60.pptx", pptx_photos60, False),
    ("potx", "pptx/empty.potx", potx, False),
    ("odp", "odf/three-slides.odp", odp, False),
    ("xlsx-sheet3", "xlsx/sheet3.xlsx", xlsx_sheet3, False),
    ("xlsx-100k", "xlsx/100k.xlsx", xlsx_100k, False),
    ("ods", "odf/one-sheet.ods", ods, False),
    ("pdf-text3", "pdf/text3.pdf", pdf_text3, False),
    ("pdf-white-rect", "pdf/white-rect.pdf", pdf_white_rect, False),
    ("pdf-tiny-text", "pdf/tiny-text.pdf", pdf_tiny_text, False),
    ("png", "image/gradient.png", png, False),
    ("jpeg", "image/gradient.jpg", jpeg, False),
    ("odt-basic-canary", "canary/odt-basic.odt", odt_basic_canary, True),
    ("docx-remote-image-canary", "canary/remote-image.docx", docx_remote_image_canary, True),
    ("xlsx-webservice-canary", "canary/webservice.xlsx", xlsx_webservice_canary, True),
    ("zip-ratio", "hostile/zip-ratio.docx", zip_ratio, True),
    ("xml-entities", "hostile/xml-entities.docx", xml_entities, True),
    ("png-bomb", "hostile/png-bomb.png", png_bomb, True),
    ("truncated", "hostile/truncated.docx", truncated, True),
    ("html-as-doc", "hostile/html-as.doc", html_as_doc, True),
]
BUILDERS = {entry_id: builder for entry_id, _, builder, _ in ENTRIES}
NEVER_PARSED = {"zip-ratio", "xml-entities", "png-bomb", "truncated", "html-as-doc"}
SPIKE_LAYOUT = [
    ("small.docx", "docx-small"), ("long.docx", "docx-long"), ("aptos.docx", None),
    ("deck.pptx", "pptx-deck20"), ("sheet.xlsx", "xlsx-sheet3"),
    ("encrypted.docx", "docx-encrypted"), ("photos.pptx", "pptx-photos10"),
]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def generate(out, hostile):
    """Writes the corpus into `out`, checks it, and returns {id: (path under out, bytes)}."""
    built = {}
    ctx = {}
    for entry_id, rel, builder, is_hostile in ENTRIES:
        if is_hostile and not hostile:
            continue
        data = builder(ctx)
        ctx[entry_id] = data
        path = out / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        built[entry_id] = (rel, data)
        digest = "unhashed" if entry_id in UNHASHED else sha256(data)
        print(f"{entry_id:26} {len(data):>11} {digest}  {rel}")
    for entry_id, (rel, data) in built.items():
        try:
            check_file(entry_id, out / rel, data)
        except Exception as err:  # noqa: BLE001 - any library error fails the check
            raise SystemExit(f"self-check failed for {entry_id} ({rel}): {err!r}") from None
    return built


# --- self check -------------------------------------------------------------------

class CheckError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CheckError(message)


def members(data):
    return zipfile.ZipFile(io.BytesIO(data))


ZIP_SUFFIXES = (".docx", ".docm", ".pptx", ".pptm", ".potx", ".xlsx", ".xlsm", ".odt", ".odp",
                ".ods")
MAIN_TYPES = {
    ".docm": "application/vnd.ms-word.document.macroEnabled.main+xml",
    ".xlsm": "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
    ".pptm": "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
    ".potx": "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
}
VBA_ENTRIES = {"docm-vba": "word", "xlsm-vba": "xl", "pptm-vba": "ppt"}


def check_file(entry_id, path, data):
    """Opens one generated file in its library and reads back the facts expected.json uses."""
    if entry_id in NEVER_PARSED:
        check_hostile(entry_id, data)
        return
    suffix = path.suffix
    low, high = SIZE_BOUNDS.get(entry_id, (1, 1 << 30))
    require(low <= len(data) <= high, f"{len(data)} bytes, outside {low}..{high}")
    if entry_id not in UNHASHED and suffix in ZIP_SUFFIXES:
        for info in members(data).infolist():
            method = zipfile.ZIP_STORED if info.filename == "mimetype" else zipfile.ZIP_DEFLATED
            require(info.date_time == ZIP_TIME and info.compress_type == method,
                    f"{info.filename} is not normalized")
    if entry_id == "docx-encrypted":
        office = msoffcrypto.OfficeFile(io.BytesIO(data))
        office.load_key(password="secret")
        plain = io.BytesIO()
        office.decrypt(plain)
        docx.Document(plain)
    elif suffix == ".docx":
        docx.Document(str(path))
    elif suffix in MAIN_TYPES:
        # The Python libraries refuse these main content types, so check the type itself.
        types = members(data).read("[Content_Types].xml").decode()
        require(MAIN_TYPES[suffix] in types, "main content type not set")
    elif suffix == ".pptx":
        pres = Presentation(str(path))
        if entry_id != "pptx-pages-mismatch":  # slide 4 is not a slide by design
            require(all(len(s.shapes) > 0 for s in pres.slides), "empty slide")
    elif suffix == ".xlsx" and entry_id == "xlsx-100k":
        require("xl/worksheets/sheet1.xml" in members(data).namelist(), "no sheet")
    elif suffix == ".xlsx":
        openpyxl.load_workbook(str(path), read_only=True).close()
    elif suffix in (".odt", ".odp", ".ods"):
        z = members(data)
        first = z.infolist()[0]
        require(first.filename == "mimetype" and first.compress_type == zipfile.ZIP_STORED,
                "mimetype is not the first, stored entry")
        for name in z.namelist():
            if name.endswith(".xml") and not name.startswith("Basic/"):  # Basic parts carry a DTD
                ElementTree.fromstring(z.read(name))  # our own output, not untrusted input
    elif suffix == ".pdf":
        require(data.startswith(b"%PDF-") and data.rstrip().endswith(b"%%EOF"), "not a PDF")
    elif suffix in (".png", ".jpg"):
        with Image.open(io.BytesIO(data)) as img:
            img.load()
            require(img.size == IMAGE_SIZE, f"size {img.size}")
    check_facts(entry_id, data)


def check_hostile(entry_id, data):
    """Size and header checks only: these files are never decompressed or parsed."""
    require(0 < len(data) < HOSTILE_MAX_FILE, f"{len(data)} bytes, the limit is {HOSTILE_MAX_FILE}")
    if entry_id == "zip-ratio":
        info = members(data).getinfo("word/document.xml")  # reads the central directory only
        require(info.file_size == ZIP_BOMB_BYTES, f"declared size {info.file_size}")
    elif entry_id == "png-bomb":
        require(data.startswith(PNG_SIGNATURE) and data[12:16] == b"IHDR", "not a PNG")
        require(struct.unpack(">II", data[16:24]) == (PNG_BOMB_SIDE, PNG_BOMB_SIDE), "IHDR size")


def check_facts(entry_id, data):
    if entry_id in VBA_ENTRIES:
        # The VBA project is never parsed: only its bytes, type and relationship are checked.
        name, part, rels, _plain, _macro = VBA_HOSTS[VBA_ENTRIES[entry_id]]
        z = members(data)
        require(z.read(part) == (FIXTURES_DIR / name).read_bytes(), f"{part} is not {name}")
        types = z.read("[Content_Types].xml").decode()
        require(f'PartName="/{part}" ContentType="{VBA_CONTENT_TYPE}"' in types, "no VBA type")
        require(VBA_RELATIONSHIP.encode() in z.read(rels), f"{rels} has no vbaProject link")
    elif entry_id == "docx-breaks12":
        z = members(data)
        app = z.read("docProps/app.xml").decode()
        require("<Pages>1</Pages>" in app, "app.xml no longer says Pages=1")
        require("<Application>Microsoft Macintosh Word</Application>" in app,
                "app.xml no longer names Microsoft Macintosh Word")
        breaks = z.read("word/document.xml").decode().count('w:type="page"')
        require(breaks == 11, f"{breaks} page breaks, 12 pages need 11")
    elif entry_id == "pptx-hidden":
        z = members(data)
        hidden = [n for n in z.namelist() if re.fullmatch(r"ppt/slides/slide\d+\.xml", n)
                  and re.search(rb'<p:sld [^>]*show="0"', z.read(n))]
        require(len(hidden) == len(HIDDEN_SLIDES), f"hidden slides {hidden}")
    elif entry_id == "pptx-aptos-theme":
        z = members(data)
        layout = next(n for n in z.namelist() if n.startswith("ppt/slideLayouts/slideLayout")
                      and b"Zzyzx Grotesk" in z.read(n))
        target = ("../slideLayouts/" + layout.rsplit("/", 1)[1]).encode()
        for name in z.namelist():
            if name.startswith("ppt/slides/_rels/"):
                require(target not in z.read(name), f"{name} reaches {layout}")
        theme = z.read("ppt/theme/theme1.xml").decode()
        require(len(re.findall(r'<a:(?:major|minor)Font>\s*<a:latin typeface="Aptos"', theme)) == 2,
                "theme fonts are not Aptos")
    elif entry_id == "pptx-pages-mismatch":
        z = members(data)
        require(z.read("ppt/presentation.xml").count(b"<p:sldId ") == 4, "sldIdLst does not list 4")
        require(b"<p:sld " not in z.read("ppt/slides/slide4.xml"), "slide 4 is still a slide")
    elif entry_id in ("docx-aptos-theme", "docx-headings"):
        theme = members(data).read("word/theme/theme1.xml").decode()
        slot, family = {"docx-aptos-theme": ("minorFont", "Aptos"),
                        "docx-headings": ("majorFont", "Aptos Display")}[entry_id]
        require(re.search(rf'<a:{slot}>\s*<a:latin typeface="{family}"', theme), "theme font")
    elif entry_id in ("pdf-text3", "pdf-white-rect", "pdf-tiny-text"):
        want = 3 if entry_id == "pdf-text3" else 1
        require(data.count(b"/Type /Page ") == want, "page count")


# --- expected.json ----------------------------------------------------------------

def environment():
    """What the hashes depend on besides the pinned packages."""
    return {"python": platform.python_version(), "zlib": zlib.ZLIB_RUNTIME_VERSION,
            "platform": f"{platform.system()}-{platform.machine()}"}


def generated_entries(doc):
    return {f["id"]: f for f in doc["files"] if f["origin"] in ("generated", "hostile")}


def compare(built, doc):
    problems = []
    listed = generated_entries(doc)
    for entry_id in sorted(set(built) - set(listed)):
        problems.append(f"{entry_id}: generated but not listed in expected.json")
    for entry_id in sorted(set(listed) - set(built)):
        problems.append(f"{entry_id}: listed in expected.json but not generated")
    for entry_id in sorted(set(built) & set(listed)):
        rel, data = built[entry_id]
        entry = listed[entry_id]
        if entry["path"] != f"out/{rel}":
            problems.append(f"{entry_id}: path {entry['path']}, the generator writes out/{rel}")
        if entry_id in UNHASHED:
            if "sha256" in entry or "unhashed" not in entry:
                problems.append(f'{entry_id}: must have "unhashed" and no "sha256"')
        elif entry.get("sha256") != sha256(data):
            problems.append(f"{entry_id}: expected.json has {entry.get('sha256')}, "
                            f"the generator wrote {sha256(data)}")
    return problems


def write_hashes(built, doc):
    """Puts sha256 or unhashed right after origin in every generated entry."""
    for entry in generated_entries(doc).values():
        if entry["id"] not in built:
            continue
        _rel, data = built[entry["id"]]
        entry.pop("sha256", None)
        entry.pop("unhashed", None)
        items = list(entry.items())
        entry.clear()
        for key, value in items:
            entry[key] = value
            if key == "origin" and entry["id"] in UNHASHED:
                entry["unhashed"] = UNHASHED[entry["id"]]
            elif key == "origin":
                entry["sha256"] = sha256(data)
    ordered = {"schema_version": doc["schema_version"], "generator": environment()}
    ordered.update((k, v) for k, v in doc.items() if k not in ordered)
    EXPECTED.write_text(json.dumps(ordered, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def spike_layout(out):
    out.mkdir(parents=True, exist_ok=True)
    ctx = {}
    for name, entry_id in SPIKE_LAYOUT:
        if entry_id is None:
            data = spike_aptos(ctx)
        else:
            data = BUILDERS[entry_id](ctx)
            ctx[entry_id] = data
        (out / name).write_bytes(data)
        print(name)


def main(argv=None):
    parser = argparse.ArgumentParser(description="Generates the rs-convert test corpus.")
    parser.add_argument("out", nargs="?", type=Path, help="output directory, normally corpus/out")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="compare a fresh run with expected.json")
    mode.add_argument("--write-hashes", action="store_true", help="write the hashes into expected.json")
    mode.add_argument("--spike-layout", action="store_true", help="write the M0 spike file names")
    parser.add_argument("--hostile", action="store_true", help="also write hostile files and canaries")
    args = parser.parse_args(argv)

    if args.check or args.write_hashes:
        if args.out is not None:
            parser.error("--check and --write-hashes take no output directory")
        doc = json.loads(EXPECTED.read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory(prefix="corpus-check-") as tmp:
            built = generate(Path(tmp), hostile=True)
        if args.write_hashes:
            write_hashes(built, doc)
            print(f"wrote {EXPECTED}")
            return 0
        recorded, current = doc.get("generator"), environment()
        if recorded != current:
            print(f"check: the hashes were recorded with {recorded}, this run uses {current}; "
                  "deflate output can differ between zlib builds", file=sys.stderr)
        problems = compare(built, doc)
        for problem in problems:
            print(f"check: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"check: all {len(built)} generated files match {EXPECTED.name}")
        return 0
    if args.out is None:
        parser.error("an output directory is required")
    if args.spike_layout:
        spike_layout(args.out)
    else:
        generate(args.out, hostile=args.hostile)
    return 0


if __name__ == "__main__":
    sys.exit(main())
