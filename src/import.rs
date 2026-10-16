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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

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
    /// 显式 OCR 批次:只处理库中 NEEDS_OCR 的失败 PDF,普通导入为 None。
    pub ocr_base_url: Option<String>,
    /// true = 忽略 hash/mtime 跳过,全部重解析
    pub force: bool,
    /// 重新扫描时也跳过未变化的失败文件;force 优先。
    pub skip_unchanged_failed: bool,
    /// 单文件重试/更新时保留已有文件夹归属,不把它改成单独文件。
    pub preserve_source_root: bool,
    /// 增量更新时核对本次输入范围内的原文件，标记明确不存在的文档。
    pub detect_missing: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 100 * 1024 * 1024,
            ocr_base_url: None,
            force: false,
            skip_unchanged_failed: false,
            preserve_source_root: false,
            detect_missing: false,
        }
    }
}

/// 导入进度事件(写库线程逐条发给调用方)。
pub enum ImportEvent {
    /// 批次无法启动或写库失败。
    Error { message: String },
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
    needs_ocr: bool,
    file_missing: bool,
    ext: String,
}

/// 文本扩展名共用解析器;其他格式保守地按扩展名隔离,避免兜底路由不同。
fn parse_route(ext: &str) -> &str {
    match ext {
        "txt" | "md" => "text",
        _ => ext,
    }
}

type ReuseEntry = Arc<Mutex<Option<Reusable>>>;

enum Reusable {
    /// 只保留尚未提交的解析产物,写库成功后释放大文本。
    Parsed(Arc<ParsedDocument>),
    Stored(i64),
}

/// 每个内容键单独加锁,避免同批并发重复解析;不同内容仍可并行。
/// 只读连接短事务按需加载正文,不预载整个资料库、不长期持有 WAL 快照。
struct ReuseCache {
    entries: Mutex<HashMap<(String, String), ReuseEntry>>,
    reader: Mutex<rusqlite::Connection>,
    require_markdown: bool,
}

impl ReuseCache {
    fn new(
        db_path: &Path,
        existing: &HashMap<String, Existing>,
        require_markdown: bool,
    ) -> anyhow::Result<Self> {
        let mut entries: HashMap<_, ReuseEntry> = HashMap::new();
        for prior in existing
            .values()
            .filter(|p| p.parsed && p.content_hash.len() == 64)
        {
            let key = (
                prior.content_hash.clone(),
                parse_route(&prior.ext).to_owned(),
            );
            let entry = entries
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)));
            let mut state = entry.lock().unwrap();
            // 优先较新的副本,让升级前缺少块信息的旧记录逐步退出复用候选。
            if !matches!(*state, Some(Reusable::Stored(id)) if id > prior.id) {
                *state = Some(Reusable::Stored(prior.id));
            }
        }
        Ok(Self {
            entries: Mutex::new(entries),
            reader: Mutex::new(crate::store::open(
                db_path,
                crate::store::OpenMode::ReadOnly,
            )?),
            require_markdown,
        })
    }

    fn resolve(
        &self,
        meta: &FileMeta,
        hash: &str,
        parse: impl FnOnce() -> Result<ParsedDocument, ParseError>,
    ) -> (Result<Arc<ParsedDocument>, ParseError>, ReuseEntry) {
        let route = parse_route(&meta.ext);
        let entry = self
            .entries
            .lock()
            .unwrap()
            .entry((hash.to_owned(), route.to_owned()))
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut state = entry.lock().unwrap();
        let cached = match &*state {
            Some(Reusable::Parsed(parsed)) => Some(Arc::clone(parsed)),
            Some(Reusable::Stored(id)) => {
                let load = || -> anyhow::Result<Option<ParsedDocument>> {
                    let mut reader = self.reader.lock().unwrap();
                    let tx = reader.transaction()?;
                    // 源位置可能也在本批次被更新,必须再次核对哈希与解析状态。
                    let parsed =
                        repo::load_reusable_document(&tx, *id, hash, route, self.require_markdown)?;
                    tx.commit()?;
                    Ok(parsed)
                };
                match load() {
                    Ok(parsed) => parsed.map(Arc::new),
                    Err(error) => {
                        log::warn!("读取重复文件解析缓存失败,回退正常解析: {error:#}");
                        None
                    }
                }
            }
            None => None,
        };
        let result = if let Some(parsed) = cached {
            Ok(parsed)
        } else {
            // 不缓存失败;失败副本不能阻止后续文件正常重试。
            let result = parse().map(Arc::new);
            *state = result
                .as_ref()
                .ok()
                .map(|p| Reusable::Parsed(Arc::clone(p)));
            result
        };
        drop(state);
        let result = result.map(|parsed| {
            let title = text::extract_title(&parsed.plain, &meta.stem());
            if title == parsed.title {
                parsed
            } else {
                // 无正文标题时使用各自文件名,并重新生成对应的分块标题路径。
                let mut own = (*parsed).clone();
                own.title = title;
                own.chunks = chunk::chunk_document(&own.title, &own.plain);
                Arc::new(own)
            }
        });
        (result, entry)
    }
}

