//! 端到端:扫描 → 导入 → 跳过 → 重解析 → 删除,验证 FTS 与 chunk 边界不变式。

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rsou_lib::import::{self, FileOutcome, ImportEvent, ImportOptions};
use rsou_lib::repo;
use rsou_lib::store::{self, OpenMode};

/// 跑一次导入,收集全部事件,返回 (counts, outcomes)。
fn run(
    dir_db: &Path,
    inputs: Vec<PathBuf>,
    force: bool,
) -> (import::ImportCounts, Vec<(PathBuf, String)>) {
    run_with_options(
        dir_db,
        inputs,
        ImportOptions {
            force,
            ..Default::default()
        },
    )
}

fn run_with_options(
    dir_db: &Path,
    inputs: Vec<PathBuf>,
    options: ImportOptions,
) -> (import::ImportCounts, Vec<(PathBuf, String)>) {
    let mut outcomes = Vec::new();
    let counts = import::run_import(
        dir_db,
        inputs,
        options,
        Arc::new(AtomicBool::new(false)),
        &mut |event| {
            if let ImportEvent::FileDone {
                path,
                outcome,
                counts: _,
            } = event
            {
                let tag = match outcome {
                    FileOutcome::Ok { .. } => "ok",
                    FileOutcome::Failed { .. } => "failed",
                    FileOutcome::Skipped => "skipped",
                };
                outcomes.push((path, tag.to_owned()));
            }
        },
    )
    .expect("导入应成功");
    (counts, outcomes)
}

