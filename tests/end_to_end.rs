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
    let mut outcomes = Vec::new();
    let counts = import::run_import(
        dir_db,
        inputs,
        ImportOptions {
            force,
            ..ImportOptions::default()
        },
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

/// 验证每个已索引文档:chunks_fts.content == plain_text[start..end](逐字节)。
fn assert_fts_matches_plain(conn: &rusqlite::Connection) {
    let mut stmt = conn
        .prepare(
            "SELECT c.start_offset, c.end_offset, dc.plain_text, f.content \
             FROM chunks c \
             JOIN document_contents dc ON dc.document_id = c.document_id \
             JOIN chunks_fts f ON f.rowid = c.id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!rows.is_empty(), "应至少有一条分块");
    for (start, end, plain, content) in &rows {
        assert_eq!(
            &plain[*start as usize..*end as usize],
            content.as_str(),
            "FTS 内容应与 plain_text 切片逐字节相等"
        );
    }
    // FTS 行数 == chunks 行数 == sum(documents.chunk_count)
    let fts: i64 = conn
        .query_row("SELECT count(*) FROM chunks_fts", [], |r| r.get(0))
        .unwrap();
    let chunks: i64 = conn
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    let declared: i64 = conn
        .query_row(
            "SELECT COALESCE(sum(chunk_count), 0) FROM documents",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts, chunks);
    assert_eq!(fts, declared);
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

    // 等一拍再原样重写:mtime 变、内容不变,应命中哈希二级跳过。
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, &bytes).unwrap();
    let new_mtime_ms = std::fs::metadata(&file)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(new_mtime_ms > before.file_mtime_ms, "mtime 应已变化");

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
    let leftover: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks_fts f JOIN chunks c ON f.rowid = c.id WHERE c.document_id = ?1",
            [doc.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0);
    let stats = repo::stats(&conn).unwrap();
    assert_eq!(stats.documents, 3);
    drop(conn);

    // ---------- force:全部重解析 ----------
    let (counts, _) = run(&db_path, vec![docs_dir.clone()], true);
    assert_eq!(counts.ok, 3, "剩 3 个可解析文件应全部重解析");
    assert_eq!(counts.skipped, 0);
}
