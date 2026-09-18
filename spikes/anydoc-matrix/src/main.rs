use anydoc::{Format, to_markdown_bytes};
use rsou_anydoc_matrix_spike::{docx_fixture, scanned_pdf_fixture, text_pdf_fixture, xlsx_fixture};

fn report(name: &str, bytes: &[u8], format: Format) {
    match to_markdown_bytes(bytes, format) {
        Ok(markdown) => {
            let preview = markdown.lines().take(2).collect::<Vec<_>>().join(" / ");
            println!(
                "{name}: OK markdown_bytes={} preview={preview:?}",
                markdown.len()
            );
        }
        Err(error) => {
            println!("{name}: ERROR code={} message={error}", error.code());
        }
    }
}

fn main() {
    println!(
        "anydoc {} target matrix fixture run",
        env!("CARGO_PKG_VERSION")
    );
    report("docx", &docx_fixture(), Format::Docx);
    report("xlsx", &xlsx_fixture(), Format::Excel);
    report("pdf-text", &text_pdf_fixture(), Format::Pdf);
    report("pdf-scanned", &scanned_pdf_fixture(), Format::Pdf);
}