/// 验证索引不变式(文档级 contentless-delete FTS):
/// - 每篇已解析文档恰好一行 FTS:`rowid == documents.id` 且双向差集为空
///   (FTS 不存列值,词元正确性由检索行为/重建覆盖);
/// - 展示分块 `chunks` 的区间落在 `plain_text` 内且与原文逐字节一致、互不重叠。
fn assert_fts_matches_plain(conn: &rusqlite::Connection) {
    // FTS 行数 == 已解析文档数,且双向差集为空。
    let parsed: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents WHERE parse_status = 'parsed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(parsed > 0, "应至少有一篇已索引文档");
    let fts: i64 = conn
        .query_row("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts, parsed);
    let missing: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents d WHERE d.parse_status = 'parsed' \
             AND NOT EXISTS(SELECT 1 FROM documents_fts f WHERE f.rowid = d.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let orphan: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents_fts f \
             WHERE NOT EXISTS(\
                 SELECT 1 FROM documents d \
                 WHERE d.id = f.rowid AND d.parse_status = 'parsed')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((missing, orphan), (0, 0), "FTS 与 documents 的行集合应对齐");

    // 展示分块的偏移不变式:区间落在 plain_text 内、与原文一致、单调不重叠。
    let mut stmt = conn
        .prepare(
            "SELECT c.document_id, c.start_offset, c.end_offset, dc.plain_text \
             FROM chunks c JOIN document_contents dc ON dc.document_id = c.document_id \
             ORDER BY c.document_id, c.chunk_index",
        )
        .unwrap();
    let chunks = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!chunks.is_empty(), "应至少有一个展示分块");
    let mut current_doc = None;
    let mut prev_end = 0i64;
    for (document_id, start, end, plain) in &chunks {
        if current_doc != Some(*document_id) {
            current_doc = Some(*document_id);
            prev_end = 0;
        }
        assert!(*start >= prev_end, "分块区间不应重叠");
        assert!(*end > *start, "分块区间应非空");
        assert!(*end as usize <= plain.len(), "分块区间不应越界");
        assert!(plain.is_char_boundary(*start as usize));
        assert!(plain.is_char_boundary(*end as usize));
        prev_end = *end;
    }
}

#[test]
fn folder_management_snapshot_keeps_summaries_failures_and_searchable_contents() {
    let dir = common::temp_dir("e2e-folder-summary");
    let db = dir.join("index.sqlite3");
    let folder = dir.join("资料");
    std::fs::create_dir_all(folder.join("子目录")).unwrap();
    std::fs::write(folder.join("甲.txt"), "文件夹汇总验证").unwrap();
    std::fs::write(folder.join("子目录/乙.txt"), "子文件仍能检索").unwrap();
    let failed = folder.join("空文件.txt");
    std::fs::write(&failed, "").unwrap();
    let (counts, _) = run(&db, vec![folder.clone()], false);
    assert_eq!((counts.total, counts.ok, counts.failed), (3, 2, 1));

    let standalone = dir.join("单独文件.txt");
    std::fs::write(&standalone, "单独添加的正文").unwrap();
    assert_eq!(run(&db, vec![standalone], false).0.ok, 1);
    let root = dunce::canonicalize(&folder)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let snapshot = repo::library_snapshot(&conn).unwrap();
    assert_eq!(snapshot.standalone.len(), 1);
    assert_eq!(snapshot.standalone[0].file_name, "单独文件.txt");
    assert_eq!(snapshot.failed.len(), 1);
    assert_eq!(
        snapshot.failed[0].source_root.as_deref(),
        Some(root.as_str())
    );
    assert_eq!(
        snapshot.folders,
        vec![repo::SourceRootSummary {
            root: root.clone(),
            total: 3,
            parsed: 2,
            failed: 1,
        }]
    );
    assert!(conn.is_autocommit(), "管理快照不得长期占用 WAL 读事务");
    let matches: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"汇总\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(matches, 1, "文件夹成功文档不预载到 GUI 也必须可检索");

    std::fs::write(&failed, "修复后的正文").unwrap();
    let (counts, _) = run_with_options(
        &db,
        vec![failed],
        ImportOptions {
            preserve_source_root: true,
            ..Default::default()
        },
    );
    assert_eq!(counts.ok, 1);
    let snapshot = repo::library_snapshot(&conn).unwrap();
    assert!(snapshot.failed.is_empty());
    assert_eq!(snapshot.folders[0].parsed, 3);
    assert_eq!(snapshot.folders[0].failed, 0);
    assert_eq!(run(&db, vec![folder], true).0.ok, 3);

    let mut writer = store::open(&db, OpenMode::ReadWrite).unwrap();
    assert_eq!(repo::delete_source_root(&mut writer, &root).unwrap(), 3);
    let snapshot = repo::library_snapshot(&conn).unwrap();
    assert!(snapshot.folders.is_empty());
    assert!(snapshot.failed.is_empty());
    assert_eq!(snapshot.standalone.len(), 1);
    assert_fts_matches_plain(&conn);
}

#[test]
fn unchanged_content_with_new_mtime_is_skipped() {
    let dir = common::temp_dir("e2e-mtime");
    let db_path = dir.join("index.sqlite3");
    let docs_dir = dir.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    let file = docs_dir.join("笔记.txt");
    let bytes = common::txt_utf8_fixture();
    std::fs::write(&file, &bytes).unwrap();

    let (counts, _) = run(&db_path, vec![docs_dir.clone()], false);
    assert_eq!(counts.ok, 1);

    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    let canonical = file.canonicalize().unwrap();
    let before = repo::find_document_by_path(&conn, &canonical)
        .unwrap()
        .unwrap();
    drop(conn);

    // 显式改 mtime、内容不变,应命中哈希二级跳过。
    let new_mtime = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_millis((before.file_mtime_ms + 5_000) as u64);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(new_mtime)
        .unwrap();
    let new_mtime_ms = std::fs::metadata(&file)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(new_mtime_ms != before.file_mtime_ms, "mtime 应已变化");

    let (counts, _) = run(&db_path, vec![docs_dir.clone()], false);
    assert_eq!(counts.skipped, 1);
    assert_eq!(counts.ok, 0);

    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    let after = repo::find_document_by_path(&conn, &canonical)
        .unwrap()
        .unwrap();
    assert_eq!(after.indexed_at, before.indexed_at, "indexed_at 不应变化");
    assert_eq!(after.updated_at, before.updated_at, "updated_at 不应变化");
    assert_eq!(after.file_mtime_ms, new_mtime_ms, "file_mtime_ms 应已更新");
}

#[test]
fn many_small_files_import_in_batches() {
    let dir = common::temp_dir("e2e-batch");
    let db_path = dir.join("index.sqlite3");
    let docs_dir = dir.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    for i in 0..150 {
        std::fs::write(
            docs_dir.join(format!("第{i}篇.txt")),
            format!("第 {i} 篇\n\n正文 {i}"),
        )
        .unwrap();
    }

    // 每个 FileDone 都带消费该条时的计数快照;批量写库下事件在事务提交后回放。
    let mut progressions = Vec::new();
    let counts = import::run_import(
        &db_path,
        vec![docs_dir.clone()],
        ImportOptions::default(),
        Arc::new(AtomicBool::new(false)),
        &mut |event| {
            if let ImportEvent::FileDone { counts, .. } = event {
                progressions.push(counts.processed);
            }
        },
    )
    .expect("导入应成功");
    assert_eq!(counts.ok, 150);
    assert_eq!(counts.processed, 150);
    assert_eq!(progressions.len(), 150);
    for pair in progressions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "processed 应严格单调递增: {progressions:?}"
        );
    }
    assert_eq!(progressions.last().copied(), Some(150));

    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    let documents: i64 = conn
        .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(documents, 150);
    let ok_items: i64 = conn
        .query_row(
            "SELECT count(*) FROM import_items WHERE status = 'ok'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ok_items, 150);
}

