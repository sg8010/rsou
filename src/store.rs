//! 索引库(SQLite + FTS5)的打开、PRAGMA 与建表。
//!
//! 「打开索引库」在应用里只有 `open`/`open_in_memory` 这一个入口:FTS5 自定义
//! tokenizer 的注册是 per-connection 的,任何未经注册就直接 `MATCH` 的连接都会
//! 报「no such tokenizer」,因此读连接与写连接都必须从这里走。
//!
//! schema 单版本管理:版本号写在 `settings.schema_version`,
//! 打开到版本不兼容的库时直接报中文错误,不做自动迁移。版本检查在建表之前,
//! 因此拒绝一个旧库时不会在它里面留下任何新表。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};

use crate::tokenize;

/// 当前 schema 版本;写入 `settings` 表,版本不一致时拒绝打开。
///
/// 版本 2 把全文索引从「每分块一行」改成「每文档一行」(`documents_fts`)。
/// 索引语义变了(FTS 的 AND/OR/NOT 从分块级升到文档级),旧库无法就地沿用。
/// 版本 3 给 documents 加 `source_root`:记录文档是被哪个「已添加文件夹」导入的
/// (NULL = 单独添加的文件)。旧的「文件夹 → 文档」归属关系没存过,无法补,
/// 但加上列后新导入就能用了;旧库升上来时存量文档一律归到「单独文件」页。
/// 版本 4 把 documents_fts 改成 contentless-delete 表(`content=''`):
/// 全文只在 `document_contents.plain_text` 存一份,FTS 不再重复存约一倍体积。
/// 旧表整个丢弃(索引由启动后的维护任务重灌,不在 open 里做全量重建),
/// 检索语义不变。
/// 版本 5 合并 ASCII 字母数字词元;旧索引清空后由维护任务重建。
pub const SCHEMA_VERSION: &str = "5";

/// 待重建标志:迁移丢弃旧 FTS 表时置 '1',`maintain::rebuild_fts` 提交时清掉。
/// GUI 启动读到它即自动发起重建;CLI 各命令打印提示。
pub(crate) const FTS_REBUILD_PENDING_KEY: &str = "fts_rebuild_pending";

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
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .context("开启表结构迁移事务失败")?;
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
    // v3 及以前更换普通 FTS 表,v4 更换旧分词规则。先丢弃旧索引,
    // SCHEMA_SQL 再创建新表;正文保留,由维护任务重建。
    drop_legacy_fts(connection)?;
    connection
        .execute_batch(SCHEMA_SQL)
        .context("创建索引库表结构失败")?;

    // 兼容此前迁移在 DROP 与写标志之间中断的库。必须在导入新文档前持久化,
    // 否则新文档填入 FTS 后,「空索引」的兜底判断就无法发现旧文档漏索引。
    connection
        .execute(
            "INSERT INTO settings(key, value) \
             SELECT ?1, '1' WHERE NOT EXISTS(SELECT 1 FROM documents_fts) \
             AND EXISTS(SELECT 1 FROM documents WHERE parse_status = 'parsed') \
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            [FTS_REBUILD_PENDING_KEY],
        )
        .context("保存待重建状态失败")?;

    if stored_schema_version(connection)?.as_deref() != Some(SCHEMA_VERSION) {
        connection
            .execute(
                "INSERT INTO settings(key, value) VALUES ('schema_version', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION],
            )
            .context("写入 schema_version 失败")?;
    }
    tx.commit().context("提交表结构迁移失败")?;
    // 路径归一化有自己的事务,需在表结构迁移提交后运行。
    crate::normalize::normalize(connection)?;
    crate::repo::prune_import_history(connection, crate::repo::now_ms())?;
    Ok(())
}

/// 能否从 `from` 就地升级到当前版本。
///
/// - v2 → v5:补 `source_root` 列并更换 FTS 表;
/// - v3 → v5:更换 FTS 表结构与分词规则;
/// - v4 → v5:更换分词规则,丢弃旧索引等待重建。
///
/// 都不丢文档数据;旧 FTS 表丢弃后索引由维护任务重建(见 `drop_legacy_fts`)。
/// v1 的 chunks_fts 是每分块一行的另一套结构,继续拒绝并提示重建。
fn can_upgrade_from(from: &str) -> bool {
    matches!(from, "2" | "3" | "4")
}

