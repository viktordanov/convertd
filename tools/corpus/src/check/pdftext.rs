//! The text and the image count of a PDF, for the `text_present`,
//! `text_absent` and `pdf_images` expectations.
//!
//! The text comes from the content stream of every page and from every Form
//! XObject that a content stream draws with `Do`, nested forms included.
//! lopdf's own `extract_text` reads the page streams only, so text inside a
//! form would be missed.

use std::collections::BTreeSet;

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

/// The most decompressed content that the text walk reads in total. A corpus
/// PDF is small, and a larger one fails the case.
const MAX_CONTENT: usize = 64 << 20;
/// The deepest nesting of forms that the walk follows.
const MAX_FORM_DEPTH: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfFacts {
    /// The text of every page, in page order, with all white space removed,
    /// so that the line breaks and word gaps of the extraction do not decide
    /// a match. `None` when the case did not ask for the text.
    pub text: Option<String>,
    /// The image XObjects in the file, soft masks included.
    pub images: usize,
}

/// Counts the image XObjects of `pdf`, and reads the text of every page when
/// `with_text` is set.
pub fn read(pdf: &[u8], with_text: bool) -> Result<PdfFacts, String> {
    let doc = Document::load_mem(pdf).map_err(|err| format!("failed to parse the PDF: {err}"))?;
    let images = doc
        .objects
        .values()
        .filter(|object| {
            object
                .as_stream()
                .is_ok_and(|stream| subtype(&stream.dict) == Some(b"Image".as_slice()))
        })
        .count();
    let text = if with_text {
        Some(squeeze(&text(&doc)?))
    } else {
        None
    };
    Ok(PdfFacts { text, images })
}

/// `text` without white space, the form that [`PdfFacts::text`] holds.
pub fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn subtype(dict: &Dictionary) -> Option<&[u8]> {
    dict.get(b"Subtype").and_then(Object::as_name).ok()
}

/// The text of every page, forms included.
fn text(doc: &Document) -> Result<String, String> {
    let mut walk = Walk {
        doc,
        out: String::new(),
        budget: MAX_CONTENT,
    };
    for page_id in doc.get_pages().into_values() {
        let content = doc
            .get_page_content_with_limit(page_id, walk.budget)
            .map_err(|err| format!("failed to read the content of page {page_id:?}: {err}"))?;
        walk.charge(content.len())?;
        let (inline, ids) = doc
            .get_page_resources(page_id)
            .map_err(|err| format!("failed to read the resources of page {page_id:?}: {err}"))?;
        let mut resources: Vec<&Dictionary> = inline.into_iter().collect();
        resources.extend(ids.into_iter().filter_map(|id| doc.get_dictionary(id).ok()));
        walk.content(&content, &resources, &mut BTreeSet::new())?;
    }
    Ok(walk.out)
}

struct Walk<'a> {
    doc: &'a Document,
    out: String,
    /// The decompressed bytes that the walk may still read.
    budget: usize,
}