/// worker 产出的单文件结果(内部消息;公开事件是 FileOutcome)。
enum FileResult {
    Missing {
        path: PathBuf,
        document_id: i64,
    },
    /// 按 mtime/size 或 hash 判定未变更;touch = 需要回写新的 size/mtime
    Skipped {
        meta: FileMeta,
        document_id: i64,
        touch: bool,
    },
    Done {
        meta: FileMeta,
        hash: String,
        result: Result<Arc<ParsedDocument>, ParseError>,
        reuse: Option<ReuseEntry>,
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

    let existing = load_existing(&conn)?;
    let ocr = options
        .ocr_base_url
        .as_deref()
        .map(crate::ocr::Client::new)
        .transpose()?;
    let mut options = options;
    let mut files = if ocr.is_some() {
        options.force = true;
        options.preserve_source_root = true;
        // 以库中当前状态为准,防止 GUI 缓存过期后重写已成功的文档。
        let requested: HashSet<_> = inputs
            .iter()
            .map(|p| dunce::canonicalize(p).unwrap_or_else(|_| absolute(p)))
            .collect();
        let mut files: Vec<_> = existing
            .iter()
            .filter(|(path, prior)| prior.needs_ocr && requested.contains(&PathBuf::from(path)))
            .map(|(path, _)| PathBuf::from(path))
            .collect();
        files.sort();
        files
    } else {
        scan_paths(&inputs)
    };
    let missing = if options.detect_missing && ocr.is_none() {
        collect_missing(&inputs, &existing, &cancel)?
    } else {
        HashMap::new()
    };
    files.extend(missing.keys().cloned());
    files.sort();
    files.dedup();
    // force(含显式 OCR)保留逐文件重新解析语义,不复用任何旧结果。
    let reuse = if options.force {
        None
    } else {
        Some(ReuseCache::new(
            db_path,
            &existing,
            repo::save_markdown_enabled(&conn),
        )?)
    };
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
    let reuse_ref = reuse.as_ref();
    // 生产者线程跑 rayon 并行解析,消费(写库)留在调用线程:
    // Receiver/回调/连接都不需要跨线程。
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let process = |path: &PathBuf| {
                if cancel_ref.load(Ordering::Relaxed) {
                    return;
                }
                let result = if let Some(&document_id) = missing.get(path) {
                    FileResult::Missing {
                        path: path.clone(),
                        document_id,
                    }
                } else {
                    process_file(
                        path,
                        options_ref,
                        existing_ref,
                        source_root_ref,
                        ocr.as_ref(),
                        cancel_ref,
                        reuse_ref,
                    )
                };
                // OCR 可在轮询间取消,取消不覆盖原失败原因。
                if ocr.is_some() && cancel_ref.load(Ordering::Relaxed) {
                    return;
                }
                if tx.send(result).is_err() {
                    cancel_ref.store(true, Ordering::Relaxed);
                }
            };
            if ocr.is_some() {
                // 服务端自行并行处理页面;客户端逐文档提交,避免挤爆队列。
                files.iter().for_each(process);
            } else {
                files.par_iter().for_each(process);
            }
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

/// 不用扫描差集判断删除:被跳过的目录、权限错误都不能作为删除证据。
fn collect_missing(
    inputs: &[PathBuf],
    existing: &HashMap<String, Existing>,
    cancel: &AtomicBool,
) -> anyhow::Result<HashMap<PathBuf, i64>> {
    let mut scopes = Vec::new();
    for input in inputs {
        let normalized = dunce::canonicalize(input).unwrap_or_else(|_| absolute(input));
        let known_root = existing.values().any(|prior| {
            prior.source_root.as_deref() == Some(normalized.to_string_lossy().as_ref())
        });
        let is_dir = match std::fs::metadata(&normalized) {
            Ok(meta) => meta.is_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !known_root => false,
            Err(error) => {
                return Err(error).with_context(|| format!("更新路径不可用: {}", input.display()));
            }
        };
        if known_root && !is_dir {
            anyhow::bail!("来源文件夹不可用: {}", input.display());
        }
        scopes.push((normalized, is_dir));
    }
    let mut missing = HashMap::new();
    for (path, prior) in existing {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let path = Path::new(path);
        if scopes
            .iter()
            .any(|(input, is_dir)| path == input || (*is_dir && path.starts_with(input)))
            && repo::original_file_missing(path, prior.source_root.as_deref())?
        {
            missing.insert(path.to_path_buf(), prior.id);
        }
    }
    Ok(missing)
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
        "SELECT id, canonical_path, file_size, file_mtime_ms, content_hash, parse_status, source_root, parse_error_code, file_type, ext \
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
                    needs_ocr: row.get::<_, String>(5)? == "failed"
                        && row.get::<_, Option<String>>(7)?.as_deref() == Some("NEEDS_OCR")
                        && row.get::<_, String>(8)? == "pdf",
                    file_missing: row.get::<_, Option<String>>(7)?.as_deref()
                        == Some("FILE_MISSING"),
                    ext: row.get(9)?,
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
    ocr: Option<&crate::ocr::Client>,
    cancel: &AtomicBool,
    reuse: Option<&ReuseCache>,
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
                reuse: None,
                result: Err(ParseError {
                    code: parse::ParseErrorCode::Unsupported,
                    message: "无法识别的格式,或该格式无法转换(例如纯图片 PDF)".to_owned(),
                    detail: format!("扩展名不支持: {}", path.display()),
                }),
            };
        }
        Err(error) => {
            if options.detect_missing
                && let Some(prior) = existing.get(path.to_string_lossy().as_ref())
                && matches!(
                    repo::original_file_missing(path, prior.source_root.as_deref()),
                    Ok(true)
                )
            {
                return FileResult::Missing {
                    path: path.to_path_buf(),
                    document_id: prior.id,
                };
            }
            return FileResult::Done {
                meta: dummy_meta(path, source_root),
                hash: String::new(),
                reuse: None,
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
        && !prior.file_missing
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
            if options.detect_missing
                && let Some(prior) = prior
                && matches!(
                    repo::original_file_missing(path, prior.source_root.as_deref()),
                    Ok(true)
                )
            {
                return FileResult::Missing {
                    path: path.to_path_buf(),
                    document_id: prior.id,
                };
            }
            return FileResult::Done {
                meta,
                hash: String::new(),
                reuse: None,
                result: Err(error),
            };
        }
    };
    let hash = format!("{:x}", sha2::Sha256::digest(&bytes));

