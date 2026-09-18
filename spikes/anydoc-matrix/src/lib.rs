//! Tiny, deterministic fixtures for the anydoc compatibility spike.

use std::io::{Cursor, Write};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

/// Build a ZIP package from `(path, contents)` pairs.
pub fn zip_package(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (path, contents) in parts {
        writer.start_file(path, options).expect("start ZIP part");
        writer
            .write_all(contents.as_bytes())
            .expect("write ZIP part");
    }
    writer.finish().expect("finish ZIP package").into_inner()
}

/// Small WordprocessingML document with one heading and one paragraph.
pub fn docx_fixture() -> Vec<u8> {
    zip_package(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
        ),
        (
            "word/document.xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>阶段零文档</w:t></w:r></w:p>
    <w:p><w:r><w:t>anydoc docx 编译与解析验证。</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
        ),
    ])
}

/// Minimal SpreadsheetML package using inline strings (no shared-string part).
pub fn xlsx_fixture() -> Vec<u8> {
    zip_package(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="验证表" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>阶段零表格</t></is></c><c r="B1"><v>42</v></c></row>
  </sheetData>
</worksheet>"#,
        ),
    ])
}

fn pdf_from_objects(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len() + 1);
    offsets.push(0usize);
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    pdf
}

/// One text-based PDF page.
pub fn text_pdf_fixture() -> Vec<u8> {
    let stream = b"BT /F1 18 Tf 50 200 Td (stage zero pdf text) Tj ET";
    let mut contents = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
    contents.extend_from_slice(stream);
    contents.extend_from_slice(b"\nendstream");
    pdf_from_objects(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        contents,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ])
}

/// One image-only PDF page.  It is deliberately tiny, but contains a real
/// image XObject so pdf-inspector exercises the scanned-page path.
pub fn scanned_pdf_fixture() -> Vec<u8> {
    let stream = b"q 200 0 0 200 50 50 cm /Im1 Do Q";
    let mut contents = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
    contents.extend_from_slice(stream);
    contents.extend_from_slice(b"\nendstream");

    let image_data = [220u8, 220, 220];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
        image_data.len()
    )
    .into_bytes();
    image.extend_from_slice(&image_data);
    image.extend_from_slice(b"\nendstream");

    pdf_from_objects(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        contents,
        image,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use anydoc::{ConvertError, Format, to_markdown_bytes};

    #[test]
    fn docx_fixture_is_converted() {
        let markdown = to_markdown_bytes(&docx_fixture(), Format::Docx).unwrap();
        assert!(markdown.contains("阶段零文档"));
        assert!(markdown.contains("anydoc docx"));
    }

    #[test]
    fn xlsx_fixture_is_converted() {
        let markdown = to_markdown_bytes(&xlsx_fixture(), Format::Excel).unwrap();
        assert!(markdown.contains("阶段零表格"));
        assert!(markdown.contains("42"));
    }

    #[test]
    fn text_pdf_fixture_is_converted() {
        let markdown = to_markdown_bytes(&text_pdf_fixture(), Format::Pdf).unwrap();
        assert!(markdown.contains("stage zero pdf text"));
    }

    #[test]
    fn scanned_pdf_fixture_is_reported_as_needing_ocr() {
        let error = to_markdown_bytes(&scanned_pdf_fixture(), Format::Pdf).unwrap_err();
        assert_eq!(error.code(), "needsOcr");
        assert!(matches!(error, ConvertError::NeedsOcr { .. }));
    }
}
