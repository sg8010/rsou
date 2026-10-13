mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rsou_lib::{
    import::{ImportOptions, run_import},
    repo,
    store::{self, OpenMode},
};
use serde_json::json;

fn server(results: Vec<serde_json::Value>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for result in results {
            for (route, body) in [
                ("POST /api/ocr/pdf", json!({"job_id":"test-job"})),
                ("GET /api/ocr/job/test-job?wait=2", result),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with(route), "{line}");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse::<usize>().unwrap();
                    }
                }
                let mut data = vec![0; length];
                reader.read_exact(&mut data).unwrap();
                if route.starts_with("POST") {
                    let body = String::from_utf8_lossy(&data);
                    assert!(body.contains("name=\"file\""));
                    assert!(body.contains("%PDF"));
                }
                let body = body.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        }
    });
    (url, handle)
}

fn run(db: &Path, inputs: Vec<std::path::PathBuf>, url: Option<String>) -> repo::ImportCounts {
    run_import(
        db,
        inputs,
        ImportOptions {
            ocr_base_url: url,
            ..Default::default()
        },
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
    .unwrap()
}

#[test]
fn explicit_ocr_only_processes_eligible_failures_and_preserves_membership() {
    let dir = common::temp_dir("ocr");
    let docs = dir.join("docs");
    std::fs::create_dir(&docs).unwrap();
    let scanned = docs.join("扫描.pdf");
    let good = docs.join("正常.txt");
    let bad = docs.join("空.txt");
    std::fs::write(&scanned, common::scanned_pdf_fixture()).unwrap();
    std::fs::write(&good, "普通文档不调用 OCR").unwrap();
    std::fs::write(&bad, "").unwrap();
    let db = dir.join("index.db");
    let counts = run(&db, vec![docs.clone()], None);
    assert_eq!((counts.ok, counts.failed), (1, 2));
    let (url, handle) = server(vec![
        json!({"status":"done","complete":true,"truncated":false,"markdown":"扫描识别验证正文"}),
    ]);
    let counts = run(&db, vec![scanned.clone(), good, bad], Some(url.clone()));
    handle.join().unwrap();
    assert_eq!((counts.total, counts.ok, counts.failed), (1, 1, 0));
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let doc = repo::find_document_by_path(&conn, &scanned)
        .unwrap()
        .unwrap();
    assert_eq!(doc.parse_status, "parsed");
    assert_eq!(doc.source_root.as_deref(), docs.to_str());
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '识别'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // 缓存过期仍带上已成功项,不得重做 OCR 或覆盖正文。
    assert_eq!(run(&db, vec![scanned], Some(url)).total, 0);
}

#[test]
fn incomplete_and_empty_results_stay_retryable_and_do_not_stop_batch() {
    let dir = common::temp_dir("ocr-failures");
    let db = dir.join("index.db");
    let paths: Vec<_> = (0..3)
        .map(|n| {
            let p = dir.join(format!("{n}.pdf"));
            std::fs::write(&p, common::scanned_pdf_fixture()).unwrap();
            p
        })
        .collect();
    run(&db, paths.clone(), None);
    let (url, handle) = server(vec![
        json!({"status":"partial","complete":false,"markdown":"不可索引"}),
        json!({"status":"done","complete":true,"truncated":false,"markdown":"<!-- page 1 -->\n"}),
        json!({"status":"done","complete":true,"truncated":false,"markdown":"完整结果"}),
    ]);
    let counts = run(&db, paths.clone(), Some(url));
    handle.join().unwrap();
    assert_eq!((counts.ok, counts.failed), (1, 2));
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    for path in &paths[..2] {
        let doc = repo::find_document_by_path(&conn, path).unwrap().unwrap();
        assert_eq!(doc.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    }
    assert_eq!(
        conn.query_row("SELECT count(*) FROM documents_fts", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let cancel = Arc::new(AtomicBool::new(true));
    let counts = run_import(
        &db,
        paths,
        ImportOptions {
            ocr_base_url: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        },
        cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(counts.processed, 0);
}

#[test]
fn unavailable_service_leaves_ocr_failure_retryable() {
    let dir = common::temp_dir("ocr-unavailable");
    let pdf = dir.join("扫描.pdf");
    let db = dir.join("index.db");
    std::fs::write(&pdf, common::scanned_pdf_fixture()).unwrap();
    run(&db, vec![pdf.clone()], None);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(run(&db, vec![pdf.clone()], Some(url)).failed, 1);
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    assert_eq!(
        repo::find_document_by_path(&conn, &pdf)
            .unwrap()
            .unwrap()
            .parse_error_code
            .as_deref(),
        Some("NEEDS_OCR")
    );
}

#[test]
#[ignore = "需本机真实 OCR 服务及 RSOU_OCR_TEST_PDF 扫描件路径"]
fn real_service_scan_to_index() {
    let path = std::env::var("RSOU_OCR_TEST_PDF").unwrap();
    let dir = common::temp_dir("ocr-real");
    let db = dir.join("index.db");
    let pdf = dir.join("扫描.pdf");
    std::fs::copy(path, &pdf).unwrap();
    assert_eq!(run(&db, vec![pdf.clone()], None).failed, 1);
    assert_eq!(
        run(
            &db,
            vec![pdf],
            Some(rsou_lib::ocr::DEFAULT_BASE_URL.to_owned())
        )
        .ok,
        1
    );
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH 'verification'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn saved_ocr_address_survives_reopen_and_invalid_edits_keep_prior_value() {
    let dir = common::temp_dir("ocr-settings");
    let db = dir.join("index.db");
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    use rsou_lib::ocr::{DEFAULT_BASE_URL, configured_base_url, save_base_url};
    assert_eq!(configured_base_url(&conn, None).unwrap(), DEFAULT_BASE_URL);
    assert_eq!(
        configured_base_url(&conn, Some("http://localhost:9004/")).unwrap(),
        "http://localhost:9004"
    );
    assert_eq!(
        save_base_url(&conn, "  http://localhost:9005/  ").unwrap(),
        "http://localhost:9005"
    );
    drop(conn);
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    assert_eq!(
        configured_base_url(&conn, Some("http://localhost:9004")).unwrap(),
        "http://localhost:9005"
    );
    for invalid in [
        "",
        "localhost:9003",
        "https://localhost:9003",
        "http://localhost:9003?wait=1",
        "http://localhost/#fragment",
        "http://user:password@localhost",
    ] {
        assert!(save_base_url(&conn, invalid).is_err(), "{invalid}");
        assert_eq!(
            configured_base_url(&conn, None).unwrap(),
            "http://localhost:9005"
        );
    }
}

#[test]
fn connection_test_checks_info_endpoint_and_rejects_unrelated_services() {
    for (status, body, success) in [
        ("200 OK", r#"{"engine":"onnxruntime","workers":4}"#, true),
        ("200 OK", r#"{"message":"ok"}"#, false),
        ("200 OK", "not json", false),
        (
            "503 Service Unavailable",
            r#"{"engine":"onnxruntime","workers":4}"#,
            false,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "GET /api/info HTTP/1.1\r\n");
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let result = rsou_lib::ocr::Client::new(&url).unwrap().test_connection();
        assert_eq!(result.is_ok(), success, "{result:?}");
        handle.join().unwrap();
    }
}

#[test]
#[ignore = "需本机 9003 OCR 服务"]
fn real_service_connection_test() {
    let info = rsou_lib::ocr::Client::new(rsou_lib::ocr::DEFAULT_BASE_URL)
        .unwrap()
        .test_connection()
        .unwrap();
    assert!(info.contains("连接成功"));
}
