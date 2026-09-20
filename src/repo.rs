//! 索引库读写:documents/contents/chunks/documents_fts/import_*/settings。
//!
//! 写入约定:
//! - 单文档的「内容+分块+FTS」在一个 `BEGIN IMMEDIATE` 事务里完成,
//!   不会出现半成品可检索(见 docs/plan.md §5.2);
//! - FTS 是 contentless-delete 表,**一行一篇文档**,删除即
//!   `DELETE FROM documents_fts WHERE rowid = ?`(按 rowid 直接清词元,
//!   不需要回读旧值,对删除顺序没有要求),rowid 显式等于 documents.id;
//! - 全文只在 `document_contents.plain_text` 存一份;FTS 的 title/content 列
//!   只建索引不落存储(读回恒 NULL);分块只用于展示,由检索层按字节偏移切片段;
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
    /// 来自哪个「已添加文件夹」(规范化绝对路径);None = 单独添加的文件。
    /// 资料库页据此把文档分到「文件夹」/「单独文件」两个页签。
    pub source_root: Option<String>,
    pub updated_at: i64,
}

const DOCUMENT_COLS: &str = "id, path, file_name, title, ext, file_type, file_size, \
     file_mtime_ms, parse_status, parse_error_code, parse_error_message, \
     text_length, chunk_count, indexed_at, source_root, updated_at";

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
        source_root: row.get(14)?,
        updated_at: row.get(15)?,
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
    /// 本次导入的「来源文件夹」(规范化绝对路径);None = 单独添加的文件。
    /// 由导入流程注入(不是从文件本身读出来的),所以默认 None。
    pub source_root: Option<String>,
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
            source_root: None,
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
///
/// `markdown` 是过程数据:plain/标题/分块都由它派生,但是否落库由
/// `settings.save_markdown` 决定(见 `save_parsed_in`)。
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

