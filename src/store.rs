//! 索引库(SQLite + FTS5)的打开、PRAGMA 与建表。
//!
//! 「打开索引库」在应用里只有 `open`/`open_in_memory` 这一个入口:FTS5 自定义
//! tokenizer 的注册是 per-connection 的,任何未经注册就直接 `MATCH` 的连接都会
//! 报「no such tokenizer」,因此读连接与写连接都必须从这里走。
//!
//! schema 单版本管理(见 docs/plan.md §5.2):版本号写在 `settings.schema_version`,
//! 打开到版本不兼容的库时直接报中文错误,不做自动迁移。版本检查在建表之前,
//! 因此拒绝一个旧库时不会在它里面留下任何新表。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::tokenize;

/// 当前 schema 版本;写入 `settings` 表,版本不一致时拒绝打开。
///
/// 版本 2 把全文索引从「每分块一行」改成「每文档一行」(`documents_fts`)。
/// 索引语义变了(FTS 的 AND/OR/NOT 从分块级升到文档级),旧库无法就地沿用。
/// 版本 3 给 documents 加 `source_root`:记录文档是被哪个「已添加文件夹」导入的
/// (NULL = 单独添加的文件)。旧的「文件夹 → 文档」归属关系没存过,无法补,
/// 但加上列后新导入就能用了;旧库升上来时存量文档一律归到「单独文件」页。
pub const SCHEMA_VERSION: &str = "3";

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
///
/// 顺序有意为之:先比对已有库的版本,再建表。反过来的话,拒绝一个旧库之前
/// 已经把新表建了进去,旧版本程序再打开同一个库就会看到一个半新半旧的库。
pub fn ensure_schema(connection: &Connection) -> anyhow::Result<()> {
    if let Some(v) = stored_schema_version(connection)?
        && v != SCHEMA_VERSION
        && !can_upgrade_from(&v)
    {
        bail!(
            "索引文件版本不兼容:期望 schema_version = {SCHEMA_VERSION},实际为 {v};请使用与索引版本匹配的程序版本,或删除旧索引后重建"
        );
    }

    // 顺序不能换:旧库的 documents 还没有 source_root,而 SCHEMA_SQL 里已经有
    // `CREATE INDEX ... ON documents(source_root)`——先建表/索引会因缺列直接报
    // “no such column”。所以先把缺的列补上,再跑 SCHEMA_SQL(CREATE TABLE/INDEX
    // 都是 IF NOT EXISTS,对新库和已升上来的旧库都幂等)。
    add_missing_columns(connection)?;
    connection
        .execute_batch(SCHEMA_SQL)
        .context("创建索引库表结构失败")?;

    crate::normalize::normalize(connection)?;

    if stored_schema_version(connection)?.as_deref() != Some(SCHEMA_VERSION) {
        connection
            .execute(
                "INSERT INTO settings(key, value) VALUES ('schema_version', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION],
            )
            .context("写入 schema_version 失败")?;
    }
    crate::repo::prune_import_history(connection, crate::repo::now_ms())?;
    Ok(())
}

/// 能否从 `from` 就地升级到当前版本。
///
/// 只允许**纯追加列**的那一步:v2 → v3 只是给 `documents` 加了可空的
/// `source_root`,不动任何已有数据,所以升级不丢索引、不需要重新导入。
/// v1 → v2 改的是 FTS 表结构(每分块一行 → 每文档一行),语义变了、无法
/// 就地沿用,因此继续拒绝并提示重建。
fn can_upgrade_from(from: &str) -> bool {
    from == "2"
}

/// 把当前 schema 里有、而旧库缺的列补上(CREATE TABLE IF NOT EXISTS 不会动已存在的表)。
///
/// 幂等:先查 `pragma_table_info`,缺才 `ALTER TABLE ADD COLUMN`。
fn add_missing_columns(connection: &Connection) -> anyhow::Result<()> {
    // (表, 列, 列定义)— 只列可空、无默认值的追加列,ALTER 对已有行写入 NULL。
    const ADDED: &[(&str, &str, &str)] = &[("documents", "source_root", "TEXT")];
    for (table, column, definition) in ADDED {
        // 表还不存在(全新库):什么都不做,交给后面的 SCHEMA_SQL 建成带该列的表。
        let table_exists: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .with_context(|| format!("检查表 {table} 是否存在失败"))?;
        if !table_exists {
            continue;
        }
        let exists: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
                rusqlite::params![table, column],
                |row| row.get(0),
            )
            .with_context(|| format!("检查 {table}.{column} 失败"))?;
        if !exists {
            connection
                .execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {definition}"
                ))
                .with_context(|| format!("给 {table} 添加列 {column} 失败"))?;
            log::info!("已为存量索引补上 {table}.{column}");
        }
    }
    Ok(())
}