/// v4 迁移:旧 documents_fts 是普通 FTS5 表(带 %_content 影子表,整存一份
/// 全文);contentless-delete 表没有 %_content——以影子表是否存在判定旧格式,
/// 同时识别 v4 的旧分词规则并丢弃对应索引。
///
/// 发现旧表即 DROP(影子表随主表一起删),新表交给 SCHEMA_SQL 的
/// CREATE IF NOT EXISTS;同时置 `fts_rebuild_pending`,索引由启动后的维护
/// 任务重建——全量重灌若在 open 里做会长时间卡住 GUI 启动。调用方将 DROP、
/// 新表创建、重建标志和版本更新放在同一事务,中断时旧表也会回滚恢复。
fn drop_legacy_fts(connection: &Connection) -> anyhow::Result<()> {
    let legacy: bool = connection
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_master \
             WHERE type = 'table' AND name = 'documents_fts_content'",
            [],
            |row| row.get(0),
        )
        .context("检查旧全文索引表结构失败")?;
    let old_tokens = stored_schema_version(connection)?.as_deref() == Some("4");
    if !legacy && !old_tokens {
        return Ok(());
    }
    connection
        .execute_batch("DROP TABLE documents_fts")
        .context("丢弃旧全文索引表失败")?;
    connection
        .execute(
            "INSERT INTO settings(key, value) VALUES (?1, '1') \
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            [FTS_REBUILD_PENDING_KEY],
        )
        .context("写入重建标志失败")?;
    log::info!("documents_fts 已升级表结构或分词规则,等待重建全文索引");
    Ok(())
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

// 单版本表结构(STRICT 表);另有四个辅助索引。
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
  markdown TEXT NOT NULL,               -- anydoc 产物;settings.save_markdown=1 才存,否则空串
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

