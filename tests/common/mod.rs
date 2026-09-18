//! 测试 fixtures:各格式的最小可解析样本(与 spikes/anydoc-matrix 同源,
//! 补了 pptx/odt/rtf/csv/epub/txt 与 GB18030 编码)。

// 不同测试文件各取所需,未用到的构造器不算死代码。
#![allow(dead_code)]

use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

static TEMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// 建一个唯一的临时目录(测试用,不清理:失败时留现场可查)。
pub fn temp_dir(tag: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rsou-test-{tag}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

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
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>阶段二文档</w:t></w:r></w:p>
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
    <row r="1"><c r="A1" t="inlineStr"><is><t>阶段二表格</t></is></c><c r="B1"><v>42</v></c></row>
  </sheetData>
</worksheet>"#,
        ),
    ])
}

/// Minimal PresentationML package: one slide with one text run.
pub fn pptx_fixture() -> Vec<u8> {
    zip_package(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
</Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
        ),
        (
            "ppt/presentation.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>
</p:presentation>"#,
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
</Relationships>"#,
        ),
        (
            "ppt/slides/slide1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:txBody><a:p><a:r><a:t>阶段二演示</a:t></a:r></a:p></p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#,
        ),
    ])
}

/// Minimal ODF text package: mimetype + content.xml。
pub fn odt_fixture() -> Vec<u8> {
    zip_package(&[
        ("mimetype", "application/vnd.oasis.opendocument.text"),
        (
            "content.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:text><text:p>阶段二文档</text:p></office:text></office:body>
</office:document-content>"#,
        ),
    ])
}

/// Minimal EPUB: mimetype + container + OPF + 一章 xhtml。
pub fn epub_fixture() -> Vec<u8> {
    zip_package(&[
        ("mimetype", "application/epub+zip"),
        (
            "META-INF/container.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        ),
        (
            "OEBPS/content.opf",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>阶段二电子书</dc:title></metadata>
  <manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#,
        ),
        (
            "OEBPS/ch1.xhtml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml">
<body><h1>阶段二电子书</h1><p>电子书正文内容。</p></body>
</html>"#,
        ),
    ])
}

/// 最小 RTF(纯 ASCII 标记 + \uN 中文转义)。
pub fn rtf_fixture() -> Vec<u8> {
    // {\rtf1 阶段二RTF}: 38454=阶 27573=段 20108=二;RTF 里 \uN 按有符号 16 位写
    br"{\rtf1\ansi\uc1 stage two \u38454\u27573\u20108 RTF}"
        .as_slice()
        .to_vec()
}

/// CSV(无签名,只能靠扩展名判定)。
pub fn csv_fixture() -> Vec<u8> {
    "名称,数量\n阶段二表格,42\n".as_bytes().to_vec()
}

/// UTF-8 文本。
pub fn txt_utf8_fixture() -> Vec<u8> {
    "# 阶段二文本\n\n正文内容,第二行。\n".as_bytes().to_vec()
}

/// GB18030 编码文本(无 BOM,UTF-8 严格解码必然失败)。
pub fn txt_gb18030_fixture() -> Vec<u8> {
    let (bytes, _, _) = encoding_rs::GB18030.encode("中文测试文本内容");
    bytes.into_owned()
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
    let stream = b"BT /F1 18 Tf 50 200 Td (stage two pdf text) Tj ET";
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