/// 读 `settings.schema_version`;库还没有 `settings` 表(全新文件)时返回 None。
fn stored_schema_version(connection: &Connection) -> anyhow::Result<Option<String>> {
    let has_settings: bool = connection
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'settings'",
            [],
            |row| row.get(0),
        )
        .context("读取 sqlite_master 失败")?;
    if !has_settings {
        return Ok(None);
    }
    let version = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("读取 schema_version 失败")?;
    Ok(version)
}

// docs/plan.md §5.2 的单版本表结构(STRICT 表);追加本阶段定的四个辅助索引。
// chunks 是**展示**分块:检索命中后按它把原文切成片段、并给出标题路径。
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
  -- 该文档是从哪个「已添加文件夹」导入的(规范化绝对路径);
  -- NULL = 用「添加文件」单独加进来的。资料库页据此分两个页签。
  source_root TEXT,
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

-- 普通 FTS5 表:rowid 显式取 documents.id,**一行 = 一篇文档**。
-- 因此 FTS 的隐式 AND / OR / NOT 都是文档级语义(多词只要同篇命中即可),
-- 不会因为分块边界丢掉召回。content 就是 document_contents.plain_text 全文,
-- 高亮由检索层直接在原文上定位字面量,不用 FTS5 的 highlight()。
-- tokenizer 'rsou' 由本程序注册(参数 '0' = 关闭拼音,与 wsou 的 simple 0 对齐)。
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
  title, content,
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

-- 同版本存量库也清除闲置或被 UNIQUE 前缀覆盖的索引。
DROP INDEX IF EXISTS idx_documents_canonical_path;
DROP INDEX IF EXISTS idx_chunks_document_id;
DROP INDEX IF EXISTS idx_import_items_run_status;

