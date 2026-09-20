//! 导入流水线:扫描 → 并行解析 → 串行写库(无 GUI 依赖)。
//!
//! 线程模型:
//! - `run_import` 在调用线程执行(GUI 把它放进 `std::thread`),自己开写连接;
//! - 文件读取、解析、文本化、分块、哈希在 rayon 线程池并行;
//! - 所有写库操作串行发生在调用线程,经有界 mpsc 通道消费 worker 结果
//!   (写库慢时通道给解析线程背压)。
//!
//! 取消:worker 在每个文件开工前检查 cancel 标志;单文件解析不可中断
//! (anydoc 没有取消接口),置位后已开始的文件会跑完,未开始的直接不开工。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use anyhow::Context;
use rayon::prelude::*;
use sha2::Digest;

use crate::chunk;
use crate::parse::{self, ParseError};
use crate::repo::{self, FileMeta, ParsedDocument};
use crate::text;

pub use crate::repo::ImportCounts;

/// 导入参数。
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// 单文件字节上限(默认 100 MiB)
    pub max_file_bytes: u64,
    /// true = 忽略 hash/mtime 跳过,全部重解析
    pub force: bool,
    /// 重新扫描时也跳过未变化的失败文件;force 优先。
    pub skip_unchanged_failed: bool,
    /// 单文件重试/更新时保留已有文件夹归属,不把它改成单独文件。
    pub preserve_source_root: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 100 * 1024 * 1024,
            force: false,
            skip_unchanged_failed: false,
            preserve_source_root: false,
        }
    }
}

/// 导入进度事件(写库线程逐条发给调用方)。
pub enum ImportEvent {
    /// 扫描完成,确定了文件总数
    Scanned { total: usize },
    /// 一个文件处理完毕
    FileDone {
        path: PathBuf,
        outcome: FileOutcome,
        counts: ImportCounts,
    },
    /// 全部结束(正常完成或被取消)
    Finished {
        run_id: i64,
        counts: ImportCounts,
        cancelled: bool,
    },
}

/// 单文件处理结果。
pub enum FileOutcome {
    Ok { document_id: i64 },
    Failed { code: String, message: String },
    Skipped,
}

/// 递归展开输入路径为待处理文件清单。
///
/// - 文件直接收(扩展名不支持的丢弃,不计数);
/// - 目录递归:跳过 `.` 开头的条目、`node_modules`、`$RECYCLE.BIN`、
///   `System Volume Information`,不跟随符号链接;
/// - 只收 `file_type_of` 支持的文件;按规范化路径去重、排序。
pub fn scan_paths(inputs: &[PathBuf]) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &["node_modules", "$RECYCLE.BIN", "System Volume Information"];
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<PathBuf> = inputs.to_vec();

    while let Some(path) = stack.pop() {
        // symlink_metadata 不跟随符号链接:链向目录的 symlink 不被递归。
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with('.') || SKIP_DIRS.contains(&name));
            if skip {
                continue;
            }
            if let Ok(read) = std::fs::read_dir(&path) {
                for entry in read.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with('.') {
                        continue;
                    }
                    stack.push(entry.path());
                }
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if parse::file_type_of(&path).is_none() {
            continue;
        }
        // 规范化用于去重与入库;失败时退回绝对路径。
        // 走 dunce 而不是 std:Windows 上 std 会返回 `\\?\C:\...` 形式,
        // 既不适合展示,也会让「目录前缀」过滤匹配不上(见 crate 注释)。
        let normalized = dunce::canonicalize(&path).unwrap_or_else(|_| absolute(&path));
        if seen.insert(normalized.clone()) {
            files.push(normalized);
        }
    }
    files.sort();
    files
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// 库里已有文档的跳过判定信息。
struct Existing {
    id: i64,
    file_size: i64,
    file_mtime_ms: i64,
    content_hash: String,
    parsed: bool,
    source_root: Option<String>,
}

