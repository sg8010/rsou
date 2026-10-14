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
    server_responses(
        results
            .into_iter()
            .flat_map(|result| {
                [
                    (
                        "POST /api/ocr/pdf",
                        "200 OK",
                        json!({"job_id":"test-job","status":"pending","total_pages":1}),
                    ),
                    ("GET /api/ocr/job/test-job?wait=2", "200 OK", result),
                ]
            })
            .collect(),
    )
}

fn server_responses(
    responses: Vec<(&'static str, &'static str, serde_json::Value)>,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for (route, status, body) in responses {
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
                // 提交只含文件，不携带额外处理选项。
                assert_eq!(body.matches("Content-Disposition: form-data;").count(), 1);
            }
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
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
        json!({"status":"done","total_pages":1,"markdown":"扫描识别验证正文"}),
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
        json!({"status":"partial","total_pages":2,"error":"第 2 页识别失败","markdown":"不可索引"}),
        json!({"status":"done","total_pages":1,"markdown":"<!-- page 1 -->\n"}),
        json!({"status":"done","complete":true,"total_pages":1,"markdown":"完整结果"}),
    ]);
    let counts = run(&db, paths.clone(), Some(url));
    handle.join().unwrap();
    assert_eq!((counts.ok, counts.failed), (1, 2));
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    for path in &paths[..2] {
        let doc = repo::find_document_by_path(&conn, path).unwrap().unwrap();
        assert_eq!(doc.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    }
    let partial = repo::find_document_by_path(&conn, &paths[0])
        .unwrap()
        .unwrap();
    assert!(
        partial
            .parse_error_message
            .unwrap()
            .contains("第 2 页识别失败")
    );
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
fn completion_accepts_done_or_complete_without_other_completion_fields() {
    for result in [
        json!({"status":"done","total_pages":3,"markdown":"完整正文"}),
        json!({"status":"done","complete":false,"total_pages":3,"markdown":"完整正文"}),
        json!({"complete":true,"total_pages":3,"markdown":"完整正文"}),
        // 服务端可调大页数上限，客户端不能再自行用 1000 页拒绝完整结果。
        json!({"status":"done","total_pages":1001,"markdown":"完整正文"}),
    ] {
        let (url, handle) = server(vec![result]);
        let parsed = rsou_lib::ocr::Client::new(&url)
            .unwrap()
            .parse_pdf(common::scanned_pdf_fixture(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(parsed.markdown, "完整正文");
        handle.join().unwrap();
    }
}

#[test]
fn page_limit_rejection_preserves_chinese_detail_and_continues_batch() {
    let dir = common::temp_dir("ocr-page-limit");
    let db = dir.join("index.db");
    let paths: Vec<_> = (0..2)
        .map(|n| {
            let path = dir.join(format!("{n}.pdf"));
            std::fs::write(&path, common::scanned_pdf_fixture()).unwrap();
            path
        })
        .collect();
    run(&db, paths.clone(), None);
    let detail = "PDF 共 1001 页，超过单次上限 1000 页，不予处理；请调大 OCR_MAX_PAGES 后重启服务";
    let (url, handle) = server_responses(vec![
        (
            "POST /api/ocr/pdf",
            "422 Unprocessable Entity",
            json!({"detail":detail}),
        ),
        // 超限提交不能轮询任务，也不能自动截断或再次分片提交。
        (
            "POST /api/ocr/pdf",
            "200 OK",
            json!({"job_id":"test-job","status":"pending","total_pages":1}),
        ),
        (
            "GET /api/ocr/job/test-job?wait=2",
            "200 OK",
            json!({"status":"done","total_pages":1,"markdown":"下一份完整结果"}),
        ),
    ]);
    let counts = run(&db, paths.clone(), Some(url));
    handle.join().unwrap();
    assert_eq!((counts.total, counts.ok, counts.failed), (2, 1, 1));
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let rejected = repo::find_document_by_path(&conn, &paths[0])
        .unwrap()
        .unwrap();
    assert_eq!(rejected.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    assert!(rejected.parse_error_message.unwrap().contains(detail));
    assert!(repo::get_plain_text(&conn, rejected.id).unwrap().is_none());
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap(),
        1
    );
}

#[test]
fn rejection_with_non_string_detail_has_a_chinese_fallback() {
    let (url, handle) = server_responses(vec![(
        "POST /api/ocr/pdf",
        "422 Unprocessable Entity",
        json!({"detail":[{"msg":"invalid file"}]}),
    )]);
    let error = rsou_lib::ocr::Client::new(&url)
        .unwrap()
        .parse_pdf(common::scanned_pdf_fixture(), &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(error.code, rsou_lib::parse::ParseErrorCode::NeedsOcr);
    assert!(error.message.contains("OCR 提交被拒绝"));
    assert!(error.detail.contains("422"));
    handle.join().unwrap();
}

#[test]
fn pending_results_are_polled_until_complete() {
    let (url, handle) = server_responses(vec![
        (
            "POST /api/ocr/pdf",
            "200 OK",
            json!({"job_id":"test-job","status":"pending","total_pages":2}),
        ),
        (
            "GET /api/ocr/job/test-job?wait=2",
            "200 OK",
            json!({"status":"pending","total_pages":2}),
        ),
        (
            "GET /api/ocr/job/test-job?wait=2",
            "200 OK",
            json!({"status":"running","complete":false,"total_pages":2,"markdown":"未完成"}),
        ),
        (
            "GET /api/ocr/job/test-job?wait=2",
            "200 OK",
            json!({"complete":true,"total_pages":2,"markdown":"最终正文"}),
        ),
    ]);
    let parsed = rsou_lib::ocr::Client::new(&url)
        .unwrap()
        .parse_pdf(common::scanned_pdf_fixture(), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(parsed.markdown, "最终正文");
    handle.join().unwrap();
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
