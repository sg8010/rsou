//! 索引维护:统计、完整性检查、FTS 重建、optimize/VACUUM、清空与 Markdown 清理。
//! 一致性约定(与 repo.rs 相同):`documents_fts.rowid == documents.id`,
//! FTS 的 title/content 词元分别对应 `documents.title` 与
//! `document_contents.plain_text`(全文只存这一份)。
//! `documents_fts` 是 contentless-delete 表、不存列值,所以完整性检查覆盖
//! 「行对齐 + 索引内部一致」两层;`rebuild_fts` 从 documents + plain_text
//! 全量重灌索引,任何漂移都以它收口。
//!
//! 文档级 FTS 让不变式退化得很干净:一行对一篇,`rowid` 差集就是全部,
//! 不再需要「分块偏移 → FTS 内容」的逐片比对。
//!
//! 本模块只拿 `Connection`,不碰文件扫描与 GUI:设置页/CLI 共用同一批函数。

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::repo;

/// 重建时向进度回调汇报的粒度(行)。
const PROGRESS_STEP: u64 = 500;

/// 索引统计(设置页卡片与 `rsou-cli stats` 共用)。
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub documents: i64,
    pub parsed: i64,
    pub failed: i64,
    pub chunks: i64,
    /// documents_fts 行数(应与已解析文档数一致)
    pub fts_rows: i64,
    /// index.sqlite3 + -wal + -shm 之和
    pub db_bytes: u64,
    /// sum(documents.text_length),即索引进来的原文字节量
    pub text_bytes: i64,
}

pub fn index_stats(conn: &Connection, db_path: &Path) -> anyhow::Result<IndexStats> {
    let base = repo::stats(conn)?;
    let fts_rows: i64 =
        conn.query_row("SELECT count(*) FROM documents_fts", [], |row| row.get(0))?;
    let text_bytes: i64 = conn.query_row(
        "SELECT COALESCE(sum(text_length), 0) FROM documents",
        [],
        |row| row.get(0),
    )?;
    Ok(IndexStats {
        documents: base.documents,
        parsed: base.parsed,
        failed: base.failed,
        chunks: base.chunks,
        fts_rows,
        db_bytes: db_file_bytes(db_path),
        text_bytes,
    })
}

/// 索引文件本体与 WAL 伴生文件的大小之和(不存在的伴生文件按 0 计)。
fn db_file_bytes(db_path: &Path) -> u64 {
    let mut total = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut side = db_path.as_os_str().to_owned();
        side.push(suffix);
        total += std::fs::metadata(PathBuf::from(side))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    total
}

/// 完整性检查结果。
#[derive(Debug)]
pub struct IntegrityReport {
    /// `PRAGMA integrity_check` 全为 "ok"
    pub sqlite_ok: bool,
    /// integrity_check 输出的非 "ok" 行(正常情况下为空)
    pub sqlite_messages: Vec<String>,
    /// FTS5 自带的 integrity-check 命令是否成功
    /// (contentless-delete 表没有列值可对拍,这层只查倒排索引内部一致性)
    pub fts_ok: bool,
    pub fts_message: Option<String>,
    /// documents 有、documents_fts 无的行数
    pub missing_in_fts: i64,
    /// documents_fts 有、documents 无的孤儿行数
    pub orphan_in_fts: i64,
}

impl IntegrityReport {
    pub fn is_consistent(&self) -> bool {
        self.sqlite_ok && self.fts_ok && self.missing_in_fts == 0 && self.orphan_in_fts == 0
    }

    /// 中文一段话总结(设置页卡片与 `rsou-cli check` 共用)。
    pub fn summary(&self) -> String {
        if self.is_consistent() {
            return "索引一致:SQLite 结构与全文索引自检正常,行数对齐".to_owned();
        }
        let mut problems: Vec<String> = Vec::new();
        if !self.sqlite_ok {
            problems.push(format!(
                "SQLite 结构异常({})",
                self.sqlite_messages.join(";")
            ));
        }
        if let Some(message) = &self.fts_message {
            problems.push(format!("全文索引自检失败({message})"));
        }
        if self.missing_in_fts > 0 {
            problems.push(format!("{} 篇文档没有索引行", self.missing_in_fts));
        }
        if self.orphan_in_fts > 0 {
            problems.push(format!("{} 条索引行找不到文档", self.orphan_in_fts));
        }
        format!("索引不一致:{};建议执行「重建全文索引」", problems.join(","))
    }
}

