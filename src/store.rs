//! 索引库(SQLite + FTS5)的打开、PRAGMA 与建表。
//!
//! 「打开索引库」在应用里只有 `open`/`open_in_memory` 这一个入口:FTS5 自定义
//! tokenizer 的注册是 per-connection 的,任何未经注册就直接 `MATCH` 的连接都会
//! 报「no such tokenizer」,因此读连接与写连接都必须从这里走。
//!
//! schema 单版本管理(见 docs/plan.md §5.2):版本号写在 `settings.schema_version`,
//! 打开到版本不兼容的库时直接报中文错误,不做自动迁移。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags};

use crate::tokenize;

/// 当前 schema 版本;写入 `settings` 表,版本不一致时拒绝打开。
pub const SCHEMA_VERSION: &str = "1";

/// 数据目录布局:`data_dir/index.sqlite3` + `data_dir/tmp/`。
#[derive(Debug, Clone)]
pub struct DataDirs {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub tmp_dir: PathBuf,
}

/// 解析应用数据目录。
///
/// 优先级:`RSOU_DATA_DIR`(测试与 CLI 覆盖用)> 平台惯例目录 > `temp_dir()/rsou`。
/// - Linux:`$XDG_DATA_HOME/rsou`,否则 `~/.local/share/rsou`
/// - Windows:`%LOCALAPPDATA%\rsou`,否则 `%APPDATA%\rsou`
pub fn data_dirs() -> DataDirs {
    let base = std::env::var_os("RSOU_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(platform_data_dir)
        .unwrap_or_else(|| std::env::temp_dir().join("rsou"));
    DataDirs {
        db_path: base.join("index.sqlite3"),
        tmp_dir: base.join("tmp"),
        data_dir: base,
    }
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join("rsou"))
}

#[cfg(not(target_os = "windows"))]
fn platform_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("rsou"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/rsou"))
}

/// 打开索引库的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// 读写:必要时创建目录与库文件、置 WAL、建 schema。
    ReadWrite,
    /// 只读:不创建任何文件,不建 schema(检索/统计连接用)。
    ReadOnly,
}

/// 打开索引库并保证连接可用。
///
/// 流程:建父目录(仅 ReadWrite)→ 打开连接 → 注册 `rsou` tokenizer →
/// PRAGMA(journal_mode=WAL 仅 ReadWrite、busy_timeout=5000、foreign_keys=ON、
/// synchronous=NORMAL)→ 建/校验 schema(仅 ReadWrite)。
pub fn open(path: &Path, mode: OpenMode) -> anyhow::Result<Connection> {
    if mode == OpenMode::ReadWrite
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
    }
    let connection = match mode {
        OpenMode::ReadWrite => Connection::open(path),
        OpenMode::ReadOnly => Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY),
    }
    .with_context(|| format!("打开索引库失败: {}", path.display()))?;
    configure(connection, mode).with_context(|| format!("初始化索引库失败: {}", path.display()))
}

/// 打开内存索引库(测试用),与 `open` 走同一条「注册 tokenizer + PRAGMA + schema」路径。
pub fn open_in_memory() -> anyhow::Result<Connection> {
    let connection = Connection::open_in_memory().context("打开内存索引库失败")?;
    configure(connection, OpenMode::ReadWrite).context("初始化内存索引库失败")
}

fn configure(connection: Connection, mode: OpenMode) -> anyhow::Result<Connection> {
    // 注册必须在任何含 tokenize='rsou' 的语句之前;per-connection。
    tokenize::register(&connection).context("注册 rsou tokenizer 失败")?;
    connection
        .busy_timeout(Duration::from_millis(5000))
        .context("设置 busy_timeout 失败")?;
    if mode == OpenMode::ReadWrite {
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("设置 journal_mode=WAL 失败")?;
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .context("设置 synchronous 失败")?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .context("设置 foreign_keys 失败")?;
    if mode == OpenMode::ReadWrite {
        ensure_schema(&connection)?;
    }
    Ok(connection)
}

/// 建立全部表与索引(幂等),并校验 schema 版本。
pub fn ensure_schema(connection: &Connection) -> anyhow::Result<()> {
    connection
        .execute_batch(SCHEMA_SQL)
        .context("创建索引库表结构失败")?;

    let version: Option<String> = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .ok();
    match version.as_deref() {
        Some(v) if v != SCHEMA_VERSION => bail!(
            "索引文件版本不兼容:期望 schema_version = {SCHEMA_VERSION},实际为 {v};请使用与索引版本匹配的程序版本,或删除旧索引后重建"
        ),
        Some(_) => {}
        None => {
            connection
                .execute(
                    "INSERT INTO settings(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION],
                )
                .context("写入 schema_version 失败")?;
        }
    }
    Ok(())
}

// docs/plan.md §5.2 的单版本表结构(STRICT 表);追加本阶段定的四个辅助索引。
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,            -- 规范化绝对路径
  canonical_path TEXT NOT NULL,         -- realpath,用于同文件去重
  file_name TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  ext TEXT NOT NULL,
  file_type TEXT NOT NULL,              -- word/excel/ppt/pdf/text/epub
  file_size INTEGER NOT NULL,
  file_mtime_ms INTEGER NOT NULL,
  content_hash TEXT NOT NULL,           -- 解析前 sha256,未变更即跳过
  parse_status TEXT NOT NULL CHECK (parse_status IN ('parsed','failed')),
  parse_error_code TEXT,
  parse_error_message TEXT,
  parser_name TEXT, parser_version TEXT,
  text_length INTEGER NOT NULL DEFAULT 0,
  chunk_count INTEGER NOT NULL DEFAULT 0,
  indexed_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS document_contents (
  document_id INTEGER PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  markdown TEXT NOT NULL,               -- anydoc 产物,可选展示
  plain_text TEXT NOT NULL,             -- 从 markdown 提取的纯文本,偏移基准
  warnings_json TEXT NOT NULL DEFAULT '[]'
) STRICT;

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY,
  document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  chunk_index INTEGER NOT NULL,
  context_header TEXT NOT NULL DEFAULT '',  -- 「标题 › H1 › H2」
  start_offset INTEGER NOT NULL,            -- 在 plain_text 中的字节偏移
  end_offset INTEGER NOT NULL,
  UNIQUE (document_id, chunk_index)
) STRICT;