impl<'a> Walk<'a> {
    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        self.budget = self
            .budget
            .checked_sub(bytes)
            .ok_or_else(|| format!("the PDF content is over {MAX_CONTENT} bytes"))?;
        Ok(())
    }

    /// The entry `name` of the `kind` dictionary (`Font` or `XObject`) in the
    /// first resource dictionary that has it.
    fn resource(
        &self,
        resources: &[&'a Dictionary],
        kind: &[u8],
        name: &[u8],
    ) -> Option<&'a Object> {
        resources.iter().find_map(|res| {
            let entries = res.get_deref(kind, self.doc).ok()?.as_dict().ok()?;
            entries.get(name).ok()
        })
    }

    /// The text of one content stream. `forms` holds the forms on the
    /// current path, against a form that draws itself.
    fn content(
        &mut self,
        data: &[u8],
        resources: &[&'a Dictionary],
        forms: &mut BTreeSet<ObjectId>,
    ) -> Result<(), String> {
        let content = Content::decode(data)
            .map_err(|err| format!("failed to decode a content stream: {err}"))?;
        let mut encoding = None;
        for op in &content.operations {
            match op.operator.as_str() {
                "Tf" => {
                    encoding = op
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| self.resource(resources, b"Font", name))
                        .and_then(|font| match font {
                            Object::Reference(id) => self.doc.get_dictionary(*id).ok(),
                            other => other.as_dict().ok(),
                        })
                        .and_then(|font| {
                            font.get_font_encoding_with_limit(self.doc, MAX_CONTENT)
                                .ok()
                        });
                }
                "Tj" | "TJ" | "'" | "\"" => {
                    // A string with no usable font is skipped, as lopdf does.
                    if let Some(encoding) = &encoding {
                        self.out.push(' ');
                        let strings = if op.operator == "\"" {
                            op.operands.get(2).into_iter().collect::<Vec<_>>()
                        } else {
                            op.operands.iter().collect()
                        };
                        for operand in strings {
                            collect(&mut self.out, encoding, operand);
                        }
                    }
                }
                "Do" => {
                    let Some(Object::Reference(id)) = op
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| self.resource(resources, b"XObject", name))
                    else {
                        continue;
                    };
                    self.form(*id, resources, forms)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The text of the Form XObject `id`. An image or a form already on the
    /// path draws no text.
    fn form(
        &mut self,
        id: ObjectId,
        outer: &[&'a Dictionary],
        forms: &mut BTreeSet<ObjectId>,
    ) -> Result<(), String> {
        let Ok(stream) = self.doc.get_object(id).and_then(Object::as_stream) else {
            return Ok(());
        };
        if subtype(&stream.dict) != Some(b"Form".as_slice()) || forms.contains(&id) {
            return Ok(());
        }
        if forms.len() >= MAX_FORM_DEPTH {
            return Err(format!("forms nest deeper than {MAX_FORM_DEPTH}"));
        }
        let data = stream
            .decompressed_content_with_limit(self.budget)
            .or_else(|_| {
                if stream.content.len() <= self.budget {
                    Ok(stream.content.clone())
                } else {
                    Err(())
                }
            })
            .map_err(|()| format!("the PDF content is over {MAX_CONTENT} bytes"))?;
        self.charge(data.len())?;
        // A form without its own resources uses those of the content that
        // draws it (PDF 32000-1, 7.8.3).
        let own = stream
            .dict
            .get_deref(b"Resources", self.doc)
            .and_then(Object::as_dict)
            .ok();
        let resources: Vec<&Dictionary> = match own {
            Some(dict) => vec![dict],
            None => outer.to_vec(),
        };
        forms.insert(id);
        let result = self.content(&data, &resources, forms);
        forms.remove(&id);
        result
    }
}

/// Appends the text of a `Tj` string or a `TJ` array.
fn collect(out: &mut String, encoding: &lopdf::Encoding, operand: &Object) {
    match operand {
        Object::String(bytes, _) => {
            if let Ok(text) = Document::decode_text(encoding, bytes) {
                out.push_str(&text);
            }
        }
        Object::Array(items) => {
            for item in items {
                collect(out, encoding, item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One page in a standard font. `page_text` is drawn by the page; when
    /// `form_text` is given, a Form XObject draws it through `Do`. The PDF
    /// also has one image XObject. lopdf's loader rebuilds the xref table,
    /// so the offsets are not written.
    fn pdf(page_text: &str, form_text: Option<&str>) -> Vec<u8> {
        let mut objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
             /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R /Fm1 7 0 R >> >> \
             /Contents 5 0 R >>"
                .into(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .into(),
        ];
        let draw_form = if form_text.is_some() { " /Fm1 Do" } else { "" };
        let page = format!("BT /F1 12 Tf 72 720 Td {page_text} ET{draw_form}");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{page}\nendstream",
            page.len()
        ));
        objects.push(
            "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
             /BitsPerComponent 8 /Length 1 >>\nstream\n\u{7f}\nendstream"
                .into(),
        );
        // The form has no /Resources of its own, so it uses the page's font.
        let form = format!("BT /F1 12 Tf 72 600 Td ({}) Tj ET", form_text.unwrap_or(""));
        objects.push(format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 595 842] /Length {} >>\nstream\n{form}\nendstream",
            form.len()
        ));
        let mut out = String::from("%PDF-1.4\n");
        for (i, body) in objects.iter().enumerate() {
            out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\n%%EOF\n",
            objects.len() + 1
        ));
        out.into_bytes()
    }

    #[test]
    fn reads_the_text_without_white_space_and_counts_images() {
        let facts = read(
            &pdf(
                "(Before the link.) Tj 0 -14 Td [(CACHED FIELD) -250 ( RESULT)] TJ",
                None,
            ),
            true,
        )
        .expect("parse");
        let text = facts.text.expect("text");
        assert!(text.contains(&squeeze("CACHED FIELD RESULT")), "{text}");
        assert!(text.contains("Beforethelink."), "{text}");
        assert!(!text.contains(&squeeze("Debian GNU/Linux")));
        assert_eq!(facts.images, 1);
    }

    #[test]
    fn text_drawn_by_a_form_is_read() {
        let facts =
            read(&pdf("(On the page.) Tj", Some("Debian GNU/Linux 13")), true).expect("parse");
        let text = facts.text.expect("text");
        assert!(text.contains("Onthepage."), "{text}");
        assert!(text.contains(&squeeze("Debian GNU/Linux 13")), "{text}");
    }

    #[test]
    fn an_image_count_alone_reads_no_text() {
        let facts = read(&pdf("(On the page.) Tj", None), false).expect("parse");
        assert_eq!(facts.text, None);
        assert_eq!(facts.images, 1);
    }

    #[test]
    fn a_broken_pdf_is_an_error() {
        assert!(read(b"not a pdf", true).is_err());
    }
}
