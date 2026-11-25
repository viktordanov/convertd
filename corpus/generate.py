"""Builds a small synthetic corpus for the M0 spikes. Usage: make_corpus.py <out_dir>"""
import io
import sys
from pathlib import Path

import docx
import msoffcrypto
import openpyxl
from docx.shared import Pt
from pptx import Presentation
from pptx.util import Inches

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)
LOREM = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor "
         "incididunt ut labore et dolore magna aliqua. ") * 6


def make_docx(path, paragraphs, font="Calibri"):
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
    d.save(path)


make_docx(out / "small.docx", 4)
make_docx(out / "long.docx", 160)  # about 30 pages
make_docx(out / "aptos.docx", 4, font="Aptos")  # font that is not installed

p = Presentation()
for i in range(20):
    s = p.slides.add_slide(p.slide_layouts[1])
    s.shapes.title.text = f"Slide {i + 1}"
    s.placeholders[1].text = "First point\nSecond point\nThird point"
    s.shapes.add_shape(1, Inches(6), Inches(5), Inches(2), Inches(1))
p.save(out / "deck.pptx")

wb = openpyxl.Workbook()
for n in range(3):
    ws = wb.active if n == 0 else wb.create_sheet()
    ws.title = f"Sheet{n + 1}"
    for r in range(1, 201):
        for c in range(1, 11):
            ws.cell(row=r, column=c, value=r * c)
wb.save(out / "sheet.xlsx")

# Password-protected DOCX (password: secret)
plain = io.BytesIO((out / "small.docx").read_bytes())
office = msoffcrypto.OfficeFile(plain)
with open(out / "encrypted.docx", "wb") as f:
    office.encrypt("secret", f)

print("\n".join(sorted(x.name for x in out.iterdir())))

# Photo-like deck: smooth gradients with noise, the hard case for PNG and lossless WebP.
import random

from PIL import Image, ImageFilter

random.seed(7)
photo_deck = Presentation()
photo_deck.slide_width, photo_deck.slide_height = Inches(13.333), Inches(7.5)
for i in range(10):
    w, h = 1600, 900
    img = Image.radial_gradient("L").resize((w, h)).convert("RGB")
    noise = Image.effect_noise((w, h), 40 + i * 4).convert("RGB")
    img = Image.blend(img, noise, 0.35).filter(ImageFilter.GaussianBlur(1))
    r, g, b = img.split()
    img = Image.merge("RGB", (r, g.point(lambda v: (v * 3 + i * 20) % 256), b.point(lambda v: 255 - v)))
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=90)
    buf.seek(0)
    slide = photo_deck.slides.add_slide(photo_deck.slide_layouts[6])
    slide.shapes.add_picture(buf, 0, 0, photo_deck.slide_width, photo_deck.slide_height)
photo_deck.save(out / "photos.pptx")
print("photos.pptx")
