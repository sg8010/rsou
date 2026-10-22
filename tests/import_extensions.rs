//! 文件夹索引类型:扫描前过滤、持久化、更新与清理。
mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rsou_lib::import::{ImportCounts, ImportEvent, ImportOptions};
use rsou_lib::{import, maintain, repo, store};

fn selection(extensions: &[&str]) -> BTreeSet<String> {
    extensions.iter().map(|ext| (*ext).to_owned()).collect()
}

fn options(extensions: &[&str]) -> ImportOptions {
    ImportOptions {
        extensions: Some(selection(extensions)),
        ..Default::default()
    }
}

fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    let dir = common::temp_dir(tag);
    let docs = dir.join("资料");
    std::fs::create_dir_all(docs.join("子目录")).unwrap();
    for (name, content) in [
        ("保留.TXT", "正文甲"),
        ("子目录/保留.txt", "正文乙"),
        ("笔记.md", "# 笔记正文"),
        ("坏文件.pdf", "不是有效 PDF,未勾选时不应解析或报错"),
        ("未知.xyz", "不支持的格式"),
        (".隐藏.txt", "不应扫描"),
    ] {
        std::fs::write(docs.join(name), content).unwrap();
    }
    (dir.join("index.sqlite3"), docs)
}

fn run(db: &Path, input: &Path, options: ImportOptions) -> ImportCounts {
    let mut scanned = None;
    let counts = import::run_import(
        db,
        vec![input.to_path_buf()],
        options,
        Arc::new(AtomicBool::new(false)),
        &mut |event| {
            if let ImportEvent::Scanned { total } = event {
                scanned = Some(total);
            }
        },
    )
    .unwrap();
    assert_eq!(scanned, Some(counts.total));
    counts
}

#[test]
fn selection_filters_before_parsing_and_counts_only_selected_files() {
    let (db, docs) = fixture("extension-scan");
    let counts = run(&db, &docs, options(&["TXT"]));
    assert_eq!(
        (counts.total, counts.ok, counts.failed, counts.skipped),
        (2, 2, 0, 0)
    );
    let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
    let documents = repo::list_documents(&conn).unwrap();
    assert_eq!(documents.len(), 2);
    let root = dunce::canonicalize(&docs)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        documents
            .iter()
            .all(|doc| doc.ext == "txt" && doc.source_root.as_deref() == Some(&root))
    );
    assert_eq!(
        repo::folder_import_extensions(&conn, &root).unwrap(),
        Some(selection(&["txt"]))
    );
    let items: i64 = conn
        .query_row("SELECT count(*) FROM import_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(items, 2);
}

#[test]
fn rescan_and_force_reimport_reuse_saved_selection_but_explicit_file_does_not() {
    let (db, docs) = fixture("extension-rescan");
    run(&db, &docs, options(&["txt"]));
    std::fs::write(docs.join("新增.txt"), "新增正文").unwrap();
    let counts = run(
        &db,
        &docs.join("子目录/.."),
        ImportOptions {
            detect_missing: true,
            skip_unchanged_failed: true,
            ..Default::default()
        },
    );
    assert_eq!((counts.total, counts.ok, counts.skipped), (3, 1, 2));
    let counts = run(
        &db,
        &docs,
        ImportOptions {
            force: true,
            ..Default::default()
        },
    );
    assert_eq!((counts.total, counts.ok, counts.failed), (3, 3, 0));
    let counts = run(&db, &docs.join("笔记.md"), ImportOptions::default());
    assert_eq!((counts.total, counts.ok), (1, 1));
    let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
    assert_eq!(repo::list_standalone_documents(&conn).unwrap().len(), 1);
}

#[test]
fn folders_without_preferences_keep_all_supported_formats() {
    let (db, docs) = fixture("extension-default");
    let counts = run(&db, &docs, ImportOptions::default());
    assert_eq!((counts.total, counts.ok, counts.failed), (4, 3, 1));
    // 另一个文件夹的筛选不会变成全局限制。
    let other = docs.parent().unwrap().join("其他");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("仅文本.txt"), "正文").unwrap();
    std::fs::write(other.join("笔记.md"), "笔记").unwrap();
    run(&db, &docs, options(&["txt"]));
    assert_eq!(run(&db, &other, ImportOptions::default()).total, 2);
}

#[test]
fn changing_selection_preserves_existing_documents_and_limits_missing_checks() {
    let (db, docs) = fixture("extension-change");
    run(&db, &docs, options(&["txt", "md"]));
    std::fs::remove_file(docs.join("笔记.md")).unwrap();
    std::fs::remove_file(docs.join("保留.TXT")).unwrap();
    let counts = run(
        &db,
        &docs,
        ImportOptions {
            detect_missing: true,
            ..options(&["txt"])
        },
    );
    assert_eq!((counts.total, counts.failed, counts.skipped), (2, 1, 1));
    let conn = store::open(&db, store::OpenMode::ReadOnly).unwrap();
    let documents = repo::list_documents(&conn).unwrap();
    assert_eq!(documents.len(), 3);
    assert_eq!(
        documents
            .iter()
            .find(|doc| doc.ext == "md")
            .unwrap()
            .parse_status,
        "parsed"
    );
    let failed = repo::list_failed_documents(&conn).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].file_name, "保留.TXT");
    assert_eq!(failed[0].parse_error_code.as_deref(), Some("FILE_MISSING"));
    drop(conn);
    // 再次添加同一文件夹可扩大选择,恢复 Markdown 的索引。
    std::fs::write(docs.join("笔记.md"), "新的笔记正文").unwrap();
    assert_eq!(run(&db, &docs, options(&["txt", "md"])).ok, 1);
}

#[test]
fn empty_or_unsupported_selection_fails_without_overwriting_preferences() {
    let (db, docs) = fixture("extension-invalid");
    run(&db, &docs, options(&["txt"]));
    for extensions in [vec![], vec!["xyz"]] {
        assert!(
            import::run_import(
                &db,
                vec![docs.clone()],
                options(&extensions),
                Arc::new(AtomicBool::new(false)),
                &mut |_| {},
            )
            .is_err()
        );
    }
    assert_eq!(run(&db, &docs, ImportOptions::default()).total, 2);
}

#[test]
fn no_matching_files_does_not_fall_back_to_all_formats() {
    let (db, docs) = fixture("extension-no-match");
    assert_eq!(run(&db, &docs, options(&["docx"])).total, 0);
    assert_eq!(run(&db, &docs, ImportOptions::default()).total, 0);
    std::fs::write(docs.join("新文档.docx"), common::docx_fixture()).unwrap();
    assert_eq!(run(&db, &docs, ImportOptions::default()).ok, 1);
}

#[test]
fn removal_and_clear_delete_folder_preferences_but_keep_global_settings() {
    let mut conn = store::open_in_memory().unwrap();
    for root in ["/a", "/a-b"] {
        repo::set_folder_import_extensions(&conn, root, &selection(&["txt"])).unwrap();
    }
    repo::set_setting(&conn, "max_file_mb", "128").unwrap();
    repo::delete_source_root(&mut conn, "/a").unwrap();
    assert!(
        repo::folder_import_extensions(&conn, "/a")
            .unwrap()
            .is_none()
    );
    assert!(
        repo::folder_import_extensions(&conn, "/a-b")
            .unwrap()
            .is_some()
    );
    maintain::clear_all(&mut conn).unwrap();
    assert!(
        repo::folder_import_extensions(&conn, "/a-b")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo::get_setting(&conn, "max_file_mb").unwrap().as_deref(),
        Some("128")
    );
}