-- contentless-delete FTS5 表:rowid 显式取 documents.id,**一行 = 一篇文档**。
-- 因此 FTS 的隐式 AND / OR / NOT 都是文档级语义(多词只要同篇命中即可),
-- 不会因为分块边界丢掉召回。
-- content='' + contentless_delete=1:写入的 title/content 只建索引、不落存储,
-- 全文只在 document_contents.plain_text 存一份(省约一倍体积);列值读回恒为
-- NULL,检索只用 MATCH/rank/rowid。删除按 rowid 直接回收词元,不需要像外部
-- 内容表那样先回读旧值——任何顺序都不会留残留。
-- tokenizer 'rsou' 由本程序注册(参数 '0' = 关闭拼音)。
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
  title, content,
  content = '',
  contentless_delete = 1,
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
                Some(SCHEMA_VERSION)
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
        assert!(can_upgrade_from("3"));
    }

    #[test]
    fn v4_tokenizer_upgrade_blocks_search_until_rebuild() {
        let path = temp_db("tokenizer-v4");
        let mut conn = open(&path, OpenMode::ReadWrite).unwrap();
        conn.execute_batch(
            "UPDATE settings SET value='4' WHERE key='schema_version';
            INSERT INTO documents(path,canonical_path,file_name,title,ext,file_type,
                file_size,file_mtime_ms,content_hash,parse_status,created_at,updated_at)
                VALUES('/a.txt','/a.txt','a.txt','型号','txt','text',1,1,'h','parsed',1,1);
            INSERT INTO document_contents(document_id,markdown,plain_text) VALUES(1,'A4','A4');
            INSERT INTO documents_fts(rowid,title,content) VALUES(1,'型号','A 4');
            CREATE TRIGGER abort_upgrade BEFORE UPDATE OF value ON settings
            WHEN NEW.key='schema_version' BEGIN SELECT RAISE(ABORT,'upgrade failure'); END;",
        )
        .unwrap();
        let ro = open(&path, OpenMode::ReadOnly).unwrap();
        assert!(crate::maintain::needs_fts_rebuild(&ro).unwrap());
        let req = crate::search::SearchRequest {
            query: "A4".into(),
            ..Default::default()
        };
        assert!(crate::search::search(&ro, &req).is_err());
        assert!(open(&path, OpenMode::ReadWrite).is_err());
        assert_eq!(stored_schema_version(&ro).unwrap().as_deref(), Some("4"));
        let old_count: i64 = ro
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"A 4\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 1);
        conn.execute_batch("DROP TRIGGER abort_upgrade").unwrap();
        ensure_schema(&conn).unwrap();
        assert!(crate::search::search_for_listing(&ro, &req).is_err());
        assert!(crate::maintain::needs_fts_rebuild(&conn).unwrap());
        // 重复开库不能清掉重建状态。
        ensure_schema(&conn).unwrap();
        assert!(crate::maintain::needs_fts_rebuild(&conn).unwrap());
        crate::maintain::rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        assert!(!crate::maintain::needs_fts_rebuild(&conn).unwrap());
        assert_eq!(crate::search::search(&ro, &req).unwrap().documents.len(), 1);
        let old_count: i64 = ro
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"A 4\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);
        ensure_schema(&conn).unwrap();
        assert!(!crate::maintain::needs_fts_rebuild(&conn).unwrap());
        drop(ro);
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn v3_database_migrates_fts_to_contentless_and_flags_rebuild() {
        // 模拟 v3:documents_fts 是普通表(带 _content 影子表、另存一份全文)。
        let path = temp_db("migrate-v3");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute_batch(
                    "DROP TABLE documents_fts;
                     CREATE VIRTUAL TABLE documents_fts USING fts5(title, content, tokenize='rsou 0');
                     UPDATE settings SET value = '3' WHERE key = 'schema_version';
                     INSERT INTO documents(path, canonical_path, file_name, title, ext, file_type,
                         file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at)
                         VALUES ('/d/a.txt', '/d/a.txt', 'a.txt', '甲标题', 'txt', 'text',
                                 1, 1, 'h', 'parsed', 1, 1);
                     INSERT INTO document_contents(document_id, markdown, plain_text)
                         VALUES (1, '# 甲标题', '甲标题 正文 alpha');
                     INSERT INTO documents_fts(rowid, title, content)
                         VALUES (1, '甲标题', '甲标题 正文 alpha');",
                )
                .unwrap();
            // 普通表确实带 _content 影子表。
            let shadow: bool = connection
                .query_row(
                    "SELECT count(*) > 0 FROM sqlite_master \
                     WHERE type = 'table' AND name = 'documents_fts_content'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(shadow, "模拟的普通表应有 %_content 影子表");
        }

        // 重新打开:旧表被丢弃、新表为空、置重建标志、升级到当前版本。
        let connection = open(&path, OpenMode::ReadWrite).unwrap();
        assert_eq!(
            stored_schema_version(&connection).unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );
        let shadow: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'documents_fts_content'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!shadow, "contentless-delete 表不应有 %_content 影子表");
        let fts_rows: i64 = connection
            .query_row("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_rows, 0, "迁移只丢弃旧索引,重建由维护任务做");
        assert_eq!(
            crate::repo::get_setting(&connection, FTS_REBUILD_PENDING_KEY)
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert!(crate::maintain::needs_fts_rebuild(&connection).unwrap());

        // 文档数据不动,重建后检索恢复。
        let mut connection = connection;
        let rows = crate::maintain::rebuild_fts(&mut connection, &mut |_, _| {}).unwrap();
        assert_eq!(rows, 1);
        assert!(!crate::maintain::needs_fts_rebuild(&connection).unwrap());
        let hits: i64 = connection
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH '\"alpha\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
        drop(connection);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_fts_migration_rolls_back_and_can_be_retried() {
        let path = temp_db("migration-rollback");
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            connection
                .execute_batch(
                    "DROP TABLE documents_fts;
                 CREATE VIRTUAL TABLE documents_fts USING fts5(title, content, tokenize='rsou 0');
                 INSERT INTO documents_fts(rowid, title, content) VALUES (1, 'old', 'alpha');
                 UPDATE settings SET value = '3' WHERE key = 'schema_version';
                 CREATE TRIGGER abort_migration BEFORE UPDATE OF value ON settings
                 WHEN NEW.key = 'schema_version'
                 BEGIN SELECT RAISE(ABORT, 'simulated migration failure'); END;",
                )
                .unwrap();
        }
        let error = open(&path, OpenMode::ReadWrite).unwrap_err();
        assert!(format!("{error:#}").contains("simulated migration failure"));
        {
            let connection = open(&path, OpenMode::ReadOnly).unwrap();
            assert_eq!(
                stored_schema_version(&connection).unwrap().as_deref(),
                Some("3")
            );
            assert_eq!(
                crate::repo::get_setting(&connection, FTS_REBUILD_PENDING_KEY).unwrap(),
                None
            );
            // 能读回原文并 MATCH:旧 FTS 主表、影子表和索引词元一并恢复。
            let content: String = connection
                .query_row(
                    "SELECT content FROM documents_fts WHERE documents_fts MATCH 'alpha'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(content, "alpha");
        }
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("DROP TRIGGER abort_migration")
                .unwrap();
        }
        {
            let connection = open(&path, OpenMode::ReadWrite).unwrap();
            assert_eq!(
                stored_schema_version(&connection).unwrap().as_deref(),
                Some(SCHEMA_VERSION)
            );
            assert_eq!(
                crate::repo::get_setting(&connection, FTS_REBUILD_PENDING_KEY)
                    .unwrap()
                    .as_deref(),
                Some("1")
            );
            let shadow: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'documents_fts_content')",
                [], |row| row.get(0),
            ).unwrap();
            assert!(!shadow);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn interrupted_migration_persists_pending_before_new_import() {
        // 覆盖旧版本 DROP 后退出,以及已建空表但没有重建标志的两种遗留状态。
        for missing_fts in [true, false] {
            let path = temp_db(if missing_fts {
                "recover-missing-fts"
            } else {
                "recover-empty-fts"
            });
            {
                let connection = open(&path, OpenMode::ReadWrite).unwrap();
                connection.execute_batch(
                    "INSERT INTO documents(id, path, canonical_path, file_name, ext, file_type,
                         file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at)
                         VALUES (1, '/old.txt', '/old.txt', 'old.txt', 'txt', 'text', 1, 1, 'h', 'parsed', 1, 1);
                     INSERT INTO document_contents(document_id, markdown, plain_text) VALUES (1, '', 'oldword');",
                ).unwrap();
                if missing_fts {
                    connection
                        .execute_batch(
                            "DROP TABLE documents_fts;
                         UPDATE settings SET value = '3' WHERE key = 'schema_version';",
                        )
                        .unwrap();
                }
            }
            {
                let connection = open(&path, OpenMode::ReadWrite).unwrap();
                assert_eq!(
                    crate::repo::get_setting(&connection, FTS_REBUILD_PENDING_KEY)
                        .unwrap()
                        .as_deref(),
                    Some("1")
                );
                connection.execute_batch(
                    "INSERT INTO documents(id, path, canonical_path, file_name, ext, file_type,
                         file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at)
                         VALUES (2, '/new.txt', '/new.txt', 'new.txt', 'txt', 'text', 1, 1, 'h2', 'parsed', 1, 1);
                     INSERT INTO document_contents(document_id, markdown, plain_text) VALUES (2, '', 'newword');
                     INSERT INTO documents_fts(rowid, title, content) VALUES (2, 'new', 'newword');",
                ).unwrap();
            }
            {
                let mut connection = open(&path, OpenMode::ReadWrite).unwrap();
                assert!(crate::maintain::needs_fts_rebuild(&connection).unwrap());
                assert_eq!(
                    crate::maintain::rebuild_fts(&mut connection, &mut |_, _| {}).unwrap(),
                    2
                );
                assert!(!crate::maintain::needs_fts_rebuild(&connection).unwrap());
                let hits: i64 = connection
                    .query_row(
                        "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH 'oldword'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(hits, 1);
            }
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
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