    // 二级跳过:元数据变了但内容哈希相同 → 只更新 size/mtime。
    if !options.force
        && let Some(prior) = prior
        && !prior.file_missing
        && (prior.parsed || options.skip_unchanged_failed)
        && prior.content_hash == hash
    {
        return FileResult::Skipped {
            meta,
            document_id: prior.id,
            touch: true,
        };
    }

    let parse = || {
        match ocr {
            Some(client) => client.parse_pdf(bytes, cancel),
            None => parse::parse_bytes(path, &bytes),
        }
        .and_then(|parsed| build_document(&meta, parsed))
    };
    let (result, reuse) = match reuse {
        Some(cache) => {
            let (result, entry) = cache.resolve(&meta, &hash, parse);
            (result, Some(entry))
        }
        None => (parse().map(Arc::new), None),
    };
    FileResult::Done {
        meta,
        hash,
        result,
        reuse,
    }
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
        parser_name: parsed.parser_name.to_owned(),
        parser_version: parsed.parser_version.to_owned(),
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
    let mut published = Vec::new();
    for msg in batch {
        let reuse = match &msg {
            FileResult::Done {
                reuse,
                result: Ok(_),
                ..
            } => reuse.clone(),
            _ => None,
        };
        let (path, outcome) = consume(&tx, run_id, msg, &mut counts)?;
        if let (Some(entry), FileOutcome::Ok { document_id }) = (reuse, &outcome) {
            published.push((entry, *document_id));
        }
        events.push((path, outcome, counts));
    }
    tx.commit().context("提交写入事务失败")?;
    // 提交后才能把内存产物替换为 ID,否则读连接会看不到尚未提交的正文。
    for (entry, id) in published {
        *entry.lock().unwrap() = Some(Reusable::Stored(id));
    }
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
        FileResult::Missing { path, document_id } => {
            let marked = repo::mark_file_missing_in(conn, document_id, now)?;
            let (status, code, message, outcome) = if marked {
                counts.failed += 1;
                let code = parse::ParseErrorCode::FileMissing.as_str();
                let message = "原文件不存在（可能已删除或移动）";
                (
                    "failed",
                    Some(code),
                    Some(message),
                    FileOutcome::Failed {
                        code: code.to_owned(),
                        message: message.to_owned(),
                    },
                )
            } else {
                counts.skipped += 1;
                ("skipped", None, None, FileOutcome::Skipped)
            };
            repo::upsert_item(
                conn,
                run_id,
                &path,
                status,
                code,
                message,
                Some(document_id),
                now,
            )?;
            (path, outcome)
        }
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
        FileResult::Done {
            meta, hash, result, ..
        } => match result {
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
            result: Ok(Arc::new(parsed)),
            reuse: None,
        }
    }

    fn row_count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn test_cache() -> (PathBuf, rusqlite::Connection, ReuseCache) {
        let dir = std::env::temp_dir().join(format!(
            "rsou-reuse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("index.sqlite3");
        let conn = store::open(&path, store::OpenMode::ReadWrite).unwrap();
        let cache = ReuseCache::new(&path, &load_existing(&conn).unwrap(), false).unwrap();
        (path, conn, cache)
    }

    fn parsed_text(meta: &FileMeta, markdown: &str) -> ParsedDocument {
        build_document(
            meta,
            parse::Parsed {
                markdown: markdown.to_owned(),
                warnings: Vec::new(),
                parser_name: "text",
                parser_version: "text.v1",
            },
        )
        .unwrap()
    }

    #[test]
    fn concurrent_duplicates_parse_once_but_different_routes_stay_separate() {
        let (_path, _conn, cache) = test_cache();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        pool.install(|| {
            (0..32).into_par_iter().for_each(|i| {
                let meta = file_meta(&format!("/d/{i}.{}", if i % 2 == 0 { "txt" } else { "md" }));
                let (parsed, _) = cache.resolve(&meta, "same-hash", || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    Ok(parsed_text(&meta, "# 正文标题\n\n正文"))
                });
                assert_eq!(parsed.unwrap().title, "正文标题");
            });
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let meta = file_meta("/d/a.csv");
        cache
            .resolve(&meta, "same-hash", || {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(parsed_text(&meta, "不同解析路由"))
            })
            .0
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn committed_duplicates_release_memory_and_reuse_persisted_blocks() {
        let (path, mut conn, cache) = test_cache();
        let hash = "a".repeat(64);
        let source = file_meta("/d/原件.txt");
        let (parsed, entry) = cache.resolve(&source, &hash, || {
            Ok(parsed_text(&source, "### 原件\n\n正文"))
        });
        let parsed = parsed.unwrap();
        let weak = Arc::downgrade(&parsed);
        let run_id = repo::create_run(&conn, "files", None, 1, 1_000).unwrap();
        write_batch(
            &mut conn,
            run_id,
            vec![FileResult::Done {
                meta: source,
                hash: hash.clone(),
                result: Ok(parsed),
                reuse: Some(entry.clone()),
            }],
            ImportCounts::default(),
        )
        .unwrap();
        assert!(matches!(*entry.lock().unwrap(), Some(Reusable::Stored(_))));
        assert!(weak.upgrade().is_none(), "提交后不能一直缓存大文本");

        let copy = file_meta("/d/副本.md");
        // 覆盖同批提交后复用和重新启动导入后的跨批复用。
        let reopened = ReuseCache::new(&path, &load_existing(&conn).unwrap(), false).unwrap();
        for cache in [&cache, &reopened] {
            let (parsed, _) = cache.resolve(&copy, &hash, || panic!("相同内容不应再解析"));
            let parsed = parsed.unwrap();
            assert_eq!(parsed.title, "副本");
            assert_eq!(parsed.plain.text, "原件\n\n正文");
            assert_eq!(parsed.chunks[0].context_header, "副本 › 原件");
            assert_eq!(parsed.parser_name, "text");
        }
        let stored_id = match *entry.lock().unwrap() {
            Some(Reusable::Stored(id)) => id,
            _ => unreachable!(),
        };
        conn.execute(
            "UPDATE documents SET content_hash = 'changed' WHERE id = ?1",
            [stored_id],
        )
        .unwrap();
        let (parsed, _) = cache.resolve(&copy, &hash, || Ok(parsed_text(&copy, "回退重新解析")));
        assert_eq!(
            parsed.unwrap().plain.text,
            "回退重新解析",
            "源记录被更新后不能复用旧哈希"
        );
    }

    #[test]
    fn failures_are_not_cached_and_rollback_does_not_publish_ids() {
        let (_path, mut conn, cache) = test_cache();
        let meta = file_meta("/d/甲.txt");
        let (result, _) = cache.resolve(&meta, "hash", || {
            Err(ParseError {
                code: parse::ParseErrorCode::Empty,
                message: "空文件".into(),
                detail: String::new(),
            })
        });
        assert!(result.is_err());
        let (result, entry) = cache.resolve(&meta, "hash", || Ok(parsed_text(&meta, "正常重试")));
        assert!(result.is_ok());
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_item BEFORE INSERT ON import_items \
             BEGIN SELECT RAISE(ABORT, '注入写库失败'); END;",
        )
        .unwrap();
        let run_id = repo::create_run(&conn, "files", None, 1, 1_000).unwrap();
        assert!(
            write_batch(
                &mut conn,
                run_id,
                vec![FileResult::Done {
                    meta,
                    hash: "hash".into(),
                    result,
                    reuse: Some(entry.clone()),
                }],
                ImportCounts::default()
            )
            .is_err()
        );
        assert!(matches!(*entry.lock().unwrap(), Some(Reusable::Parsed(_))));
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 0);
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