#[test]
fn import_skip_reimport_and_delete() {
    let dir = common::temp_dir("e2e");
    let db_path = dir.join("index.sqlite3");
    let docs_dir = dir.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();

    // 一个 docx、一个 txt、一个 csv、一个不支持文件、一个扫描 pdf
    std::fs::write(docs_dir.join("说明.docx"), common::docx_fixture()).unwrap();
    std::fs::write(docs_dir.join("笔记.txt"), common::txt_utf8_fixture()).unwrap();
    std::fs::write(docs_dir.join("数据.csv"), common::csv_fixture()).unwrap();
    std::fs::write(docs_dir.join("忽略.xyz"), b"not supported").unwrap();
    std::fs::write(docs_dir.join("扫描.pdf"), common::scanned_pdf_fixture()).unwrap();
    // 隐藏文件与黑名单目录不进扫描
    std::fs::write(docs_dir.join(".隐藏.txt"), b"hidden").unwrap();
    let nm = docs_dir.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("包内.txt"), b"dep").unwrap();

    // ---------- 首次导入:3 成功 + 1 失败(扫描 pdf 需要 OCR) ----------
    let (counts, outcomes) = run(&db_path, vec![docs_dir.clone()], false);
    assert_eq!(counts.total, 4, "xyz/隐藏/node_modules 不应被收进");
    assert_eq!(counts.ok, 3);
    assert_eq!(counts.failed, 1);
    let failed = outcomes.iter().find(|(_, tag)| tag == "failed");
    assert!(failed.is_some_and(|(p, _)| p.ends_with("扫描.pdf")));

    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    assert_fts_matches_plain(&conn);
    // 失败文档落库为 failed + 中文原因
    let stats = repo::stats(&conn).unwrap();
    assert_eq!(stats.documents, 4);
    assert_eq!(stats.failed, 1);
    drop(conn);

    // ---------- 再次导入:已索引的全部跳过,失败文档重新失败 ----------
    let (counts, _) = run(&db_path, vec![docs_dir.clone()], false);
    assert_eq!(counts.ok, 0);
    assert_eq!(counts.skipped, 3);
    assert_eq!(counts.failed, 1, "失败文档不算未变更,会再失败一次");

    // ---------- 修改 txt 后再导入:只有它重新解析 ----------
    std::fs::write(
        docs_dir.join("笔记.txt"),
        "# 阶段二文本\n\n修改后的正文,增加了新的内容行。\n",
    )
    .unwrap();
    // mtime 粒度可能是秒;写入前先等一拍保证 mtime/size 变化可见
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (counts, outcomes) = run(&db_path, vec![docs_dir.clone()], false);
    assert_eq!(counts.ok, 1);
    let txt_outcome = outcomes
        .iter()
        .find(|(p, _)| p.ends_with("笔记.txt"))
        .map(|(_, t)| t.as_str());
    assert_eq!(txt_outcome, Some("ok"));

    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    assert_fts_matches_plain(&conn);
    let plain = repo::get_plain_text(&conn, {
        repo::find_document_by_path(&conn, &docs_dir.join("笔记.txt").canonicalize().unwrap())
            .unwrap()
            .unwrap()
            .id
    })
    .unwrap()
    .unwrap();
    assert!(plain.contains("修改后的正文"));

    // ---------- 删除一篇:FTS/chunks/contents 无残留 ----------
    let doc =
        repo::find_document_by_path(&conn, &docs_dir.join("说明.docx").canonicalize().unwrap())
            .unwrap()
            .unwrap();
    drop(conn);
    let mut wconn = store::open(&db_path, OpenMode::ReadWrite).unwrap();
    repo::delete_document(&mut wconn, doc.id).unwrap();
    drop(wconn);
    let conn = store::open(&db_path, OpenMode::ReadOnly).unwrap();
    let leftover_fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents_fts WHERE rowid = ?1",
            [doc.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover_fts, 0);
    let leftover_chunks: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks WHERE document_id = ?1",
            [doc.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover_chunks, 0);
    let leftover_contents: i64 = conn
        .query_row(
            "SELECT count(*) FROM document_contents WHERE document_id = ?1",
            [doc.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover_contents, 0);
    let stats = repo::stats(&conn).unwrap();
    assert_eq!(stats.documents, 3);
    drop(conn);

    // ---------- force:全部重解析 ----------
    let (counts, _) = run(&db_path, vec![docs_dir.clone()], true);
    assert_eq!(counts.ok, 3, "剩 3 个可解析文件应全部重解析");
    assert_eq!(counts.skipped, 0);
}

