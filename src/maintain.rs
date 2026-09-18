//! 索引维护:统计、完整性检查、FTS 重建、optimize/VACUUM 与清空。
//! 一致性约定(与 repo.rs 相同):`documents_fts.rowid == documents.id`,且
//! `documents_fts.content == document_contents.plain_text`,
//! `documents_fts.title == documents.title`。
//! `check_integrity` 验证这两层不变式与 SQLite/FTS5 自身的健康;
//! `rebuild_fts` 按该约定从 documents + plain_text 全量重写 FTS 表。
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
    pub fts_ok: bool,
    pub fts_message: Option<String>,
    /// documents 有、documents_fts 无的行数
    pub missing_in_fts: i64,
    /// documents_fts 有、documents 无的孤儿行数
    pub orphan_in_fts: i64,
    /// 抽样比对中 content/title 与 documents/plain_text 不一致的条数
    pub content_mismatch: i64,
    /// 实际抽样的行数
    pub sampled: i64,
}

impl IntegrityReport {
    pub fn is_consistent(&self) -> bool {
        self.sqlite_ok
            && self.fts_ok
            && self.missing_in_fts == 0
            && self.orphan_in_fts == 0
            && self.content_mismatch == 0
    }

    /// 中文一段话总结(设置页卡片与 `rsou-cli check` 共用)。
    pub fn summary(&self) -> String {
        if self.is_consistent() {
            return format!(
                "索引一致:SQLite 结构正常,抽样 {} 篇文档的标题与正文全部吻合",
                self.sampled
            );
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
        if self.content_mismatch > 0 {
            problems.push(format!("{} 条索引内容与原文不一致", self.content_mismatch));
        }
        format!(
            "索引不一致:{}(抽样 {} 篇);建议执行「重建全文索引」",
            problems.join(","),
            self.sampled
        )
    }
}

/// 四层检查:SQLite 完整性 → FTS5 自检 → 双向 rowid 差集 → 内容抽样比对。
///
/// FTS 自检失败不上抛(记录在 report 里,界面继续显示其它层的结论);
/// 其它层的 SQL 失败视为检查本身失败,向上返回 Err。
pub fn check_integrity(conn: &Connection, sample_limit: usize) -> anyhow::Result<IntegrityReport> {
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

    // 抽样比对:FTS 行按 rowid 等值 JOIN 回 documents/contents,逐列核对。
    let rows = conn
        .prepare(
            "SELECT f.title, d.title, f.content, dc.plain_text \
             FROM documents_fts f \
             JOIN documents d ON d.id = f.rowid \
             JOIN document_contents dc ON dc.document_id = d.id \
             ORDER BY f.rowid LIMIT ?1",
        )?
        .query_map(params![sample_limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let sampled = rows.len() as i64;
    let mut content_mismatch = 0i64;
    for (fts_title, title, fts_content, plain) in &rows {
        if fts_title != title || fts_content != plain {
            content_mismatch += 1;
        }
    }

    Ok(IntegrityReport {
        sqlite_ok,
        sqlite_messages,
        fts_ok,
        fts_message,
        missing_in_fts,
        orphan_in_fts,
        content_mismatch,
        sampled,
    })
}

/// 全量重建 documents_fts:一个 BEGIN IMMEDIATE 事务里先整表 DELETE 清表
/// (顺带清掉孤儿行;'delete-all' 命令只适用 contentless/external-content
/// 表,普通 FTS5 表不可用),再从 documents + plain_text 整篇重插。
/// 返回重建行数(= 已解析文档数);每 PROGRESS_STEP 行回调一次进度(done, total)。
pub fn rebuild_fts(
    conn: &mut Connection,
    progress: &mut dyn FnMut(u64, u64),
) -> anyhow::Result<u64> {
    let total: u64 = conn
        .query_row(
            "SELECT count(*) FROM documents d \
             JOIN document_contents dc ON dc.document_id = d.id \
             WHERE d.parse_status = 'parsed'",
            [],
            |row| row.get::<_, i64>(0),
        )?
        .max(0) as u64;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启重建事务失败")?;
    tx.execute("DELETE FROM documents_fts", [])
        .context("清空全文索引失败")?;

    // 分批 INSERT:整库一行一批会让进度回调只能报一次,大资料库上界面
    // 会长时间停在 0%。按 id 区间切,每 PROGRESS_STEP 篇回调一次。
    let mut inserted = 0u64;
    loop {
        let batch = tx
            .execute(
                "INSERT INTO documents_fts(rowid, title, content) \
                 SELECT d.id, d.title, dc.plain_text \
                 FROM documents d \
                 JOIN document_contents dc ON dc.document_id = d.id \
                 WHERE d.parse_status = 'parsed' AND d.id > ?1 \
                 ORDER BY d.id LIMIT ?2",
                params![inserted as i64, PROGRESS_STEP as i64],
            )
            .context("重建全文索引失败")? as u64;
        if batch == 0 {
            break;
        }
        inserted += batch;
        progress(inserted.min(total), total);
        if batch < PROGRESS_STEP {
            break;
        }
    }

    tx.commit().context("提交重建事务失败")?;
    Ok(inserted)
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

/// 清空资料库:一个事务删除 FTS 全部行、全部文档(级联 contents/chunks)、
/// 全部导入记录(级联 import_items);settings(含 schema_version)保留。
pub fn clear_all(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启清空事务失败")?;
    tx.execute("DELETE FROM documents_fts", [])
        .context("清空全文索引失败")?;
    tx.execute("DELETE FROM documents", [])
        .context("清空文档表失败")?;
    tx.execute("DELETE FROM import_runs", [])
        .context("清空导入记录失败")?;
    tx.commit().context("提交清空事务失败")?;
    Ok(())
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

        let report = check_integrity(&conn, 2000).unwrap();
        assert_eq!(report.missing_in_fts, 1);
        assert!(!report.is_consistent());
        assert!(report.summary().contains("不一致"));

        let mut progress_calls = 0;
        let rows = rebuild_fts(&mut conn, &mut |_, _| progress_calls += 1).unwrap();
        assert_eq!(rows, 3);
        assert!(progress_calls >= 1);

        let report = check_integrity(&conn, 2000).unwrap();
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

        let report = check_integrity(&conn, 2000).unwrap();
        assert_eq!(report.orphan_in_fts, 1);
        assert!(!report.is_consistent());

        rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        let report = check_integrity(&conn, 2000).unwrap();
        assert_eq!(report.orphan_in_fts, 0);
        assert!(report.is_consistent());
    }

    #[test]
    fn tampered_content_detected_by_sampling() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "内容是正常的原文");
        let victim: i64 = conn
            .query_row("SELECT rowid FROM documents_fts LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "UPDATE documents_fts SET content = '被篡改的内容' WHERE rowid = ?1",
            params![victim],
        )
        .unwrap();

        let report = check_integrity(&conn, 2000).unwrap();
        assert_eq!(report.content_mismatch, 1);
        assert_eq!(report.sampled, 1);
        assert!(!report.is_consistent());
    }

    #[test]
    fn tampered_title_detected_by_sampling() {
        // title 也是不变式的一部分,不只 content。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "# 原标题\n\n正文");
        conn.execute("UPDATE documents_fts SET title = '被换掉的标题'", [])
            .unwrap();
        let report = check_integrity(&conn, 2000).unwrap();
        assert_eq!(report.content_mismatch, 1);
        assert!(!report.is_consistent());
    }

    #[test]
    fn clear_all_empties_library_but_keeps_settings() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "待清空");
        repo::set_setting(&conn, "max_file_mb", "128").unwrap();

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
    }

    #[test]
    fn optimize_runs_on_memory_db() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/a.txt", "优化前先有内容");
        optimize(&conn).unwrap();
        // optimize 不改变可检索内容。
        let report = check_integrity(&conn, 2000).unwrap();
        assert!(report.is_consistent());
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
        assert!(check_integrity(&conn, 2000).unwrap().is_consistent());
    }
}
