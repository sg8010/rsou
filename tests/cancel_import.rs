//! 中途取消:以数据库已提交状态为准,覆盖进度回放、终态及再次导入。
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rsou_lib::import::{ImportEvent, ImportOptions, run_import};
use rsou_lib::{maintain, store};

#[test]
fn cancel_after_first_commit_preserves_only_committed_data_and_can_resume() {
    for force in [false, true] {
        let dir = common::temp_dir("cancel-mid-import");
        let docs = dir.join("docs");
        std::fs::create_dir(&docs).unwrap();
        for i in 0..128 {
            // 同时覆盖内容复用和不同内容的并行处理。
            std::fs::write(
                docs.join(format!("{i:03}.txt")),
                format!("# 标题\n正文 {}", i % 16),
            )
            .unwrap();
        }
        let db = dir.join("index.db");
        let cancel = Arc::new(AtomicBool::new(false));
        let mut committed_at_cancel = None;
        let mut file_events = 0;
        let mut finished = None;
        let counts = run_import(
            &db,
            vec![docs.clone()],
            ImportOptions {
                force,
                ..Default::default()
            },
            Arc::clone(&cancel),
            &mut |event| match event {
                ImportEvent::FileDone { .. } => {
                    file_events += 1;
                    if committed_at_cancel.is_none() {
                        let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
                        let committed: usize = conn
                            .query_row("SELECT count(*) FROM documents", [], |r| {
                                r.get::<_, i64>(0).map(|n| n as usize)
                            })
                            .unwrap();
                        committed_at_cancel = Some(committed);
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                ImportEvent::Finished {
                    run_id,
                    counts,
                    cancelled,
                } => {
                    assert!(finished.is_none(), "只能发一次终态");
                    assert!(cancelled);
                    finished = Some((run_id, counts.processed));
                }
                ImportEvent::Error { message } => panic!("取消不能当作失败: {message}"),
                _ => {}
            },
        )
        .unwrap();
        let committed = committed_at_cancel.unwrap();
        assert!(committed > 0 && committed < counts.total);
        assert_eq!(
            (counts.total, counts.processed, counts.ok, counts.failed),
            (128, committed, committed, 0)
        );
        // 同批事件可以晚于取消回放,但不能多于取消时已经提交的记录。
        assert_eq!(file_events, committed);
        let (id, processed) = finished.unwrap();
        assert_eq!(processed, committed);
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        let history: (String, i64, i64, bool) = conn.query_row(
            "SELECT status, total_files, processed_files, finished_at IS NOT NULL FROM import_runs WHERE id = ?1",
            [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(history, ("cancelled".into(), 128, committed as i64, true));
        let stored: usize = conn
            .query_row("SELECT count(*) FROM documents", [], |r| {
                r.get::<_, i64>(0).map(|n| n as usize)
            })
            .unwrap();
        assert_eq!(stored, committed);
        assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
        drop(conn);

        let resumed = run_import(
            &db,
            vec![docs],
            ImportOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            (resumed.processed, resumed.failed, resumed.skipped),
            (128, 0, committed)
        );
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        let stored: usize = conn
            .query_row("SELECT count(*) FROM documents", [], |r| {
                r.get::<_, i64>(0).map(|n| n as usize)
            })
            .unwrap();
        assert_eq!(stored, 128);
        assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
    }
}

#[test]
fn cancel_after_scan_skips_pending_items_and_finishes_as_cancelled() {
    let dir = common::temp_dir("cancel-after-scan");
    let file = dir.join("a.txt");
    std::fs::write(&file, "正文").unwrap();
    let db = dir.join("index.db");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut scanned = false;
    let mut finished = false;
    let counts = run_import(
        &db,
        vec![file],
        ImportOptions::default(),
        Arc::clone(&cancel),
        &mut |event| match event {
            ImportEvent::Scanning { .. } => {}
            ImportEvent::Scanned { total } => {
                assert_eq!(total, 1);
                scanned = true;
                cancel.store(true, Ordering::Relaxed);
            }
            ImportEvent::Finished { cancelled, .. } => finished = cancelled,
            _ => panic!("扫描后取消不应处理文件或报告错误"),
        },
    )
    .unwrap();
    assert!(scanned && finished);
    assert_eq!((counts.total, counts.processed), (1, 0));
    let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
    for table in ["documents", "import_items"] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
    let status: String = conn
        .query_row("SELECT status FROM import_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "cancelled");
}