-- 普通 FTS5 表:rowid 显式取 chunks.id,三列存分块原文。
-- tokenizer 'rsou' 由本程序注册(参数 '0' = 关闭拼音,与 wsou 的 simple 0 对齐)。
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  title, context_header, content,
  tokenize = 'rsou 0'
);

CREATE TABLE IF NOT EXISTS import_runs (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, root_path TEXT,
  status TEXT NOT NULL CHECK (status IN ('running','done','failed','cancelled')),
  total_files INTEGER NOT NULL DEFAULT 0, processed_files INTEGER NOT NULL DEFAULT 0,
  ok_files INTEGER NOT NULL DEFAULT 0, failed_files INTEGER NOT NULL DEFAULT 0,
  skipped_files INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER NOT NULL, finished_at INTEGER, message TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS import_items (
  id INTEGER PRIMARY KEY,
  run_id INTEGER NOT NULL REFERENCES import_runs(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','running','ok','failed','skipped')),
  error_code TEXT, error_message TEXT,
  document_id INTEGER REFERENCES documents(id) ON DELETE SET NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (run_id, path)
) STRICT;

CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;

CREATE INDEX IF NOT EXISTS idx_documents_canonical_path ON documents(canonical_path);
CREATE INDEX IF NOT EXISTS idx_documents_parse_status ON documents(parse_status);
CREATE INDEX IF NOT EXISTS idx_chunks_document_id ON chunks(document_id);
CREATE INDEX IF NOT EXISTS idx_import_items_run_status ON import_items(run_id, status);
CREATE INDEX IF NOT EXISTS idx_import_items_document_id ON import_items(document_id);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsou-store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("index.sqlite3")
    }

    #[test]
    fn readwrite_open_creates_schema_and_fts_table() {
        let path = temp_db("rw");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            // tokenizer 已注册:能建 rsou 表并向其中写入/查询。
            connection
                .execute(
                    "INSERT INTO chunks_fts(rowid, content) VALUES (1, '中文索引')",
                    [],
                )
                .unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"索引\"'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            let journal: String = connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(journal.to_lowercase(), "wal");
        }

        // 只读连接同样能 MATCH(注册是 per-connection 的,由 open 统一负责)。
        {
            let connection = open(&path, OpenMode::ReadOnly).unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"索引\"'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn import_items_document_id_index_exists() {
        let path = temp_db("idx");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            // import_items.document_id 是 ON DELETE SET NULL 外键,删文档时靠它避免全表扫。
            let exists: bool = connection
                .query_row(
                    "SELECT count(*) > 0 FROM sqlite_master \
                     WHERE type = 'index' AND name = 'idx_import_items_document_id'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "应创建 idx_import_items_document_id 索引");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn schema_version_mismatch_is_rejected_in_chinese() {
        let path = temp_db("ver");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute(
                    "UPDATE settings SET value = '999' WHERE key = 'schema_version'",
                    [],
                )
                .unwrap();
        }
        let error = open(&path, OpenMode::ReadWrite).unwrap_err();
        assert!(
            format!("{error:#}").contains("索引文件版本不兼容"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn data_dirs_respects_env_override() {
        let dir = std::env::temp_dir().join(format!("rsou-dirs-{}", std::process::id()));
        unsafe {
            std::env::set_var("RSOU_DATA_DIR", &dir);
        }
        let dirs = data_dirs();
        unsafe {
            std::env::remove_var("RSOU_DATA_DIR");
        }
        assert_eq!(dirs.data_dir, dir);
        assert_eq!(dirs.db_path, dir.join("index.sqlite3"));
        assert_eq!(dirs.tmp_dir, dir.join("tmp"));
    }
}