/// 三层检查:SQLite 完整性 → FTS5 自检(索引内部一致性)→ 双向 rowid 差集。
///
/// FTS 自检失败不上抛(记录在 report 里,界面继续显示其它层的结论);
/// 其它层的 SQL 失败视为检查本身失败,向上返回 Err。
pub fn check_integrity(conn: &Connection) -> anyhow::Result<IntegrityReport> {
    let sqlite_messages = conn
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|message| message != "ok")
        .collect::<Vec<_>>();
    let sqlite_ok = sqlite_messages.is_empty();

    // FTS5 的 integrity-check 以「写入特殊命令」的形式存在;只读连接也会失败,
    // 失败信息原样进报告而不上抛,让调用方决定怎么展示。
    let (fts_ok, fts_message) = match conn.execute(
        "INSERT INTO documents_fts(documents_fts) VALUES('integrity-check')",
        [],
    ) {
        Ok(_) => (true, None),
        Err(error) => (false, Some(error.to_string())),
    };

    // 双向差集:只比对已解析文档(FTS 里只有它们)。
    let missing_in_fts: i64 = conn.query_row(
        "SELECT count(*) FROM documents d \
         WHERE d.parse_status = 'parsed' AND d.id NOT IN (SELECT rowid FROM documents_fts)",
        [],
        |row| row.get(0),
    )?;
    let orphan_in_fts: i64 = conn.query_row(
        "SELECT count(*) FROM documents_fts \
         WHERE rowid NOT IN (SELECT id FROM documents)",
        [],
        |row| row.get(0),
    )?;

    Ok(IntegrityReport {
        sqlite_ok,
        sqlite_messages,
        fts_ok,
        fts_message,
        missing_in_fts,
        orphan_in_fts,
    })
}

/// 全文索引是否待重建:迁移丢弃旧表时置 `settings.fts_rebuild_pending`;
/// 索引为空但已有已解析文档时也算(迁移中途崩溃同样兜住)。
/// `rebuild_fts` 提交时清标志。
pub fn needs_fts_rebuild(conn: &Connection) -> anyhow::Result<bool> {
    if repo::get_setting(conn, crate::store::FTS_REBUILD_PENDING_KEY)?.as_deref() == Some("1") {
        return Ok(true);
    }
    let pending: bool = conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM documents_fts) \
         AND EXISTS(SELECT 1 FROM documents WHERE parse_status = 'parsed')",
        [],
        |row| row.get(0),
    )?;
    Ok(pending)
}