#[test]
fn identical_files_keep_locations_titles_and_membership_and_force_bypasses_reuse() {
    let dir = common::temp_dir("e2e-identical-files");
    let db = dir.join("index.sqlite3");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    // 超过写库批次大小,覆盖尚未提交和已经提交的重复副本。
    for i in 0..80 {
        std::fs::write(docs.join(format!("副本{i}.txt")), "### 章节\n\n相同正文").unwrap();
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let (counts, events) = pool.install(|| run(&db, vec![docs.clone()], false));
    assert_eq!((counts.ok, counts.failed, counts.skipped), (80, 0, 0));
    assert_eq!(events.len(), 80);
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    let root = dunce::canonicalize(&docs)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let documents = repo::list_documents_by_source_root(&conn).unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].1.len(), 80);
    for document in &documents[0].1 {
        assert_eq!(document.title, document.file_name.trim_end_matches(".txt"));
        assert_eq!(document.source_root.as_deref(), Some(root.as_str()));
    }
    let hashes: i64 = conn
        .query_row(
            "SELECT count(DISTINCT content_hash) FROM documents",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hashes, 1);
    assert_fts_matches_plain(&conn);
    let (counts, _) = run(&db, vec![docs.clone()], false);
    assert_eq!((counts.ok, counts.skipped), (0, 80));

    // 测试探针:若复用了已有解析产物,副本会沿用这个解析器标记。
    conn.execute("UPDATE documents SET parser_name = 'reuse-probe'", [])
        .unwrap();
    let standalone = dir.join("单独副本.md");
    std::fs::copy(docs.join("副本0.txt"), &standalone).unwrap();
    let (counts, _) = run(&db, vec![standalone.clone()], false);
    assert_eq!(counts.ok, 1);
    let own = repo::find_document_by_path(&conn, &dunce::canonicalize(&standalone).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(own.title, "单独副本");
    assert_eq!(own.source_root, None);
    let parser = |id| {
        conn.query_row(
            "SELECT parser_name FROM documents WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    assert_eq!(parser(own.id), "reuse-probe", "跨批次也应复用正文");
    let (counts, _) = run(&db, vec![standalone.clone()], true);
    assert_eq!((counts.ok, counts.skipped), (1, 0));
    assert_eq!(parser(own.id), "text", "force 必须真正重新解析");

    let oversized = dir.join("超限副本.txt");
    std::fs::copy(&standalone, &oversized).unwrap();
    let (counts, _) = run_with_options(
        &db,
        vec![oversized],
        ImportOptions {
            max_file_bytes: 1,
            ..Default::default()
        },
    );
    assert_eq!(counts.failed, 1, "重复内容也不能绕过文件大小限制");
    assert_fts_matches_plain(&conn);
}

#[test]
fn identical_file_reuse_respects_markdown_setting_and_real_headings() {
    let dir = common::temp_dir("e2e-identical-markdown");
    let db = dir.join("index.sqlite3");
    let source = dir.join("正文标题.txt");
    std::fs::write(&source, "# 正文标题\n\n相同正文").unwrap();
    run(&db, vec![source.clone()], false);
    let conn = store::open(&db, OpenMode::ReadWrite).unwrap();
    repo::set_setting(&conn, "save_markdown", "1").unwrap();
    let copy = dir.join("另一个文件名.txt");
    std::fs::copy(&source, &copy).unwrap();
    let (counts, _) = run(&db, vec![copy.clone()], false);
    assert_eq!(counts.ok, 1);
    let doc = repo::find_document_by_path(&conn, &dunce::canonicalize(&copy).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.title, "正文标题",
        "正文标题即使恰好等于源文件名也不能被改写"
    );
    let markdown: String = conn
        .query_row(
            "SELECT markdown FROM document_contents WHERE document_id = ?1",
            [doc.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        markdown, "# 正文标题\n\n相同正文",
        "旧缓存没有 Markdown 时应回退解析"
    );

    // 大小相同但字节不同,不得视为相同内容。
    let different = dir.join("不同内容.txt");
    std::fs::write(&different, "# 正文标题\n\n不同正文").unwrap();
    run(&db, vec![different.clone()], false);
    let different_doc =
        repo::find_document_by_path(&conn, &dunce::canonicalize(&different).unwrap())
            .unwrap()
            .unwrap();
    assert!(
        repo::get_plain_text(&conn, different_doc.id)
            .unwrap()
            .unwrap()
            .contains("不同正文")
    );
    assert_fts_matches_plain(&conn);
}

#[test]
fn rescan_skips_unchanged_failures_and_imports_new_or_changed_files() {
    let dir = common::temp_dir("e2e-rescan");
    let db = dir.join("index.sqlite3");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let good = docs.join("正常.txt");
    let bad = docs.join("失败.txt");
    std::fs::write(&good, "原始正文").unwrap();
    std::fs::write(&bad, "").unwrap();
    let (counts, _) = run(&db, vec![docs.clone()], false);
    assert_eq!((counts.ok, counts.failed), (1, 1));
    let options = ImportOptions {
        skip_unchanged_failed: true,
        ..Default::default()
    };
    let (counts, _) = run_with_options(&db, vec![docs.clone()], options.clone());
    assert_eq!((counts.skipped, counts.ok, counts.failed), (2, 0, 0));

    // 失败文件仅修改时间变化,哈希相同时仍跳过,并保留失败原因。
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let before = repo::find_document_by_path(&conn, &bad.canonicalize().unwrap())
        .unwrap()
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&bad)
        .unwrap()
        .set_modified(
            std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_millis((before.file_mtime_ms + 5000) as u64),
        )
        .unwrap();
    let (counts, _) = run_with_options(&db, vec![bad.clone()], options.clone());
    assert_eq!((counts.skipped, counts.failed), (1, 0));
    let after = repo::find_document_by_path(&conn, &bad.canonicalize().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(after.parse_status, "failed");
    assert_eq!(after.parse_error_message, before.parse_error_message);
    assert_eq!(after.updated_at, before.updated_at);
    assert_ne!(after.file_mtime_ms, before.file_mtime_ms);

    // 普通导入仍重试失败文件;force 优先于重新扫描的跳过策略。
    let (counts, _) = run(&db, vec![docs.clone()], false);
    assert_eq!((counts.skipped, counts.failed), (1, 1));
    let (counts, _) = run_with_options(
        &db,
        vec![docs.clone()],
        ImportOptions {
            force: true,
            ..options.clone()
        },
    );
    assert_eq!((counts.skipped, counts.ok, counts.failed), (0, 1, 1));

    std::fs::write(&good, "已经修改的正文内容").unwrap();
    std::fs::write(&bad, "修复后的正文").unwrap();
    let sub = docs.join("子目录");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("新增.txt"), "新增正文").unwrap();
    let (counts, _) = run_with_options(&db, vec![docs.clone()], options.clone());
    assert_eq!((counts.ok, counts.failed, counts.skipped), (3, 0, 0));
    assert_fts_matches_plain(&conn);
    let (counts, _) = run_with_options(&db, vec![docs], options);
    assert_eq!((counts.ok, counts.failed, counts.skipped), (0, 0, 3));
}

#[test]
fn rescan_skips_unchanged_read_failure_without_hash() {
    let dir = common::temp_dir("e2e-rescan-no-hash");
    let db = dir.join("index.sqlite3");
    let file = dir.join("过大.txt");
    std::fs::write(&file, "超过上限的正文").unwrap();
    let options = ImportOptions {
        max_file_bytes: 1,
        skip_unchanged_failed: true,
        ..Default::default()
    };
    let (counts, _) = run_with_options(&db, vec![file.clone()], options.clone());
    assert_eq!(counts.failed, 1);
    let (counts, _) = run_with_options(&db, vec![file.clone()], options.clone());
    assert_eq!((counts.skipped, counts.failed), (1, 0));
    std::fs::write(&file, "a").unwrap();
    let (counts, _) = run_with_options(&db, vec![file], options);
    assert_eq!((counts.ok, counts.skipped), (1, 0));
}

#[test]
fn single_file_retry_preserves_folder_on_failure_and_success() {
    let dir = common::temp_dir("e2e-retry-source-root");
    let db = dir.join("index.sqlite3");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let file = docs.join("失败.txt");
    std::fs::write(&file, "").unwrap();
    let (counts, _) = run(&db, vec![docs.clone()], false);
    assert_eq!(counts.failed, 1);
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    let path = dunce::canonicalize(&file).unwrap();
    let before = repo::find_document_by_path(&conn, &path).unwrap().unwrap();
    assert_eq!(
        before.source_root,
        Some(
            dunce::canonicalize(&docs)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    let retry = ImportOptions {
        force: true,
        preserve_source_root: true,
        ..Default::default()
    };
    let (counts, _) = run_with_options(&db, vec![file.clone()], retry.clone());
    assert_eq!(counts.failed, 1);
    let failed = repo::find_document_by_path(&conn, &path).unwrap().unwrap();
    assert_eq!(failed.id, before.id);
    assert_eq!(failed.source_root, before.source_root);
    assert_eq!(failed.parse_status, "failed");

    std::fs::write(&file, "修复后的正文").unwrap();
    let (counts, _) = run_with_options(&db, vec![file.clone()], retry.clone());
    assert_eq!(counts.ok, 1);
    let parsed = repo::find_document_by_path(&conn, &path).unwrap().unwrap();
    assert_eq!(parsed.id, before.id);
    assert_eq!(parsed.source_root, before.source_root);
    assert_eq!(parsed.parse_status, "parsed");
    assert!(repo::list_standalone_documents(&conn).unwrap().is_empty());
    assert_eq!(
        repo::list_documents_by_source_root(&conn).unwrap()[0]
            .1
            .len(),
        1
    );
    assert_fts_matches_plain(&conn);

    // 单文件增量更新也保留归属;真正单独添加的文件仍为单独文件。
    std::fs::write(&file, "再次修改后的正文内容").unwrap();
    let (counts, _) = run_with_options(
        &db,
        vec![file],
        ImportOptions {
            skip_unchanged_failed: true,
            preserve_source_root: true,
            ..Default::default()
        },
    );
    assert_eq!(counts.ok, 1);
    assert_eq!(
        repo::find_document_by_path(&conn, &path)
            .unwrap()
            .unwrap()
            .source_root,
        before.source_root
    );
    let standalone = dir.join("单独.txt");
    std::fs::write(&standalone, "独立正文").unwrap();
    let (counts, _) = run_with_options(&db, vec![standalone], retry);
    assert_eq!(counts.ok, 1);
    assert_eq!(repo::list_standalone_documents(&conn).unwrap().len(), 1);
}