/// 来自「添加文件夹」的文档,按来源文件夹分组(文件夹按路径排序)。
///
/// 资料库页用它建树:外层是文件夹,内层是该文件夹下的文档。只返回
/// `source_root` 非空的行;单独添加的文件走 `list_documents` 那一侧。
pub fn list_documents_by_source_root(
    conn: &Connection,
) -> anyhow::Result<Vec<(String, Vec<DocumentRow>)>> {
    let sql = format!(
        "SELECT {DOCUMENT_COLS} FROM documents WHERE source_root IS NOT NULL \
         ORDER BY source_root ASC, file_name COLLATE NOCASE ASC"
    );
    let rows = conn
        .prepare(&sql)?
        .query_map([], |row| {
            Ok((row_to_document(row)?, row.get::<_, String>(14)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // 保序分组(SQL 已按 source_root 排序,同一个文件夹的行必然相邻)。
    let mut grouped: Vec<(String, Vec<DocumentRow>)> = Vec::new();
    for (document, root) in rows {
        match grouped.last_mut() {
            Some((last_root, docs)) if *last_root == root => docs.push(document),
            _ => grouped.push((root, vec![document])),
        }
    }
    Ok(grouped)
}

/// 单独添加的文件(没有来源文件夹)。
pub fn list_standalone_documents(conn: &Connection) -> anyhow::Result<Vec<DocumentRow>> {
    let sql = format!(
        "SELECT {DOCUMENT_COLS} FROM documents WHERE source_root IS NULL \
         ORDER BY updated_at DESC, id DESC"
    );
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

    // markdown 只是过程数据(plain/标题/分块都由它派生):默认不持久化,
    // settings.save_markdown=1 时才落库(设置页开关;省一份全文体积)。
    let markdown = if save_markdown_enabled(conn) {
        parsed.markdown.as_str()
    } else {
        ""
    };
    conn.execute(
        "INSERT INTO document_contents(document_id, markdown, plain_text, warnings_json) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            markdown,
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
    // contentless-delete 表:这里的 title/content 只进倒排索引,不落存储。
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
    // 注意:SQL 里不要再写 `--` 行注释——这是一个用 `\` 续行的单行字符串,
    // 续行把换行吃掉,`--` 会把后面所有子句一起注释掉(实际踩过:报
    // "incomplete input")。要注释就写在 Rust 这一侧。
    let id: i64 = conn.query_row(
        "INSERT INTO documents(path, canonical_path, file_name, title, ext, file_type, \
         file_size, file_mtime_ms, content_hash, parse_status, parser_name, parser_version, \
         indexed_at, source_root, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'parsed', ?10, ?11, ?12, ?13, ?14, ?14) \
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
           source_root = excluded.source_root, \
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
            meta.source_root.as_deref(),
            now_ms,
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 清掉文档旧的正文/分块/FTS(重解析与失败写库共用)。
/// contentless-delete 下 FTS 删除按 rowid 清词元,与 contents 谁先谁后无所谓;
/// 保持「FTS 先行」只是沿用旧约定的直观顺序。
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

/// 删除整篇文档(先删 FTS 行,再删 documents 靠外键级联清 chunks/contents)。
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

/// 移除一个「已添加文件夹」及其**全部**归属文档(按 source_root 精确匹配)。
/// 返回删除的文档数。
///
/// 只删库里的索引记录,不动磁盘上的文件——用户点「移除」表达的是「不再索引
/// 这个文件夹」,不是「删我的文件」。文案上也必须说清这一点。
pub fn delete_source_root(conn: &mut Connection, source_root: &str) -> anyhow::Result<usize> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("开启移除文件夹事务失败")?;
    // 与 delete_document 同序:先删 FTS 行,再删 documents 靠外键级联清
    // chunks/contents。
    tx.execute(
        "DELETE FROM documents_fts WHERE rowid IN \
         (SELECT id FROM documents WHERE source_root = ?1)",
        params![source_root],
    )?;
    let deleted = tx.execute(
        "DELETE FROM documents WHERE source_root = ?1",
        params![source_root],
    )?;
    tx.commit().context("提交移除文件夹事务失败")?;
    Ok(deleted)
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

/// 已结束的导入历史保留 90 天,任务明细随任务级联删除。
pub const IMPORT_HISTORY_RETENTION_DAYS: i64 = 90;

/// 读写打开数据库时清理过期导入历史,不影响文档、正文或全文索引。
///
/// 以结束时间计算期限;旧记录缺少结束时间时回退到开始时间。
/// 正在运行的任务不清理,避免破坏另一个连接正在写入的任务。
/// 恰好到达 90 天边界的记录仍保留,超过边界才删除。
pub fn prune_import_history(conn: &Connection, now_ms: i64) -> anyhow::Result<usize> {
    let cutoff = now_ms.saturating_sub(IMPORT_HISTORY_RETENTION_DAYS * 86_400_000);
    conn.execute(
        "DELETE FROM import_runs \
         WHERE status IN ('done', 'failed', 'cancelled') \
         AND COALESCE(finished_at, started_at) < ?1",
        [cutoff],
    )
    .context("清理超过 90 天的导入历史失败")
}

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

/// settings.save_markdown 的默认值:不保存(索引文件小一半左右)。
pub const DEFAULT_SAVE_MARKDOWN: bool = false;

/// 是否持久化 document_contents.markdown:读 settings.save_markdown,
/// 缺失、非法或读取出错时回默认(写库路径上不为配置项读失败而中断导入)。
pub fn save_markdown_enabled(conn: &Connection) -> bool {
    get_setting(conn, "save_markdown")
        .ok()
        .flatten()
        .map(|value| value.trim() == "1")
        .unwrap_or(DEFAULT_SAVE_MARKDOWN)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::FileType;
    use crate::text;

    /// 造一篇文档并落库(source_root 由参数决定)。
    fn save_doc(conn: &mut Connection, path: &str, source_root: Option<&str>) -> i64 {
        let meta = FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: "txt".to_owned(),
            file_type: FileType::Text,
            file_size: 1,
            file_mtime_ms: 1,
            source_root: source_root.map(str::to_owned),
        };
        let plain = text::markdown_to_plain("合同正文");
        let chunks = crate::chunk::chunk_document("标题", &plain);
        let parsed = ParsedDocument {
            title: "标题".to_owned(),
            markdown: String::new(),
            plain,
            chunks,
            warnings: Vec::new(),
            parser_name: "t",
            parser_version: "t",
        };
        save_parsed(conn, &meta, "h", &parsed, 1).unwrap()
    }

    #[test]
    fn history_retention_uses_finish_time_and_preserves_documents() {
        let mut conn = crate::store::open_in_memory().unwrap();
        let document_id = save_doc(&mut conn, "/a/retained.txt", None);
        let now = 200 * 86_400_000;
        let cutoff = now - 90 * 86_400_000;
        // 状态、开始时间、结束时间、是否过期。覆盖毫秒边界与旧记录缺失时间。
        let cases = [
            ("done", 1, Some(cutoff - 1), true),
            ("failed", 1, Some(cutoff - 1), true),
            ("cancelled", 1, Some(cutoff - 1), true),
            ("done", 1, Some(cutoff), false),
            ("failed", 1, Some(cutoff + 1), false),
            ("cancelled", 1, Some(now), false),
            ("running", 1, None, false),
            ("done", cutoff - 1, None, true),
            ("done", cutoff, None, false),
        ];
        let mut retained = Vec::new();
        for (status, started, finished, expired) in cases {
            let run = create_run(&conn, "files", None, 1, started).unwrap();
            conn.execute(
                "UPDATE import_runs SET status = ?2, finished_at = ?3 WHERE id = ?1",
                params![run, status, finished],
            )
            .unwrap();
            upsert_item(
                &conn,
                run,
                Path::new("/a/retained.txt"),
                "ok",
                None,
                None,
                Some(document_id),
                started,
            )
            .unwrap();
            if !expired {
                retained.push(run);
            }
        }

        assert_eq!(prune_import_history(&conn, now).unwrap(), 4);
        for sql in [
            "SELECT id FROM import_runs ORDER BY id",
            "SELECT run_id FROM import_items ORDER BY run_id",
        ] {
            let actual = conn
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(actual, retained);
        }
        assert_eq!(prune_import_history(&conn, now).unwrap(), 0);
        assert_eq!(list_documents(&conn).unwrap().len(), 1);
        assert_eq!(
            get_plain_text(&conn, document_id).unwrap().unwrap(),
            "合同正文"
        );
        assert_eq!(stats(&conn).unwrap().chunks, 1);
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"合同\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 1);
    }

    #[test]
    fn source_root_groups_and_standalone_are_separated() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/a/1.txt", Some("/a"));
        save_doc(&mut conn, "/a/2.txt", Some("/a"));
        save_doc(&mut conn, "/b/3.txt", Some("/b"));
        save_doc(&mut conn, "/loose.txt", None);

        let grouped = list_documents_by_source_root(&conn).unwrap();
        assert_eq!(grouped.len(), 2, "应有两个来源文件夹");
        assert_eq!(grouped[0].0, "/a");
        assert_eq!(grouped[0].1.len(), 2);
        assert_eq!(grouped[1].0, "/b");
        assert_eq!(grouped[1].1.len(), 1);

        let standalone = list_standalone_documents(&conn).unwrap();
        assert_eq!(standalone.len(), 1);
        assert_eq!(standalone[0].file_name, "loose.txt");
        assert_eq!(standalone[0].source_root, None);
    }

    #[test]
    fn delete_source_root_removes_only_that_folder() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/a/1.txt", Some("/a"));
        save_doc(&mut conn, "/a/2.txt", Some("/a"));
        save_doc(&mut conn, "/b/3.txt", Some("/b"));
        save_doc(&mut conn, "/loose.txt", None);

        let removed = delete_source_root(&mut conn, "/a").unwrap();
        assert_eq!(removed, 2);

        // 只剩 /b 与单独文件。
        assert_eq!(list_documents_by_source_root(&conn).unwrap().len(), 1);
        assert_eq!(list_standalone_documents(&conn).unwrap().len(), 1);

        // FTS 也不该留下被删文档的行(否则完整性检查会报孤儿)。
        let report = crate::maintain::check_integrity(&conn).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());
    }

    #[test]
    fn delete_source_root_does_not_touch_similarly_named_folders() {
        // 精确匹配:删 /a 不能把 /a-b 或 /ab 一起带走。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/a/1.txt", Some("/a"));
        save_doc(&mut conn, "/a-b/2.txt", Some("/a-b"));
        save_doc(&mut conn, "/ab/3.txt", Some("/ab"));

        assert_eq!(delete_source_root(&mut conn, "/a").unwrap(), 1);
        let left = list_documents_by_source_root(&conn).unwrap();
        assert_eq!(left.len(), 2, "只应删掉 /a");
        assert!(left.iter().any(|(r, _)| r == "/a-b"));
        assert!(left.iter().any(|(r, _)| r == "/ab"));
    }

    #[test]
    fn delete_source_root_with_no_match_is_a_noop() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/a/1.txt", Some("/a"));
        assert_eq!(delete_source_root(&mut conn, "/nowhere").unwrap(), 0);
        assert_eq!(list_documents_by_source_root(&conn).unwrap().len(), 1);
    }

    #[test]
    fn reimport_updates_source_root_assignment() {
        // 同一文件先从文件夹导入、再单独导入 → 归属应改判为「单独文件」。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/a/1.txt", Some("/a"));
        assert_eq!(list_documents_by_source_root(&conn).unwrap().len(), 1);

        save_doc(&mut conn, "/a/1.txt", None);
        assert_eq!(
            list_documents_by_source_root(&conn).unwrap().len(),
            0,
            "应已改判为单独文件"
        );
        assert_eq!(list_standalone_documents(&conn).unwrap().len(), 1);
    }

    /// 与 save_doc 同款,但带一份非空 markdown 供开关断言。
    fn save_doc_with_markdown(
        conn: &mut Connection,
        path: &str,
        source_root: Option<&str>,
        markdown: &str,
    ) -> i64 {
        let meta = FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: "txt".to_owned(),
            file_type: FileType::Text,
            file_size: 1,
            file_mtime_ms: 1,
            source_root: source_root.map(str::to_owned),
        };
        let plain = text::markdown_to_plain(markdown);
        let title = text::extract_title(&plain, &meta.stem());
        let chunks = crate::chunk::chunk_document(&title, &plain);
        let parsed = ParsedDocument {
            title,
            markdown: markdown.to_owned(),
            plain,
            chunks,
            warnings: Vec::new(),
            parser_name: "t",
            parser_version: "t",
        };
        save_parsed(conn, &meta, "h", &parsed, 1).unwrap()
    }

    fn stored_markdown(conn: &Connection, document_id: i64) -> String {
        conn.query_row(
            "SELECT markdown FROM document_contents WHERE document_id = ?1",
            params![document_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn markdown_is_not_persisted_by_default() {
        let mut conn = crate::store::open_in_memory().unwrap();
        assert!(!save_markdown_enabled(&conn));

        let id = save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 标题\n正文内容");
        assert_eq!(stored_markdown(&conn, id), "", "默认应只存 plain_text");
        // 纯文本与检索不受影响。
        assert_eq!(
            get_plain_text(&conn, id).unwrap().unwrap(),
            "标题\n\n正文内容"
        );
    }

    #[test]
    fn save_markdown_setting_controls_persistence() {
        let mut conn = crate::store::open_in_memory().unwrap();
        set_setting(&conn, "save_markdown", "1").unwrap();
        assert!(save_markdown_enabled(&conn));

        let id = save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 标题\n正文内容");
        assert_eq!(stored_markdown(&conn, id), "# 标题\n正文内容");

        // 关掉后重解析同一文件:正文照常重写,markdown 被清成空串。
        set_setting(&conn, "save_markdown", "0").unwrap();
        assert!(!save_markdown_enabled(&conn));
        save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 标题\n正文内容v2");
        assert_eq!(
            stored_markdown(&conn, id),
            "",
            "关闭后重导入应不再存 markdown"
        );
        assert_eq!(
            get_plain_text(&conn, id).unwrap().unwrap(),
            "标题\n\n正文内容v2"
        );
    }

    /// 查询某个短语在 FTS 里的命中行数(验证词元级行为,不读列值)。
    fn match_count(conn: &Connection, phrase: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH ?1",
            [format!("\"{phrase}\"")],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn reimport_replaces_stale_index_terms() {
        // contentless-delete:同一路径重导入时,先 DELETE 同 rowid 行——
        // 旧词元必须随之消失,不能留下可命中的残留。
        let mut conn = crate::store::open_in_memory().unwrap();
        let id = save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 旧标题\n\n独有旧词");
        let same_id = save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 新标题\n\n独有新词");
        assert_eq!(id, same_id, "重导入应复用同一 documents.id");

        assert_eq!(match_count(&conn, "独有旧词"), 0, "旧正文词元应已清掉");
        assert_eq!(match_count(&conn, "旧标题"), 0, "旧标题词元应已清掉");
        assert_eq!(match_count(&conn, "独有新词"), 1);
        assert_eq!(match_count(&conn, "新标题"), 1);
    }

    #[test]
    fn delete_document_removes_index_terms() {
        // 删除路径(contentless-delete 按 rowid 直接清词元)不依赖回读旧值,
        // 删完后被删文档的词元不再可命中。
        let mut conn = crate::store::open_in_memory().unwrap();
        let id_a = save_doc_with_markdown(&mut conn, "/d/a.txt", None, "# 文档甲\n\n独有词甲");
        save_doc_with_markdown(&mut conn, "/d/b.txt", None, "# 文档乙\n\n独有词乙");

        delete_document(&mut conn, id_a).unwrap();

        assert_eq!(match_count(&conn, "独有词甲"), 0, "被删文档的词元应已清掉");
        assert_eq!(match_count(&conn, "独有词乙"), 1);
        assert!(
            crate::maintain::check_integrity(&conn)
                .unwrap()
                .is_consistent()
        );
    }

    #[test]
    fn delete_source_root_removes_index_terms() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc_with_markdown(&mut conn, "/a/1.txt", Some("/a"), "# 甲\n\n文件夹独有词");
        let meta_other = save_doc(&mut conn, "/b/2.txt", Some("/b"));
        let _ = meta_other;

        assert_eq!(delete_source_root(&mut conn, "/a").unwrap(), 1);
        assert_eq!(match_count(&conn, "文件夹独有词"), 0);
        assert_eq!(match_count(&conn, "合同"), 1, "其它文件夹的词元不受影响");
    }
}