/// 全量重建 documents_fts:一个 BEGIN IMMEDIATE 事务里先 'delete-all' 清空
/// 索引(contentless-delete 专用命令,顺带清掉孤儿行与残留词元),再从
/// documents + plain_text 分批重插;提交前清掉 fts_rebuild_pending 标志。
/// 返回重建行数(= 已解析文档数);每 PROGRESS_STEP 行回调一次进度(done, total)。
pub fn rebuild_fts(
    conn: &mut Connection,
    progress: &mut dyn FnMut(u64, u64),
) -> anyhow::Result<u64> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启重建事务失败")?;
    let total: u64 = tx
        .query_row(
            "SELECT count(*) FROM documents d \
             JOIN document_contents dc ON dc.document_id = d.id \
             WHERE d.parse_status = 'parsed'",
            [],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u64;

    tx.execute(
        "INSERT INTO documents_fts(documents_fts) VALUES('delete-all')",
        [],
    )
    .context("清空全文索引失败")?;

    // 分批 INSERT:整库一行一批会让进度回调只能报一次,大资料库上界面
    // 会长时间停在 0%。按 id 区间切,每 PROGRESS_STEP 篇回调一次。
    let mut last_id = 0i64;
    let mut processed = 0u64;
    loop {
        let batch_last_id: Option<i64> = tx.query_row(
            "SELECT max(id) FROM (\
             SELECT d.id FROM documents d \
             JOIN document_contents dc ON dc.document_id = d.id \
             WHERE d.parse_status = 'parsed' AND d.id > ?1 \
             ORDER BY d.id LIMIT ?2)",
            params![last_id, PROGRESS_STEP as i64],
            |row| row.get(0),
        )?;
        let Some(batch_last_id) = batch_last_id else {
            break;
        };
        let batch = tx
            .execute(
                "INSERT INTO documents_fts(rowid, title, content) \
                 SELECT d.id, d.title, dc.plain_text \
                 FROM documents d \
                 JOIN document_contents dc ON dc.document_id = d.id \
                 WHERE d.parse_status = 'parsed' AND d.id > ?1 AND d.id <= ?2 \
                 ORDER BY d.id",
                params![last_id, batch_last_id],
            )
            .context("重建全文索引失败")? as u64;
        last_id = batch_last_id;
        processed += batch;
        progress(processed, total);
        if batch < PROGRESS_STEP {
            break;
        }
    }

    tx.execute(
        "DELETE FROM settings WHERE key = ?1",
        [crate::store::FTS_REBUILD_PENDING_KEY],
    )?;
    tx.commit().context("提交重建事务失败")?;
    Ok(processed)
}

/// 索引优化:FTS5 'optimize' 合并内部段 → WAL 截断 → VACUUM。
pub fn optimize(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO documents_fts(documents_fts) VALUES('optimize')",
        [],
    )
    .context("FTS optimize 失败")?;
    // wal_checkpoint 返回一行结果,execute_batch 直接丢弃它即可。
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .context("WAL checkpoint 失败")?;
    conn.execute_batch("VACUUM;").context("VACUUM 失败")?;
    Ok(())
}

/// 清空资料库:一个事务清空 FTS 索引、全部文档(级联 contents/chunks)、
/// 全部导入记录(级联 import_items);settings(含 schema_version)保留。
pub fn clear_all(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启清空事务失败")?;
    // contentless-delete 表:'delete-all' 一条命令清掉全部索引条目。
    tx.execute(
        "INSERT INTO documents_fts(documents_fts) VALUES('delete-all')",
        [],
    )
    .context("清空全文索引失败")?;
    tx.execute("DELETE FROM documents", [])
        .context("清空文档表失败")?;
    tx.execute("DELETE FROM import_runs", [])
        .context("清空导入记录失败")?;
    tx.execute(
        "DELETE FROM settings WHERE key = ?1",
        [crate::store::FTS_REBUILD_PENDING_KEY],
    )?;
    tx.commit().context("提交清空事务失败")?;
    Ok(())
}

