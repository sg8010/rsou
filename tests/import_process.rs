mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rsou_lib::import::{ImportEvent, ImportOptions};
use rsou_lib::{import_process, maintain, repo, store};

#[test]
fn isolated_import_reports_failures_and_preserves_options() {
    let dir = common::temp_dir("isolated-import");
    let docs = dir.join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("正常.txt"), "可检索正文").unwrap();
    std::fs::write(docs.join("扫描.pdf"), common::scanned_pdf_fixture()).unwrap();
    std::fs::write(docs.join("忽略.md"), "不导入").unwrap();
    let db = dir.join("index.db");
    let mut terminal = None;
    let counts = import_process::run_import(
        Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
        &db,
        vec![docs.clone()],
        ImportOptions {
            extensions: Some(["txt".into(), "pdf".into()].into()),
            ..Default::default()
        },
        Arc::new(AtomicBool::new(false)),
        &mut |event| {
            if let ImportEvent::Finished {
                cancelled, run_id, ..
            } = event
            {
                assert!(!cancelled);
                terminal = Some(run_id);
            }
        },
    )
    .unwrap();
    assert!(terminal.is_some());
    assert_eq!(
        (counts.total, counts.processed, counts.ok, counts.failed),
        (2, 2, 1, 1)
    );
    let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
    let source: (String, Option<String>) = conn
        .query_row(
            "SELECT kind, root_path FROM import_runs WHERE id = ?1",
            [terminal.unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        source,
        ("folder".into(), Some(docs.to_string_lossy().into_owned()))
    );
    assert!(
        repo::find_document_by_path(&conn, &docs.join("忽略.md"))
            .unwrap()
            .is_none()
    );
    let scanned = repo::find_document_by_path(&conn, &docs.join("扫描.pdf"))
        .unwrap()
        .unwrap();
    assert_eq!(scanned.source_root.as_deref(), docs.to_str());
    assert_eq!(scanned.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    assert!(!scanned.parse_error_message.unwrap().contains('['));
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}

#[test]
fn isolated_folder_settings_add_remove_and_clear_formats() {
    let dir = common::temp_dir("isolated-folder-settings");
    let docs = dir.join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("文本.TXT"), "文本正文").unwrap();
    std::fs::write(docs.join("笔记.md"), "笔记正文").unwrap();
    let db = dir.join("index.db");
    for (extensions, sync, expected_ok, expected_documents) in [
        (vec!["txt"], false, 1, 1),
        (vec!["md"], true, 1, 1),
        (vec![], true, 0, 0),
    ] {
        let selected = extensions.into_iter().map(str::to_owned).collect();
        let counts = import_process::run_import(
            Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
            &db,
            vec![docs.clone()],
            ImportOptions {
                extensions: Some(selected),
                sync_folder_extensions: sync,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(counts.ok, expected_ok);
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        let snapshot = repo::library_snapshot(&conn).unwrap();
        assert_eq!(snapshot.folders.len(), 1);
        assert_eq!(snapshot.folders[0].total, expected_documents);
        assert_eq!(
            repo::list_documents(&conn).unwrap().len() as u64,
            expected_documents
        );
        assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
    }
    assert!(docs.join("文本.TXT").exists());
    assert!(docs.join("笔记.md").exists());
}

#[test]
fn isolated_cancel_request_finishes_and_preserves_committed_counts() {
    let dir = common::temp_dir("isolated-cancel");
    let path = dir.join("a.txt");
    std::fs::write(&path, "正文").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut finished = 0;
    let counts = import_process::run_import(
        Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
        &dir.join("index.db"),
        vec![path],
        ImportOptions::default(),
        Arc::clone(&cancel),
        &mut |event| match event {
            ImportEvent::Scanned { .. } => cancel.store(true, Ordering::Relaxed),
            ImportEvent::Finished {
                cancelled, counts, ..
            } => {
                // IPC 送达前也可能已完成整个小批次,此时正常完成同样有效。
                assert!(cancelled || counts.processed == counts.total);
                finished += 1;
            }
            _ => {}
        },
    )
    .unwrap();
    assert_eq!(counts.total, 1);
    assert_eq!(finished, 1);
}

#[test]
fn cancellation_stops_an_ocr_request_that_never_responds() {
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let dir = common::temp_dir("isolated-stalled-ocr");
    let path = dir.join("扫描.pdf");
    std::fs::write(&path, common::scanned_pdf_fixture()).unwrap();
    let db = dir.join("index.db");
    rsou_lib::import::run_import(
        &db,
        vec![path.clone()],
        ImportOptions::default(),
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let (release, wait) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // TCP 已连接但永远没有响应,模拟不可中断的 HTTP 调用。
                    flag.store(true, Ordering::Relaxed);
                    let _ = wait.recv_timeout(Duration::from_secs(6));
                    drop(stream);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "OCR 任务未连接测试服务");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let start = Instant::now();
    let mut cancelled = false;
    let result = import_process::run_import(
        Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
        &db,
        vec![path.clone()],
        ImportOptions {
            ocr_base_url: Some(url),
            ..Default::default()
        },
        cancel,
        &mut |event| {
            if let ImportEvent::Finished {
                cancelled: value, ..
            } = event
            {
                cancelled = value;
            }
        },
    );
    let elapsed = start.elapsed();
    let _ = release.send(());
    server.join().unwrap();
    let counts = result.unwrap();
    assert!(cancelled);
    assert!(elapsed < Duration::from_secs(5));
    assert_eq!((counts.total, counts.processed), (1, 0));
    let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
    let scanned = repo::find_document_by_path(&conn, &path).unwrap().unwrap();
    assert_eq!(scanned.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}

#[test]
fn isolated_worker_errors_do_not_turn_into_successful_cancellation() {
    let dir = common::temp_dir("isolated-invalid-options");
    let path = dir.join("a.txt");
    std::fs::write(&path, "正文").unwrap();
    let db = dir.join("index.db");
    let error = import_process::run_import(
        Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
        &db,
        vec![path],
        ImportOptions {
            extensions: Some(Default::default()),
            ..Default::default()
        },
        Arc::new(AtomicBool::new(false)),
        &mut |event| assert!(!matches!(event, ImportEvent::Finished { .. })),
    )
    .unwrap_err();
    assert!(error.to_string().contains("请至少选择一种"));
    let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
    let status: String = conn
        .query_row("SELECT status FROM import_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "failed");
}

#[cfg(unix)]
#[test]
fn isolated_import_supports_non_unicode_database_input_and_event_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = common::temp_dir("isolated-non-unicode");
    let docs = dir.join("docs");
    std::fs::create_dir(&docs).unwrap();
    let path = docs.join(OsString::from_vec(b"\xff.txt".to_vec()));
    std::fs::write(&path, "无损路径正文").unwrap();
    // 分别覆盖扫描得到的非 Unicode 路径、请求中直接传入的非 Unicode 路径。
    for (i, input) in [docs, path.clone()].into_iter().enumerate() {
        let db = dir.join(OsString::from_vec(
            format!("{i}-")
                .bytes()
                .chain(b"\xfe.db".iter().copied())
                .collect(),
        ));
        let mut reported_path = None;
        let mut finished = false;
        let counts = import_process::run_import(
            Path::new(env!("CARGO_BIN_EXE_rsou-cli")),
            &db,
            vec![input],
            ImportOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |event| match event {
                ImportEvent::FileDone { path, .. } => reported_path = Some(path),
                ImportEvent::Finished { cancelled, .. } => {
                    assert!(!cancelled);
                    finished = true;
                }
                _ => {}
            },
        )
        .unwrap();
        assert!(finished);
        assert_eq!((counts.total, counts.ok), (1, 1));
        assert_eq!(reported_path.as_deref(), Some(path.as_path()));
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
    }
}

#[test]
fn worker_exits_after_parent_pipe_closes_even_when_ocr_is_stuck() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    for cancel_first in [false, true] {
        let dir = common::temp_dir("isolated-orphan");
        let path = dir.join("扫描.pdf");
        let db = dir.join("index.db");
        std::fs::write(&path, common::scanned_pdf_fixture()).unwrap();
        rsou_lib::import::run_import(
            &db,
            vec![path.clone()],
            ImportOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
        .unwrap();
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        let run_id = repo::create_run(&conn, "pending", None, 0, repo::now_ms()).unwrap();
        drop(conn);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let request = serde_json::json!({
            "db_path": db, "inputs": [path], "run_id": run_id,
            "options": ImportOptions {
                ocr_base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
                ..Default::default()
            }
        });
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_rsou-cli"))
                .arg("--rsou-import-worker")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        serde_json::to_writer(&mut input, &request).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "OCR 未连接测试服务");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        if cancel_first {
            input.write_all(b"cancel\n").unwrap();
            input.flush().unwrap();
            // 正常取消不能误判为父进程死亡;父进程仍持有管道时由父进程负责强杀。
            std::thread::sleep(Duration::from_millis(1200));
            assert!(child.0.try_wait().unwrap().is_none());
        }
        drop(input); // 模拟父进程退出关闭管道,HTTP 连接继续保持阻塞。
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "父进程消失后不能残留导入进程");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        drop(stream);
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
        let doc = repo::find_document_by_path(&conn, &path).unwrap().unwrap();
        assert_eq!(doc.parse_error_code.as_deref(), Some("NEEDS_OCR"));
    }
}
