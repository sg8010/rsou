//! 索引库读写:documents/contents/chunks/documents_fts/import_*/settings。
//!
//! 写入约定:
//! - 单文档的「内容+分块+FTS」在一个 `BEGIN IMMEDIATE` 事务里完成,
//!   不会出现半成品可检索(见 docs/plan.md §5.2);
//! - FTS 是普通表,**一行一篇文档**,删除即 `DELETE FROM documents_fts WHERE rowid = ?`,
//!   rowid 显式等于 documents.id;
//! - `documents_fts.content` 与 `document_contents.plain_text` 是同一份全文
//!   (不是分块文本);分块只用于展示,由检索层按字节偏移切片段;
//! - 时间戳一律 Unix 毫秒。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::chunk::Chunk;
use crate::parse::{FileType, ParseError};
use crate::text::PlainText;

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// documents 表的一行(展示与跳过判定用)。
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub id: i64,
    pub path: String,
    pub file_name: String,
    pub title: String,
    pub ext: String,
    pub file_type: String,
    pub file_size: i64,
    pub file_mtime_ms: i64,
    pub parse_status: String,
    pub parse_error_code: Option<String>,
    pub parse_error_message: Option<String>,
    pub text_length: i64,
    pub chunk_count: i64,
    pub indexed_at: Option<i64>,
    pub updated_at: i64,
}

const DOCUMENT_COLS: &str = "id, path, file_name, title, ext, file_type, file_size, \
     file_mtime_ms, parse_status, parse_error_code, parse_error_message, \
     text_length, chunk_count, indexed_at, updated_at";

fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRow> {
    Ok(DocumentRow {
        id: row.get(0)?,
        path: row.get(1)?,
        file_name: row.get(2)?,
        title: row.get(3)?,
        ext: row.get(4)?,
        file_type: row.get(5)?,
        file_size: row.get(6)?,
        file_mtime_ms: row.get(7)?,
        parse_status: row.get(8)?,
        parse_error_code: row.get(9)?,
        parse_error_message: row.get(10)?,
        text_length: row.get(11)?,
        chunk_count: row.get(12)?,
        indexed_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

/// 待导入文件的元数据(扫描/跳过判定用)。
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// 规范化绝对路径(规范化失败时退回原样的绝对路径)
    pub path: PathBuf,
    /// realpath,用于同文件去重
    pub canonical_path: PathBuf,
    pub file_name: String,
    pub ext: String,
    pub file_type: FileType,
    pub file_size: u64,
    pub file_mtime_ms: i64,
}

impl FileMeta {
    /// 读取文件元数据;扩展名不支持时返回 Ok(None)。
    pub fn of(path: &Path) -> anyhow::Result<Option<FileMeta>> {
        let Some(file_type) = crate::parse::file_type_of(path) else {
            return Ok(None);
        };
        let meta = std::fs::metadata(path)
            .with_context(|| format!("读取文件信息失败: {}", path.display()))?;
        // 与 import::scan_paths 用同一个规范化口径(见那里的注释):
        // 两边必须一致,否则「扫描到的路径」与「已有的 canonical_path」
        // 拼写不同,同一文件会被当成两个文件重复入库。
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(Some(FileMeta {
            path: path.to_path_buf(),
            canonical_path: canonical,
            file_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ext: path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default(),
            file_type,
            file_size: meta.len(),
            file_mtime_ms: mtime_ms,
        }))
    }

    /// 文件名去扩展名(标题回退用)。
    pub fn stem(&self) -> String {
        self.file_name
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_owned())
            .unwrap_or_else(|| self.file_name.clone())
    }
}

/// 解析 + 文本化 + 分块后的完整产物(worker 线程产出,写库线程消费)。
#[derive(Debug)]
pub struct ParsedDocument {
    pub title: String,
    pub markdown: String,
    pub plain: PlainText,
    pub chunks: Vec<Chunk>,
    pub warnings: Vec<String>,
    pub parser_name: &'static str,
    pub parser_version: &'static str,
}

// ---------- documents 查询 ----------

pub fn find_document_by_path(
    conn: &Connection,
    path: &Path,
) -> anyhow::Result<Option<DocumentRow>> {
    let sql = format!("SELECT {DOCUMENT_COLS} FROM documents WHERE path = ?1");
    let row = conn
        .query_row(&sql, params![path.to_string_lossy()], row_to_document)
        .optional()?;
    Ok(row)
}

/// 全部文档,新更新的在前。
pub fn list_documents(conn: &Connection) -> anyhow::Result<Vec<DocumentRow>> {
    let sql = format!("SELECT {DOCUMENT_COLS} FROM documents ORDER BY updated_at DESC, id DESC");
    let rows = conn
        .prepare(&sql)?
        .query_map([], row_to_document)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn list_failed_documents(conn: &Connection) -> anyhow::Result<Vec<DocumentRow>> {
    let sql = format!(
        "SELECT {DOCUMENT_COLS} FROM documents WHERE parse_status = 'failed' \
         ORDER BY updated_at DESC, id DESC"
    );
    let rows = conn
        .prepare(&sql)?
        .query_map([], row_to_document)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 按 id 批量取文档(检索结果回填元数据用);分批避免占位符过多。
pub fn get_documents_by_ids(
    conn: &Connection,
    ids: &[i64],
) -> anyhow::Result<std::collections::HashMap<i64, DocumentRow>> {
    let mut map = std::collections::HashMap::with_capacity(ids.len());
    for batch in ids.chunks(500) {
        let marks = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT {DOCUMENT_COLS} FROM documents WHERE id IN ({marks})");
        let rows = conn
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(batch.iter()), row_to_document)?
            .collect::<Result<Vec<_>, _>>()?;
        for row in rows {
            map.insert(row.id, row);
        }
    }
    Ok(map)
}

/// 文档纯文本(预览/片段切片用);文档不存在或没有内容时返回 None。
pub fn get_plain_text(conn: &Connection, document_id: i64) -> anyhow::Result<Option<String>> {
    let text = conn
        .query_row(
            "SELECT plain_text FROM document_contents WHERE document_id = ?1",
            params![document_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(text)
}

// ---------- 写入 ----------

/// 已解析文档落库:一个 BEGIN IMMEDIATE 事务写完 documents + contents +
/// chunks + documents_fts。返回 documents.id。
pub fn save_parsed(
    conn: &mut Connection,
    meta: &FileMeta,
    content_hash: &str,
    parsed: &ParsedDocument,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启写入事务失败")?;
    let id = save_parsed_in(&tx, meta, content_hash, parsed, now_ms)?;
    tx.commit().context("提交写入事务失败")?;
    Ok(id)
}

/// `save_parsed` 的事务内版本:假定调用方已在事务内,不自己开/提交事务
/// (导入批量写库时多个文档共用一个事务)。
pub fn save_parsed_in(
    conn: &Connection,
    meta: &FileMeta,
    content_hash: &str,
    parsed: &ParsedDocument,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let id = upsert_document(
        conn,
        meta,
        content_hash,
        parsed.title.as_str(),
        parsed.parser_name,
        parsed.parser_version,
        now_ms,
    )?;
    clear_document_body(conn, id)?;

    conn.execute(
        "INSERT INTO document_contents(document_id, markdown, plain_text, warnings_json) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            parsed.markdown,
            parsed.plain.text,
            warnings_to_json(&parsed.warnings),
        ],
    )
    .context("写入 document_contents 失败")?;

    let mut insert_chunk = conn.prepare(
        "INSERT INTO chunks(document_id, chunk_index, context_header, start_offset, end_offset) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for chunk in &parsed.chunks {
        insert_chunk.execute(params![
            id,
            chunk.index as i64,
            chunk.context_header,
            chunk.start as i64,
            chunk.end as i64,
        ])?;
    }
    drop(insert_chunk);

    // 一篇文档一行:FTS 的 AND/OR/NOT 因而都是文档级语义。
    conn.execute(
        "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, ?2, ?3)",
        params![id, parsed.title, parsed.plain.text],
    )?;

    conn.execute(
        "UPDATE documents SET parse_status = 'parsed', parse_error_code = NULL, \
         parse_error_message = NULL, text_length = ?2, chunk_count = ?3, indexed_at = ?4 \
         WHERE id = ?1",
        params![
            id,
            parsed.plain.text.len() as i64,
            parsed.chunks.len() as i64,
            now_ms
        ],
    )?;
    Ok(id)
}

/// 解析失败落库:清掉旧正文/分块/FTS,记 failed 与中文原因。返回 documents.id。
pub fn save_failed(
    conn: &mut Connection,
    meta: &FileMeta,
    content_hash: &str,
    error: &ParseError,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启写入事务失败")?;
    let id = save_failed_in(&tx, meta, content_hash, error, now_ms)?;
    tx.commit().context("提交写入事务失败")?;
    Ok(id)
}

/// `save_failed` 的事务内版本:假定调用方已在事务内,不自己开/提交事务。
pub fn save_failed_in(
    conn: &Connection,
    meta: &FileMeta,
    content_hash: &str,
    error: &ParseError,
    now_ms: i64,
) -> anyhow::Result<i64> {
    // 失败文档拿不到标题,用文件名去扩展名顶替;解析器信息无从得知,留空。
    let id = upsert_document(conn, meta, content_hash, &meta.stem(), "", "", now_ms)?;
    clear_document_body(conn, id)?;
    conn.execute(
        "UPDATE documents SET parse_status = 'failed', parse_error_code = ?2, \
         parse_error_message = ?3, text_length = 0, chunk_count = 0, indexed_at = NULL \
         WHERE id = ?1",
        params![id, error.code.as_str(), error.to_string()],
    )?;
    Ok(id)
}

/// 按 path UPSERT documents 行,保留已有 id/created_at;返回行 id。
fn upsert_document(
    conn: &Connection,
    meta: &FileMeta,
    content_hash: &str,
    title: &str,
    parser_name: &str,
    parser_version: &str,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let id: i64 = conn.query_row(
        "INSERT INTO documents(path, canonical_path, file_name, title, ext, file_type, \
         file_size, file_mtime_ms, content_hash, parse_status, parser_name, parser_version, \
         indexed_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'parsed', ?10, ?11, ?12, ?13, ?13) \
         ON CONFLICT(path) DO UPDATE SET \
           canonical_path = excluded.canonical_path, \
           file_name = excluded.file_name, \
           title = excluded.title, \
           ext = excluded.ext, \
           file_type = excluded.file_type, \
           file_size = excluded.file_size, \
           file_mtime_ms = excluded.file_mtime_ms, \
           content_hash = excluded.content_hash, \
           parser_name = excluded.parser_name, \
           parser_version = excluded.parser_version, \
           updated_at = excluded.updated_at \
         RETURNING id",
        params![
            meta.path.to_string_lossy(),
            meta.canonical_path.to_string_lossy(),
            meta.file_name,
            title,
            meta.ext,
            meta.file_type.as_str(),
            meta.file_size as i64,
            meta.file_mtime_ms,
            content_hash,
            parser_name,
            parser_version,
            now_ms,
            now_ms,
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 清掉文档旧的正文/分块/FTS(重解析与失败写库共用)。
fn clear_document_body(conn: &Connection, document_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM documents_fts WHERE rowid = ?1",
        params![document_id],
    )?;
    conn.execute(
        "DELETE FROM chunks WHERE document_id = ?1",
        params![document_id],
    )?;
    conn.execute(
        "DELETE FROM document_contents WHERE document_id = ?1",
        params![document_id],
    )?;
    Ok(())
}

/// 删除整篇文档(FTS 先行,再靠外键级联清 chunks/contents)。
pub fn delete_document(conn: &mut Connection, id: i64) -> anyhow::Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启删除事务失败")?;
    tx.execute("DELETE FROM documents_fts WHERE rowid = ?1", params![id])?;
    let affected = tx.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
    if affected == 0 {
        bail!("文档不存在: id = {id}");
    }
    tx.commit().context("提交删除事务失败")?;
    Ok(())
}

/// 文件已变更(mtime/size 不同)但内容哈希相同时,只更新元数据。
pub fn touch_unchanged(
    conn: &Connection,
    id: i64,
    file_size: u64,
    file_mtime_ms: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE documents SET file_size = ?2, file_mtime_ms = ?3 WHERE id = ?1",
        params![id, file_size as i64, file_mtime_ms],
    )?;
    Ok(())
}

// ---------- import_runs / import_items ----------

#[derive(Debug, Default, Clone, Copy)]
pub struct ImportCounts {
    pub total: usize,
    pub processed: usize,
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub fn create_run(
    conn: &Connection,
    kind: &str,
    root: Option<&Path>,
    total: usize,
    now_ms: i64,
) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO import_runs(kind, root_path, status, total_files, started_at) \
         VALUES (?1, ?2, 'running', ?3, ?4)",
        params![
            kind,
            root.map(|p| p.to_string_lossy().into_owned()),
            total as i64,
            now_ms
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// 签名由阶段 2 任务书固定,参数个数不收敛。
#[allow(clippy::too_many_arguments)]
pub fn upsert_item(
    conn: &Connection,
    run_id: i64,
    path: &Path,
    status: &str,
    error_code: Option<&str>,
    error_message: Option<&str>,
    document_id: Option<i64>,
    now_ms: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO import_items(run_id, path, status, error_code, error_message, document_id, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(run_id, path) DO UPDATE SET \
           status = excluded.status, \
           error_code = excluded.error_code, \
           error_message = excluded.error_message, \
           document_id = excluded.document_id, \
           updated_at = excluded.updated_at",
        params![
            run_id,
            path.to_string_lossy(),
            status,
            error_code,
            error_message,
            document_id,
            now_ms
        ],
    )?;
    Ok(())
}

pub fn finish_run(
    conn: &Connection,
    run_id: i64,
    status: &str,
    counts: &ImportCounts,
    message: Option<&str>,
    now_ms: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE import_runs SET status = ?2, processed_files = ?3, ok_files = ?4, \
         failed_files = ?5, skipped_files = ?6, finished_at = ?7, message = ?8 \
         WHERE id = ?1",
        params![
            run_id,
            status,
            counts.processed as i64,
            counts.ok as i64,
            counts.failed as i64,
            counts.skipped as i64,
            now_ms,
            message
        ],
    )?;
    Ok(())
}

// ---------- settings / stats ----------

pub fn get_setting(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// settings.max_file_mb 的默认与允许范围(设置页 DragValue 用同一组常量)。
pub const DEFAULT_MAX_FILE_MB: u64 = 100;
pub const MAX_FILE_MB_LIMIT: u64 = 2048;

/// 单文件体积上限(字节):读 settings.max_file_mb,缺失或非法时回默认。
pub fn max_file_bytes(conn: &Connection) -> u64 {
    let mb = get_setting(conn, "max_file_mb")
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|mb| (1..=MAX_FILE_MB_LIMIT).contains(mb))
        .unwrap_or(DEFAULT_MAX_FILE_MB);
    mb.saturating_mul(1024 * 1024)
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub documents: i64,
    pub parsed: i64,
    pub failed: i64,
    pub chunks: i64,
}

pub fn stats(conn: &Connection) -> anyhow::Result<Stats> {
    let documents: i64 = conn.query_row("SELECT count(*) FROM documents", [], |r| r.get(0))?;
    let parsed: i64 = conn.query_row(
        "SELECT count(*) FROM documents WHERE parse_status = 'parsed'",
        [],
        |r| r.get(0),
    )?;
    let chunks: i64 = conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
    Ok(Stats {
        documents,
        parsed,
        failed: documents - parsed,
        chunks,
    })
}

/// warnings_json 的最小 JSON 数组序列化(不引 serde_json:只需要转义引号、
/// 反斜杠与控制字符)。
fn warnings_to_json(warnings: &[String]) -> String {
    let mut out = String::from("[");
    for (i, warning) in warnings.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in warning.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}
