//! 原文件不存在:范围核对、保留正文、恢复和安全清除。
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use import::{ImportCounts, ImportEvent, ImportOptions};
use rsou_lib::{import, maintain, repo, store};
use store::OpenMode;

fn rescan_options() -> ImportOptions {
    ImportOptions {
        detect_missing: true,
        skip_unchanged_failed: true,
        preserve_source_root: true,
        ..Default::default()
    }
}

fn run(db: &Path, input: &Path, options: ImportOptions) -> anyhow::Result<ImportCounts> {
    import::run_import(
        db,
        vec![input.to_path_buf()],
        options,
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
}

fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = common::temp_dir(tag);
    let db = dir.join("index.sqlite3");
    let docs = dir.join("资料");
    std::fs::create_dir(&docs).unwrap();
    let file = docs.join("测试.txt");
    std::fs::write(&file, "用于核对删除恢复的正文").unwrap();
    run(&db, &docs, ImportOptions::default()).unwrap();
    (db, docs, file)
}

#[test]
fn missing_marker_retains_body_and_restoration_reindexes_identical_metadata() {
    let (db, docs, file) = fixture("missing-restore");
    let mut conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    let before = repo::list_documents(&conn).unwrap().remove(0);
    let body = repo::get_plain_text(&conn, before.id).unwrap();
    let chunks: i64 = conn
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::fs::remove_file(&file).unwrap();
    let counts = run(&db, &docs, rescan_options()).unwrap();
    assert_eq!((counts.total, counts.failed, counts.processed), (1, 1, 1));
    let marked = repo::list_failed_documents(&conn).unwrap().remove(0);
    assert_eq!(marked.id, before.id);
    assert_eq!(marked.parse_error_code.as_deref(), Some("FILE_MISSING"));
    assert_eq!(marked.source_root, before.source_root);
    assert_eq!(marked.file_size, before.file_size);
    assert_eq!(repo::get_plain_text(&conn, before.id).unwrap(), body);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .unwrap(),
        chunks
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
    maintain::rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());

    std::fs::write(&file, "用于核对删除恢复的正文").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    // 大小、mtime、内容哈希全都相同，也必须解除不存在标记并重新建索引。
    let counts = run(&db, &docs, rescan_options()).unwrap();
    assert_eq!((counts.ok, counts.skipped), (1, 0));
    assert!(repo::list_failed_documents(&conn).unwrap().is_empty());
    let restored = repo::list_documents(&conn).unwrap().remove(0);
    assert_eq!(
        (restored.id, restored.source_root),
        (before.id, before.source_root)
    );
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}

#[test]
fn missing_check_is_scoped_and_single_file_update_works() {
    let (db, docs, file) = fixture("missing-scope");
    let other = docs.parent().unwrap().join("其他.txt");
    std::fs::write(&other, "其他来源正文").unwrap();
    run(&db, &other, ImportOptions::default()).unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_file(&other).unwrap();
    run(&db, &docs, rescan_options()).unwrap();
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let failed = repo::list_failed_documents(&conn).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].path, file.to_string_lossy());
    let counts = run(&db, &other, rescan_options()).unwrap();
    assert_eq!((counts.total, counts.failed), (1, 1));
    assert_eq!(repo::list_failed_documents(&conn).unwrap().len(), 2);
}

#[test]
fn unavailable_root_does_not_mark_or_clear_documents() {
    let (db, docs, file) = fixture("missing-unavailable");
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&docs).unwrap();
    assert!(run(&db, &docs, rescan_options()).is_err());
    let mut conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    assert!(repo::list_failed_documents(&conn).unwrap().is_empty());
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
    std::fs::create_dir(&docs).unwrap();
    run(&db, &docs, rescan_options()).unwrap();
    std::fs::remove_dir(&docs).unwrap();
    assert_eq!(repo::delete_missing_documents(&mut conn).unwrap(), (0, 1));
    assert_eq!(repo::list_failed_documents(&conn).unwrap().len(), 1);
}

#[test]
fn bulk_clear_keeps_restored_files_and_other_failures() {
    let (db, docs, file) = fixture("missing-clear");
    let restored = docs.join("已恢复.txt");
    let failed = docs.join("解析失败.txt");
    std::fs::write(&restored, "恢复后不能清除").unwrap();
    std::fs::write(&failed, "").unwrap();
    run(&db, &docs, ImportOptions::default()).unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_file(&restored).unwrap();
    run(&db, &docs, rescan_options()).unwrap();
    std::fs::write(&restored, "恢复后不能清除").unwrap();
    let mut conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    let removed_id = repo::find_document_by_path(&conn, &file)
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(repo::delete_missing_documents(&mut conn).unwrap(), (1, 1));
    assert_eq!(repo::list_documents(&conn).unwrap().len(), 2);
    assert!(repo::get_plain_text(&conn, removed_id).unwrap().is_none());
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM chunks WHERE document_id = ?1",
            [removed_id],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert!(restored.is_file());
    assert!(failed.is_file());
    assert_eq!(repo::list_failed_documents(&conn).unwrap().len(), 2);
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}

#[test]
fn bulk_clear_rolls_back_on_write_error() {
    let (db, docs, file) = fixture("missing-rollback");
    std::fs::remove_file(file).unwrap();
    run(&db, &docs, rescan_options()).unwrap();
    let mut conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    let id = repo::list_failed_documents(&conn).unwrap()[0].id;
    conn.execute_batch("CREATE TEMP TRIGGER refuse_delete BEFORE DELETE ON documents BEGIN SELECT RAISE(ABORT, '注入删除失败'); END;").unwrap();
    assert!(repo::delete_missing_documents(&mut conn).is_err());
    assert!(conn.is_autocommit());
    assert_eq!(repo::list_failed_documents(&conn).unwrap().len(), 1);
    assert!(repo::get_plain_text(&conn, id).unwrap().is_some());
}

#[test]
fn cancellation_and_plain_import_do_not_mark_missing_files() {
    let (db, docs, file) = fixture("missing-cancel");
    std::fs::remove_file(&file).unwrap();
    run(&db, &docs, ImportOptions::default()).unwrap();
    let mut finished = false;
    import::run_import(
        &db,
        vec![docs],
        rescan_options(),
        Arc::new(AtomicBool::new(true)),
        &mut |event| {
            if let ImportEvent::Finished { cancelled, .. } = event {
                finished = cancelled;
            }
        },
    )
    .unwrap();
    assert!(finished);
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    assert!(repo::list_failed_documents(&conn).unwrap().is_empty());
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}

#[test]
fn non_not_found_errors_are_not_missing() {
    let dir = common::temp_dir("missing-errors");
    let file = dir.join("不是目录");
    std::fs::write(&file, "普通文件").unwrap();
    // 含 NUL 的路径在各平台都属于非法路径，而不是 NotFound。
    assert!(repo::original_file_missing(&dir.join("非法\0路径.txt"), None).is_err());
    assert!(!repo::original_file_missing(&file, None).unwrap());
}

#[cfg(unix)]
#[test]
fn permission_failure_is_not_missing() {
    use std::os::unix::fs::PermissionsExt;
    let (db, docs, file) = fixture("missing-permission");
    std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o0)).unwrap();
    let checked = repo::original_file_missing(&file, None);
    let result = run(&db, &docs, rescan_options());
    std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o700)).unwrap();
    // root 运行测试时可以越过权限;无论哪种身份，都不能误标。
    if checked.is_err() {
        assert!(result.is_err());
    }
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    assert!(repo::list_failed_documents(&conn).unwrap().is_empty());
    assert!(maintain::check_integrity(&conn).unwrap().is_consistent());
}