/// 清掉存量 `document_contents.markdown`(改写为空串),返回清理的行数。
///
/// 只清数据不回收页面——调用方随后跑 `optimize`(VACUUM)才能落实体积;
/// 「保存 Markdown 原文」开关只管新导入,这个函数负责回收存量。
pub fn purge_stored_markdown(conn: &Connection) -> anyhow::Result<u64> {
    let cleared = conn
        .execute(
            "UPDATE document_contents SET markdown = '' WHERE markdown <> ''",
            [],
        )
        .context("清理 Markdown 原文失败")?;
    Ok(cleared as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk;
    use crate::parse::FileType;
    use crate::repo::{FileMeta, ParsedDocument};
    use crate::search::{self, SearchResponse};
    use crate::text;

    /// 与 search.rs 测试同款:走真实 text→chunk 管线造一篇已解析文档。
    fn save_doc(conn: &mut Connection, path: &str, markdown: &str) -> i64 {
        let meta = FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: path.rsplit('.').next().unwrap_or("").to_owned(),
            file_type: FileType::Text,
            file_size: 0,
            file_mtime_ms: 1_000,
            source_root: None,
        };
        let plain = text::markdown_to_plain(markdown);
        let title = text::extract_title(&plain, &meta.stem());
        let chunks = chunk::chunk_document(&title, &plain);
        let parsed = ParsedDocument {
            title,
            markdown: markdown.to_owned(),
            plain,
            chunks,
            warnings: Vec::new(),
            parser_name: "text",
            parser_version: "text.v1",
        };
        repo::save_parsed(conn, &meta, "hash", &parsed, 1_000).unwrap()
    }

    /// 检索结果的可比签名:(document_id, [(片段起点, 高亮区间)])。
    type HitSig = Vec<(i64, Vec<(usize, Vec<(usize, usize)>)>)>;
    fn signature(response: &SearchResponse) -> HitSig {
        response
            .documents
            .iter()
            .map(|doc| {
                (
                    doc.document.id,
                    doc.hits
                        .iter()
                        .map(|hit| {
                            (
                                hit.start_offset,
                                hit.highlights
                                    .iter()
                                    .map(|span| (span.start, span.end))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    fn request(query: &str) -> search::SearchRequest {
        search::SearchRequest {
            query: query.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn missing_row_detected_and_rebuild_restores_results() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "文档管理系统的设计");
        save_doc(&mut conn, "/d/b.txt", "另一个文档,讲实施");
        save_doc(&mut conn, "/d/c.txt", "完全无关的内容");

        let before = search::search(&conn, &request("文档")).unwrap();
        let before_sig = signature(&before);
        assert_eq!(before.total_documents, 2);

        // 制造不一致:删掉一篇文档的 FTS 行。
        let victim: i64 = conn
            .query_row("SELECT rowid FROM documents_fts LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "DELETE FROM documents_fts WHERE rowid = ?1",
            params![victim],
        )
        .unwrap();

        let report = check_integrity(&conn).unwrap();
        assert_eq!(report.missing_in_fts, 1);
        assert!(!report.is_consistent());
        assert!(report.summary().contains("不一致"));

        let mut progress_calls = 0;
        let rows = rebuild_fts(&mut conn, &mut |_, _| progress_calls += 1).unwrap();
        assert_eq!(rows, 3);
        assert!(progress_calls >= 1);

        let report = check_integrity(&conn).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());

        // 重建后的结果集与之前逐字段相等。
        let after = search::search(&conn, &request("文档")).unwrap();
        assert_eq!(signature(&after), before_sig);
        assert_eq!(after.total_hits, before.total_hits);
        assert_eq!(after.total_documents, before.total_documents);
    }

    #[test]
    fn orphan_row_detected_and_rebuild_clears_it() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "孤儿测试文档");
        conn.execute(
            "INSERT INTO documents_fts(rowid, title, content) \
             VALUES (999999, 'x', 'x')",
            [],
        )
        .unwrap();

        let report = check_integrity(&conn).unwrap();
        assert_eq!(report.orphan_in_fts, 1);
        assert!(!report.is_consistent());

        rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        let report = check_integrity(&conn).unwrap();
        assert_eq!(report.orphan_in_fts, 0);
        assert!(report.is_consistent());
    }

    #[test]
    fn stale_index_tokens_are_filtered_by_literal_recheck() {
        // contentless-delete 允许同 rowid 重复 INSERT(词元叠加):借此模拟
        // 「索引里多出与原文不符的词元」这种漂移。行级检查看不出问题
        // (rowid 对齐、索引内部一致),但检索层在 plain_text 上做字面量复核,
        // 假阳性不会进入结果。
        let mut conn = crate::store::open_in_memory().unwrap();
        let id = save_doc(&mut conn, "/d/a.txt", "内容是正常的原文");
        conn.execute(
            "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, 'x', '垃圾词')",
            params![id],
        )
        .unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"垃圾词\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "残留词元会让 MATCH 命中");

        let response = search::search(&conn, &request("垃圾词")).unwrap();
        assert_eq!(response.total_documents, 1);
        assert!(response.documents.is_empty(), "字面量复核应把假阳性剔除");

        // 重建能把这类漂移一并清掉。
        rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"垃圾词\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "重建后残留词元应消失");
    }

    #[test]
    fn needs_fts_rebuild_tracks_flag_and_empty_index() {
        let mut conn = crate::store::open_in_memory().unwrap();
        // 空库不需要重建。
        assert!(!needs_fts_rebuild(&conn).unwrap());
        save_doc(&mut conn, "/d/a.txt", "正常文档");
        assert!(!needs_fts_rebuild(&conn).unwrap());

        // 迁移置位 → 需要重建。
        repo::set_setting(&conn, crate::store::FTS_REBUILD_PENDING_KEY, "1").unwrap();
        assert!(needs_fts_rebuild(&conn).unwrap());
        rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        assert!(!needs_fts_rebuild(&conn).unwrap(), "重建提交后应清标志");

        // 无标志但索引空、有已解析文档(迁移中途崩溃的情形)也判为待重建。
        conn.execute(
            "INSERT INTO documents_fts(documents_fts) VALUES('delete-all')",
            [],
        )
        .unwrap();
        assert!(needs_fts_rebuild(&conn).unwrap());
    }

    #[test]
    fn purge_stored_markdown_clears_existing_but_keeps_plain_text() {
        let mut conn = crate::store::open_in_memory().unwrap();
        repo::set_setting(&conn, "save_markdown", "1").unwrap();
        let meta = FileMeta {
            path: "/d/a.txt".into(),
            canonical_path: "/d/a.txt".into(),
            file_name: "a.txt".to_owned(),
            ext: "txt".to_owned(),
            file_type: FileType::Text,
            file_size: 1,
            file_mtime_ms: 1,
            source_root: None,
        };
        let plain = text::markdown_to_plain("# 标题\n\n正文");
        let parsed = ParsedDocument {
            title: "标题".to_owned(),
            markdown: "# 标题\n\n正文".to_owned(),
            plain,
            chunks: Vec::new(),
            warnings: Vec::new(),
            parser_name: "t",
            parser_version: "t",
        };
        let id = repo::save_parsed(&mut conn, &meta, "h", &parsed, 1).unwrap();
        assert_eq!(purge_stored_markdown(&conn).unwrap(), 1);
        // 再跑一遍是幂等空转。
        assert_eq!(purge_stored_markdown(&conn).unwrap(), 0);
        let markdown: String = conn
            .query_row(
                "SELECT markdown FROM document_contents WHERE document_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(markdown, "");
        assert_eq!(
            repo::get_plain_text(&conn, id).unwrap().unwrap(),
            "标题\n\n正文"
        );
    }

    #[test]
    fn clear_all_empties_library_but_keeps_settings() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "待清空");
        repo::set_setting(&conn, "max_file_mb", "128").unwrap();
        repo::set_setting(&conn, crate::store::FTS_REBUILD_PENDING_KEY, "1").unwrap();

        clear_all(&mut conn).unwrap();

        let stats = repo::stats(&conn).unwrap();
        assert_eq!(stats.documents, 0);
        assert_eq!(stats.chunks, 0);
        let fts_rows: i64 = conn
            .query_row("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 0);
        let runs: i64 = conn
            .query_row("SELECT count(*) FROM import_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 0);
        assert_eq!(
            repo::get_setting(&conn, "schema_version")
                .unwrap()
                .as_deref(),
            Some(crate::store::SCHEMA_VERSION)
        );
        assert_eq!(
            repo::get_setting(&conn, "max_file_mb").unwrap().as_deref(),
            Some("128")
        );
        // 清空顺带摘掉待重建标志(库里已无文档,没有要重建的东西)。
        assert_eq!(
            repo::get_setting(&conn, crate::store::FTS_REBUILD_PENDING_KEY)
                .unwrap()
                .as_deref(),
            None
        );
    }

    #[test]
    fn optimize_runs_on_memory_db() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "优化前先有内容");
        optimize(&conn).unwrap();
        // optimize 不改变可检索内容。
        let report = check_integrity(&conn).unwrap();
        assert!(report.is_consistent());
    }

    #[test]
    fn rebuild_uses_document_ids_across_deleted_and_failed_rows() {
        for sparse in [false, true] {
            let mut conn = crate::store::open_in_memory().unwrap();
            // 已删除的低 ID 区间之后,仍有一篇失败文档占据 ID 500。
            conn.execute(
                "INSERT INTO documents(id, path, canonical_path, file_name, ext, file_type, \
                 file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at) \
                 VALUES(500, '/d/failed.txt', '/d/failed.txt', 'failed.txt', 'txt', 'text', \
                 0, 0, 'failed', 'failed', 0, 0)",
                [],
            )
            .unwrap();
            let count = if sparse { 503 } else { 501 };
            for i in 0..count {
                save_doc(
                    &mut conn,
                    &format!("/d/{i}.txt"),
                    &format!("文档 {i}\n\n{}", "测试正文 ".repeat(i % 7 + 1)),
                );
            }
            if sparse {
                conn.execute_batch(
                    "DELETE FROM documents_fts WHERE rowid IN (700, 800);\
                     DELETE FROM documents WHERE id = 700;\
                     DELETE FROM document_contents WHERE document_id = 800;\
                     DELETE FROM chunks WHERE document_id = 800;\
                     UPDATE documents SET parse_status = 'failed' WHERE id = 800;",
                )
                .unwrap();
            }
            let before = search::search(&conn, &request("正文")).unwrap();
            let ranks = |response: &SearchResponse| {
                response
                    .documents
                    .iter()
                    .map(|doc| (doc.document.id, doc.best_rank))
                    .collect::<Vec<_>>()
            };
            let mut calls = Vec::new();
            let rows =
                rebuild_fts(&mut conn, &mut |done, total| calls.push((done, total))).unwrap();
            assert_eq!(rows, 501);
            assert_eq!(calls, vec![(500, 501), (501, 501)]);
            assert!(check_integrity(&conn).unwrap().is_consistent());
            let after = search::search(&conn, &request("正文")).unwrap();
            assert_eq!(signature(&after), signature(&before));
            // contentless-delete 删除后的 BM25 分数不保证与重建后完全
            // 一致;有删除时验证排序,无删除时也验证精确分数。
            if !sparse {
                assert_eq!(ranks(&after), ranks(&before));
            }
            assert_eq!(after.total_documents, before.total_documents);
            assert_eq!(after.total_hits, before.total_hits);
        }
    }

    #[test]
    fn rebuild_failure_keeps_pending_and_rolls_back_index() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "正文内容");
        repo::set_setting(&conn, crate::store::FTS_REBUILD_PENDING_KEY, "1").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_rebuild BEFORE DELETE ON settings \
             WHEN OLD.key = 'fts_rebuild_pending' \
             BEGIN SELECT RAISE(ABORT, 'injected rebuild failure'); END;",
        )
        .unwrap();
        let before = search::search(&conn, &request("正文")).unwrap();
        assert!(rebuild_fts(&mut conn, &mut |_, _| {}).is_err());
        assert!(needs_fts_rebuild(&conn).unwrap());
        assert!(check_integrity(&conn).unwrap().is_consistent());
        assert_eq!(
            signature(&search::search(&conn, &request("正文")).unwrap()),
            signature(&before)
        );
        conn.execute_batch("DROP TRIGGER fail_rebuild").unwrap();
        assert_eq!(rebuild_fts(&mut conn, &mut |_, _| {}).unwrap(), 1);
        assert!(!needs_fts_rebuild(&conn).unwrap());
    }

    #[test]
    fn rebuild_reports_incremental_progress() {
        // 分批重建:进度回调应多于一次(旧的单条 INSERT 只能报一次)。
        let mut conn = crate::store::open_in_memory().unwrap();
        for i in 0..(PROGRESS_STEP as usize + 10) {
            save_doc(&mut conn, &format!("/d/{i}.txt"), "正文内容");
        }
        let mut calls = Vec::new();
        let rows = rebuild_fts(&mut conn, &mut |done, total| calls.push((done, total))).unwrap();
        assert_eq!(rows, PROGRESS_STEP + 10);
        assert!(calls.len() >= 2, "应分批回调: {calls:?}");
        assert_eq!(
            calls.last().copied(),
            Some((PROGRESS_STEP + 10, PROGRESS_STEP + 10))
        );
        assert!(check_integrity(&conn).unwrap().is_consistent());
    }
}