/// worker 产出的单文件结果(内部消息;公开事件是 FileOutcome)。
enum FileResult {
    /// 按 mtime/size 或 hash 判定未变更;touch = 需要回写新的 size/mtime
    Skipped {
        meta: FileMeta,
        document_id: i64,
        touch: bool,
    },
    Done {
        meta: FileMeta,
        hash: String,
        result: Result<ParsedDocument, ParseError>,
    },
}

/// 执行一次导入。在调用线程同步运行;事件经 `on_event` 逐条回调。
pub fn run_import(
    db_path: &Path,
    inputs: Vec<PathBuf>,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
    on_event: &mut dyn FnMut(ImportEvent),
) -> anyhow::Result<ImportCounts> {
    let mut conn = crate::store::open(db_path, crate::store::OpenMode::ReadWrite)
        .context("导入前打开索引库失败")?;

    let files = scan_paths(&inputs);
    on_event(ImportEvent::Scanned { total: files.len() });

    let now = repo::now_ms();
    let (kind, root) = classify_inputs(&inputs);
    let run_id = repo::create_run(&conn, kind, root.as_deref(), files.len(), now)?;
    // 本次导入的「来源文件夹」:只有「单个目录」这种输入才算文件夹导入。
    // 选多个文件、或「文件+文件夹」混合时,一律按单独文件处理——那种混合
    // 输入没有唯一的归属,硬指定会让树形归属失真。
    //
    // 注意 root 是**输入时用户选的**路径,可能与文档的 canonical 路径不同形
    // (符号链接/相对路径)。这里统一用 dunce 规范化一次,保证与 documents.path
    // 同口径,树形分组才能对得上。
    let source_root: Option<String> = match (&kind, &root) {
        (&"folder", Some(dir)) => {
            let normalized = dunce::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            Some(normalized.to_string_lossy().into_owned())
        }
        _ => None,
    };
    // 全部 item 先记 pending,跑完/失败/跳过时逐个改写。
    seed_items(&mut conn, run_id, &files, now)?;

    // 跳过判定用的一次性快照(读已有记录,不逐个查库)。
    let existing = load_existing(&conn)?;

    let mut counts = ImportCounts {
        total: files.len(),
        ..ImportCounts::default()
    };
    let mut first_error: Option<anyhow::Error> = None;

    // 有界通道:写库慢时给解析线程背压;消费者在写库出错后仍排空通道,
    // 生产者不会卡死在 send 上。
    let (tx, rx) = mpsc::sync_channel::<FileResult>(rayon::current_num_threads().max(1) * 2);
    let options_ref = &options;
    let cancel_ref = &cancel;
    let existing_ref = &existing;
    let source_root_ref = source_root.as_deref();
    // 生产者线程跑 rayon 并行解析,消费(写库)留在调用线程:
    // Receiver/回调/连接都不需要跨线程。
    std::thread::scope(|scope| {
        scope.spawn(move || {
            files.par_iter().for_each(|path| {
                if cancel_ref.load(Ordering::Relaxed) {
                    return;
                }
                let result = process_file(path, options_ref, existing_ref, source_root_ref);
                if tx.send(result).is_err() {
                    // 消费者已退出(写库出错),直接收工。
                    cancel_ref.store(true, Ordering::Relaxed);
                }
            });
        });

        // 调用线程做消费者:写库串行。收到一条就开事务;通道里紧随其后
        // 已到达的条目并入同一事务,通道空或攒满一批即提交,不为凑批次等待。
        while let Ok(first) = rx.recv() {
            if first_error.is_some() {
                continue;
            }
            let mut batch = Vec::with_capacity(WRITE_BATCH_MAX);
            batch.push(first);
            while batch.len() < WRITE_BATCH_MAX {
                match rx.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(_) => break,
                }
            }
            match write_batch(&mut conn, run_id, batch, counts) {
                Ok((new_counts, events)) => {
                    counts = new_counts;
                    for (path, outcome, event_counts) in events {
                        on_event(ImportEvent::FileDone {
                            path,
                            outcome,
                            counts: event_counts,
                        });
                    }
                }
                Err(error) => {
                    // 记首个写库错误,置 cancel 让 worker 收工,排空通道后统一返回。
                    first_error = Some(error);
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    });

    let cancelled = cancel.load(Ordering::Relaxed) && first_error.is_none();
    let now = repo::now_ms();
    let (status, message) = if let Some(error) = &first_error {
        ("failed", Some(format!("{error:#}")))
    } else if cancelled {
        ("cancelled", None)
    } else {
        ("done", None)
    };
    if let Err(error) = repo::finish_run(&conn, run_id, status, &counts, message.as_deref(), now) {
        log::warn!("写入导入结束状态失败: {error:#}");
    }
    on_event(ImportEvent::Finished {
        run_id,
        counts,
        cancelled,
    });
    match first_error {
        Some(error) => Err(error),
        None => Ok(counts),
    }
}

fn classify_inputs(inputs: &[PathBuf]) -> (&'static str, Option<PathBuf>) {
    if inputs.len() == 1 && inputs[0].is_dir() {
        ("folder", Some(inputs[0].clone()))
    } else {
        ("files", None)
    }
}

fn seed_items(
    conn: &mut rusqlite::Connection,
    run_id: i64,
    files: &[PathBuf],
    now: i64,
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    for path in files {
        repo::upsert_item(&tx, run_id, path, "pending", None, None, None, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// 一次读出 `canonical_path → 已有记录` 的映射(worker 的跳过判定用)。
fn load_existing(conn: &rusqlite::Connection) -> anyhow::Result<HashMap<String, Existing>> {
    let mut stmt = conn.prepare(
        "SELECT id, canonical_path, file_size, file_mtime_ms, content_hash, parse_status, source_root \
         FROM documents",
    )?;
    let map = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                Existing {
                    id: row.get(0)?,
                    file_size: row.get(2)?,
                    file_mtime_ms: row.get(3)?,
                    content_hash: row.get(4)?,
                    parsed: row.get::<_, String>(5)? == "parsed",
                    source_root: row.get(6)?,
                },
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(map)
}

/// worker:读文件 → 算 hash → 解析 → 文本化 → 分块。
/// 单文件解析不可中断(anydoc 无取消接口),只在文件之间检查 cancel。
fn process_file(
    path: &Path,
    options: &ImportOptions,
    existing: &HashMap<String, Existing>,
    source_root: Option<&str>,
) -> FileResult {
    // scan_paths 已统一为规范化路径。重试沿用原归属,显式目录输入仍优先。
    let source_root = source_root.or_else(|| {
        options
            .preserve_source_root
            .then(|| {
                existing
                    .get(path.to_string_lossy().as_ref())
                    .and_then(|prior| prior.source_root.as_deref())
            })
            .flatten()
    });
    let meta = match FileMeta::of(path) {
        Ok(Some(mut meta)) => {
            // 注入来源文件夹(FileMeta::of 只读文件本身,不知道这次是谁导入的)。
            meta.source_root = source_root.map(str::to_owned);
            meta
        }
        Ok(None) => {
            // 扫描与处理之间文件被改名/替换导致扩展名不再受支持。
            return FileResult::Done {
                meta: dummy_meta(path, source_root),
                hash: String::new(),
                result: Err(ParseError {
                    code: parse::ParseErrorCode::Unsupported,
                    message: "无法识别的格式,或该格式无法转换(例如纯图片 PDF)".to_owned(),
                    detail: format!("扩展名不支持: {}", path.display()),
                }),
            };
        }
        Err(error) => {
            return FileResult::Done {
                meta: dummy_meta(path, source_root),
                hash: String::new(),
                result: Err(ParseError {
                    code: parse::ParseErrorCode::Io,
                    message: "无法读取文件(权限/占用/路径失效)".to_owned(),
                    detail: format!("{error:#}"),
                }),
            };
        }
    };

    let key = meta.canonical_path.to_string_lossy().into_owned();
    let prior = existing.get(&key);
    // 一级跳过:size 与 mtime 都未变,不读文件。
    if !options.force
        && let Some(prior) = prior
        && (prior.parsed || options.skip_unchanged_failed)
        && prior.file_size == meta.file_size as i64
        && prior.file_mtime_ms == meta.file_mtime_ms
    {
        return FileResult::Skipped {
            meta,
            document_id: prior.id,
            touch: false,
        };
    }

    // 先读后算哈希:内容未变的文件在二级跳过就返回,不白白解析一遍。
    let bytes = match parse::read_file(path, options.max_file_bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            return FileResult::Done {
                meta,
                hash: String::new(),
                result: Err(error),
            };
        }
    };
    let hash = format!("{:x}", sha2::Sha256::digest(&bytes));

    // 二级跳过:元数据变了但内容哈希相同 → 只更新 size/mtime。
    if !options.force
        && let Some(prior) = prior
        && (prior.parsed || options.skip_unchanged_failed)
        && prior.content_hash == hash
    {
        return FileResult::Skipped {
            meta,
            document_id: prior.id,
            touch: true,
        };
    }

    let result = parse::parse_bytes(path, &bytes).and_then(|parsed| build_document(&meta, parsed));
    FileResult::Done { meta, hash, result }
}

/// 元数据读取失败时的占位 FileMeta(只为把错误带回写库线程记 item)。
fn dummy_meta(path: &Path, source_root: Option<&str>) -> FileMeta {
    FileMeta {
        path: path.to_path_buf(),
        canonical_path: path.to_path_buf(),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        ext: String::new(),
        file_type: parse::FileType::Text,
        file_size: 0,
        file_mtime_ms: 0,
        source_root: source_root.map(str::to_owned),
    }
}

/// 解析产物 → ParsedDocument(markdown → plain → 标题 → 分块)。
fn build_document(meta: &FileMeta, parsed: parse::Parsed) -> Result<ParsedDocument, ParseError> {
    let plain = text::markdown_to_plain(&parsed.markdown);
    let title = text::extract_title(&plain, &meta.stem());
    let chunks = chunk::chunk_document(&title, &plain);
    Ok(ParsedDocument {
        title,
        markdown: parsed.markdown,
        plain,
        chunks,
        warnings: parsed.warnings,
        parser_name: parsed.parser_name,
        parser_version: parsed.parser_version,
    })
}

/// 一次事务最多并入的 worker 结果条数(上限防大事务;不为凑批次等待)。
const WRITE_BATCH_MAX: usize = 64;

/// 一批结果消费后待回放的 FileDone 事件(path、outcome、消费该条时的计数快照)。
type BatchEvents = Vec<(PathBuf, FileOutcome, ImportCounts)>;

/// 一批 worker 结果在同一个 BEGIN IMMEDIATE 事务里落库。
///
/// commit 成功才返回 (新计数, 逐条 FileDone 事件缓存);任何一步出错事务随
/// drop 回滚,计数与事件都不发布。`on_event` 由调用方在提交后逐条回放,
/// 不在持锁期间调用。
fn write_batch(
    conn: &mut rusqlite::Connection,
    run_id: i64,
    batch: Vec<FileResult>,
    counts: ImportCounts,
) -> anyhow::Result<(ImportCounts, BatchEvents)> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .context("开启写入事务失败")?;
    let mut counts = counts;
    let mut events = Vec::with_capacity(batch.len());
    for msg in batch {
        let (path, outcome) = consume(&tx, run_id, msg, &mut counts)?;
        events.push((path, outcome, counts));
    }
    tx.commit().context("提交写入事务失败")?;
    Ok((counts, events))
}

/// 写库线程:把一条 worker 结果落库,转成对外事件。
/// `conn` 是调用方已开事务的连接(`Transaction` Deref 到 `Connection`)。
fn consume(
    conn: &rusqlite::Connection,
    run_id: i64,
    msg: FileResult,
    counts: &mut ImportCounts,
) -> anyhow::Result<(PathBuf, FileOutcome)> {
    let now = repo::now_ms();
    let (path, outcome) = match msg {
        FileResult::Skipped {
            meta,
            document_id,
            touch,
        } => {
            if touch {
                repo::touch_unchanged(conn, document_id, meta.file_size, meta.file_mtime_ms)?;
            }
            repo::upsert_item(
                conn,
                run_id,
                &meta.path,
                "skipped",
                None,
                None,
                Some(document_id),
                now,
            )?;
            counts.skipped += 1;
            (meta.path.clone(), FileOutcome::Skipped)
        }
        FileResult::Done { meta, hash, result } => match result {
            Ok(parsed) => {
                let id = repo::save_parsed_in(conn, &meta, &hash, &parsed, now)?;
                repo::upsert_item(conn, run_id, &meta.path, "ok", None, None, Some(id), now)?;
                counts.ok += 1;
                (meta.path.clone(), FileOutcome::Ok { document_id: id })
            }
            Err(error) => {
                let id = repo::save_failed_in(conn, &meta, &hash, &error, now)?;
                repo::upsert_item(
                    conn,
                    run_id,
                    &meta.path,
                    "failed",
                    Some(error.code.as_str()),
                    Some(&error.to_string()),
                    Some(id),
                    now,
                )?;
                counts.failed += 1;
                (
                    meta.path.clone(),
                    FileOutcome::Failed {
                        code: error.code.as_str().to_owned(),
                        message: error.to_string(),
                    },
                )
            }
        },
    };
    counts.processed += 1;
    Ok((path, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn file_meta(path: &str) -> FileMeta {
        FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: path.rsplit('.').next().unwrap_or("").to_owned(),
            file_type: crate::parse::FileType::Text,
            file_size: 0,
            file_mtime_ms: 1_000,
            source_root: None,
        }
    }

    fn done_result(path: &str, hash: &str) -> FileResult {
        let meta = file_meta(path);
        let parsed = build_document(
            &meta,
            parse::Parsed {
                markdown: "正文".to_owned(),
                warnings: Vec::new(),
                parser_name: "text",
                parser_version: "text.v1",
            },
        )
        .unwrap();
        FileResult::Done {
            meta,
            hash: hash.to_owned(),
            result: Ok(parsed),
        }
    }

    fn row_count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn write_batch_rolls_back_whole_batch_on_error() {
        let mut conn = store::open_in_memory().unwrap();
        // 特定路径的 item 写入触发失败,模拟批量写库中途出错。
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_item BEFORE INSERT ON import_items \
             WHEN new.path LIKE '%坏%' \
             BEGIN SELECT RAISE(ABORT, '注入的写库失败'); END;",
        )
        .unwrap();
        let run_id = repo::create_run(&conn, "files", None, 3, 1_000).unwrap();
        let counts = ImportCounts {
            total: 3,
            ..ImportCounts::default()
        };

        let batch = vec![
            done_result("/d/甲.txt", "h1"),
            done_result("/d/坏.txt", "h2"),
            done_result("/d/丙.txt", "h3"),
        ];
        let result = write_batch(&mut conn, run_id, batch, counts);
        assert!(result.is_err(), "触发器应让整批失败");
        // 整批回滚:触发器之前已写成功的 甲.txt 也不能留。
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 0);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents_fts"), 0);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM import_items"), 0);
        assert!(conn.is_autocommit(), "失败事务应已随 drop 回滚");

        // 对照:不含坏文件的批次正常提交,事件带逐条计数快照。
        let batch = vec![
            done_result("/d/甲.txt", "h1"),
            done_result("/d/丙.txt", "h3"),
        ];
        let (counts, events) = write_batch(&mut conn, run_id, batch, counts).unwrap();
        assert_eq!(counts.ok, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].2.processed, 1);
        assert_eq!(events[1].2.processed, 2);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 2);
    }
}