-- 资料库页按 source_root 分页签/建树,加索引避免每次全表扫。
CREATE INDEX IF NOT EXISTS idx_documents_source_root ON documents(source_root);
CREATE INDEX IF NOT EXISTS idx_documents_parse_status ON documents(parse_status);
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
                    "INSERT INTO documents_fts(rowid, content) VALUES (1, '中文索引')",
                    [],
                )
                .unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"索引\"'",
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
                    "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"索引\"'",
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

    fn assert_required_indexes_only(connection: &Connection) {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%' ORDER BY name")
            .unwrap();
        let names: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            names,
            [
                "idx_documents_parse_status",
                "idx_documents_source_root",
                "idx_import_items_document_id"
            ]
        );
    }

    #[test]
    fn new_database_omits_unused_indexes() {
        assert_required_indexes_only(&open_in_memory().unwrap());
    }

    #[test]
    fn existing_v3_database_drops_unused_indexes_without_losing_data() {
        let path = temp_db("drop-unused-indexes");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute_batch(
                    "CREATE INDEX idx_documents_canonical_path ON documents(canonical_path);
                     CREATE INDEX idx_chunks_document_id ON chunks(document_id);
                     CREATE INDEX idx_import_items_run_status ON import_items(run_id, status);
                     INSERT INTO documents(id, path, canonical_path, file_name, ext, file_type,
                         file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at)
                         VALUES (1, '/a.txt', '/a.txt', 'a.txt', 'txt', 'text', 1, 1, 'h', 'parsed', 1, 1);
                     INSERT INTO document_contents(document_id, markdown, plain_text)
                         VALUES (1, '# 正文', '正文');
                     INSERT INTO chunks(document_id, chunk_index, start_offset, end_offset)
                         VALUES (1, 0, 0, 6);
                     INSERT INTO documents_fts(rowid, title, content) VALUES (1, 'a', '正文');
                     INSERT INTO import_runs(id, kind, status, started_at) VALUES (1, 'files', 'running', 1);
                     INSERT INTO import_items(run_id, path, status, document_id, updated_at)
                         VALUES (1, '/a.txt', 'ok', 1, 1);",
                )
                .unwrap();
        }
        // 只读打开不能修改旧库的索引。
        {
            let connection = open(&path, OpenMode::ReadOnly).unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name IN
                     ('idx_documents_canonical_path', 'idx_chunks_document_id', 'idx_import_items_run_status')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 3);
        }
        // 同版本读写打开即清理,后续重复打开仍幂等。
        for _ in 0..2 {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            assert_required_indexes_only(&connection);
            assert_eq!(
                stored_schema_version(&connection).unwrap().as_deref(),
                Some("3")
            );
            let markdown: String = connection
                .query_row(
                    "SELECT markdown FROM document_contents WHERE document_id = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(markdown, "# 正文");
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM documents d JOIN chunks c ON c.document_id = d.id
                     JOIN import_items i ON i.document_id = d.id JOIN import_runs r ON r.id = i.run_id
                     WHERE d.id IN (SELECT rowid FROM documents_fts WHERE documents_fts MATCH '正文')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            assert!(connection.execute("INSERT INTO chunks(document_id, chunk_index, start_offset, end_offset) VALUES (1, 0, 0, 6)", []).is_err());
            assert!(connection.execute("INSERT INTO import_items(run_id, path, status, updated_at) VALUES (1, '/a.txt', 'ok', 1)", []).is_err());
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
    fn rejecting_a_stale_version_leaves_no_new_tables() {
        // 旧库(版本 1)只应被拒绝,不应被建进 documents_fts。
        let path = temp_db("stale");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute("DROP TABLE IF EXISTS documents_fts", [])
                .unwrap();
            connection
                .execute(
                    "UPDATE settings SET value = '1' WHERE key = 'schema_version'",
                    [],
                )
                .unwrap();
        }
        let error = open(&path, OpenMode::ReadWrite).unwrap_err();
        assert!(format!("{error:#}").contains("索引文件版本不兼容"));

        let connection = Connection::open(&path).unwrap();
        let created: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'documents_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created, 0, "被拒绝的旧库不应被写入新表");
        drop(connection);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v2_database_upgrades_in_place_without_losing_data() {
        // v2 的库(没有 source_root 列)应能就地升到 v3:加列、保留全部数据。
        let path = temp_db("upgrade-v2");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            // 模拟 v2:删掉新索引与列,把版本号退回去,再写一行数据。
            // 必须先 DROP INDEX:否则 ALTER ... DROP COLUMN 会因索引仍引用该列失败。
            connection
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_documents_source_root; \
                     ALTER TABLE documents DROP COLUMN source_root; \
                     UPDATE settings SET value = '2' WHERE key = 'schema_version';",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO documents(path, canonical_path, file_name, ext, file_type, \
                     file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at) \
                     VALUES ('/d/a.txt', '/d/a.txt', 'a.txt', 'txt', 'text', 1, 1, 'h', 'parsed', 1, 1)",
                    [],
                )
                .unwrap();
        }

        // 重新打开 → 自动升级
        let connection = open(&path, OpenMode::ReadWrite).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // 数据还在,新列存在且为 NULL(旧库无从知道归属)。
        let (file_name, source_root): (String, Option<String>) = connection
            .query_row("SELECT file_name, source_root FROM documents", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(file_name, "a.txt");
        assert_eq!(source_root, None, "存量文档应归到「单独文件」");

        // 索引也应被重建出来(升级不该动索引定义)。
        let has_index: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='index' AND name='idx_documents_source_root'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_index, "升级应补上新索引吗(execute_batch 会建)");
        drop(connection);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn upgrade_is_idempotent() {
        let path = temp_db("upgrade-twice");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_documents_source_root; \
                     ALTER TABLE documents DROP COLUMN source_root; \
                     UPDATE settings SET value = '2' WHERE key = 'schema_version';",
                )
                .unwrap();
        }
        // 连开三次都不应报错(每次都会检查缺列)。
        for _ in 0..3 {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('documents') WHERE name='source_root'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "source_root 应恰好存在一列");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v1_is_still_rejected_because_fts_semantics_changed() {
        // v1 → v2 改的是 FTS 表结构,不能就地升:必须明确拒绝。
        assert!(!can_upgrade_from("1"));
        assert!(!can_upgrade_from("0"));
        assert!(!can_upgrade_from("999"));
        assert!(can_upgrade_from("2"));
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
