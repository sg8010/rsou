//! 解析矩阵:每种格式的 fixture 经 parse_bytes 解析成功且含关键中文;
//! 错误路径(扫描 PDF / 空文件 / 未知扩展名 / 加密)落到正确错误码。

mod common;

use std::path::Path;

use anydoc::ConvertError;
use rsou_lib::parse::{self, ParseErrorCode, file_type_of};

#[test]
fn every_supported_fixture_parses_with_chinese_text() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("docx", common::docx_fixture(), "阶段二文档"),
        ("xlsx", common::xlsx_fixture(), "阶段二表格"),
        ("pptx", common::pptx_fixture(), "阶段二演示"),
        ("odt", common::odt_fixture(), "阶段二文档"),
        ("epub", common::epub_fixture(), "阶段二电子书"),
        ("pdf", common::text_pdf_fixture(), "stage two pdf text"),
        ("csv", common::csv_fixture(), "阶段二表格"),
        ("txt", common::txt_utf8_fixture(), "阶段二文本"),
    ];
    for (ext, bytes, keyword) in &cases {
        let name = format!("样例.{ext}");
        let path = Path::new(&name);
        let parsed =
            parse::parse_bytes(path, bytes).unwrap_or_else(|e| panic!("{ext} 解析失败: {e}"));
        assert!(
            parsed.markdown.contains(keyword),
            "{ext} 的 markdown 应包含 {keyword:?},实际:{}",
            &parsed.markdown[..parsed.markdown.len().min(300)]
        );
    }
}

#[test]
fn rtf_parses() {
    let parsed =
        parse::parse_bytes(Path::new("样例.rtf"), &common::rtf_fixture()).expect("rtf 应解析成功");
    assert!(parsed.markdown.contains("stage two"));
}

#[test]
fn scanned_pdf_reports_needs_ocr() {
    let error = parse::parse_bytes(Path::new("扫描.pdf"), &common::scanned_pdf_fixture())
        .expect_err("扫描 PDF 应报需要 OCR");
    assert_eq!(error.code, ParseErrorCode::NeedsOcr);
    assert!(error.message.contains("OCR"));
}

#[test]
fn gb18030_text_decodes_with_warning() {
    let parsed = parse::parse_bytes(Path::new("老文本.txt"), &common::txt_gb18030_fixture())
        .expect("GB18030 文本应回退解码成功");
    assert!(parsed.markdown.contains("中文测试文本内容"));
    assert_eq!(parsed.warnings, vec!["按 GB18030 解码".to_owned()]);
}

#[test]
fn empty_text_file_is_empty_error() {
    for bytes in [&b""[..], b"   \n\n  ".as_slice()] {
        let error = parse::parse_bytes(Path::new("空.txt"), bytes).expect_err("空文本应报 EMPTY");
        assert_eq!(error.code, ParseErrorCode::Empty);
    }
}

#[test]
fn unsupported_extension_is_rejected_early() {
    assert_eq!(file_type_of(Path::new("数据.xyz")), None);
    assert_eq!(file_type_of(Path::new("没有扩展名")), None);
    let error = parse::parse_bytes(Path::new("数据.xyz"), b"whatever")
        .expect_err("未知扩展名应报 UNSUPPORTED");
    assert_eq!(error.code, ParseErrorCode::Unsupported);
}

#[test]
fn convert_error_mapping_produces_chinese_messages() {
    let error = parse::map_convert_error(&ConvertError::Encrypted);
    assert_eq!(error.code, ParseErrorCode::Encrypted);
    assert!(error.message.contains("加密"));

    let error = parse::map_convert_error(&ConvertError::NeedsOcr {
        pages: vec![1],
        page_count: 3,
    });
    assert_eq!(error.code, ParseErrorCode::NeedsOcr);
    assert!(error.message.contains("OCR"));

    let error = parse::map_convert_error(&ConvertError::ResourceLimit {
        limit: "max_entry_bytes",
        detail: "太大".to_owned(),
    });
    assert_eq!(error.code, ParseErrorCode::ResourceLimit);
    assert!(error.detail.contains("max_entry_bytes"));

    let error = parse::map_convert_error(&ConvertError::MissingPart {
        part: "word/document.xml".to_owned(),
    });
    assert_eq!(error.code, ParseErrorCode::Malformed);
    assert!(error.detail.contains("word/document.xml"));
}

#[test]
fn every_listed_extension_maps_to_a_type() {
    for ext in parse::supported_extensions() {
        let name = format!("样例.{ext}");
        let path = Path::new(&name);
        assert!(
            file_type_of(path).is_some(),
            "{ext} 应在 file_type_of 中有类型"
        );
    }
}
