//! 导入流水线:扫描 → 并行解析 → 串行写库(无 GUI 依赖)。
//!
//! 线程模型:
//! - `run_import` 在调用线程执行(GUI 把它放进 `std::thread`),自己开写连接;
//! - 文件读取、解析、文本化、分块、哈希在 rayon 线程池并行;
//! - 所有写库操作串行发生在调用线程,经有界 mpsc 通道消费 worker 结果
//!   (写库慢时通道给解析线程背压)。
//!
//! 取消:扫描、初始化及文件处理的阶段边界检查 cancel,丢弃待写入结果,
//! 未提交事务回滚。已提交数据保留。anydoc/单次 I/O 不可中断,须等返回后
//! 才能退出;缓存锁仍使用阻塞等待,拿锁后再次检查,不忙轮询。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use rayon::prelude::*;
use sha2::Digest;

use crate::chunk;
use crate::parse::{self, ParseError};
use crate::repo::{self, FileMeta, ParsedDocument};
use crate::text;

pub use crate::repo::ImportCounts;

/// 内部控制流,不作为解析失败入库,也不覆盖真正的写库错误。
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("导入已取消")
    }
}

impl std::error::Error for Cancelled {}

fn check_cancel(cancel: &AtomicBool) -> Result<(), Cancelled> {
    if cancel.load(Ordering::Relaxed) {
        Err(Cancelled)
    } else {
        Ok(())
    }
}

/// 导入参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportOptions {
    /// 只导入这些扩展名(不含点,忽略大小写)。None 沿用文件夹设置,
    /// 没有设置时导入全部支持格式;显式空集合只允许用于文件夹设置。
    pub extensions: Option<BTreeSet<String>>,
    /// 保存单个来源文件夹的格式设置并移除未选格式的索引(可清空选择)。
    /// 输入须为库中规范化的来源路径,允许原文件夹离线。
    /// 新增格式时自动按增量更新处理;仅减少格式时不扫描原文件。
    /// 设置和移除操作原子提交,后续增量更新被取消时保留已提交的设置。
    #[serde(default)]
    pub sync_folder_extensions: bool,
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
            extensions: None,
            sync_folder_extensions: false,
            max_file_bytes: 100 * 1024 * 1024,
            ocr_base_url: None,
            force: false,
            skip_unchanged_failed: false,
            preserve_source_root: false,
            detect_missing: false,
        }
    }
}

/// 导入进度事件(扫描阶段及写库提交后发给调用方)。
#[derive(serde::Serialize, serde::Deserialize)]
pub enum ImportEvent {
    /// 批次无法启动或写库失败。
    Error { message: String },
    /// 正在扫描:已发现的待索引文件数(按格式过滤、路径去重,总数尚未确定)。
    Scanning { found: usize },
    /// 扫描完成,确定了文件总数
    Scanned { total: usize },
    /// 一个文件处理完毕
    FileDone {
        #[serde(with = "crate::ipc_path")]
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
#[derive(serde::Serialize, serde::Deserialize)]
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
    scan_paths_filtered(inputs, None, || Ok(()), |_| {}).expect("未启用取消")
}

/// 中间扫描进度至多每 200ms 发送一次;开始和结束立即回报。
const SCAN_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

fn extension_selected(path: &Path, extensions: Option<&BTreeSet<String>>) -> bool {
    extensions.is_none_or(|selected| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| selected.iter().any(|s| s.eq_ignore_ascii_case(ext)))
    })
}

fn scan_paths_filtered(
    inputs: &[PathBuf],
    extensions: Option<&BTreeSet<String>>,
    mut check: impl FnMut() -> Result<(), Cancelled>,
    mut on_found: impl FnMut(usize),
) -> Result<Vec<PathBuf>, Cancelled> {
    const SKIP_DIRS: &[&str] = &["node_modules", "$RECYCLE.BIN", "System Volume Information"];
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    check()?;
    on_found(0);
    for input in inputs {
        check()?;
        stack.push(input.clone());
    }

    while let Some(path) = stack.pop() {
        check()?;
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
                    check()?;
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
        if parse::file_type_of(&path).is_none() || !extension_selected(&path, extensions) {
            continue;
        }
        // 规范化用于去重与入库;失败时退回绝对路径。
        // 走 dunce 而不是 std:Windows 上 std 会返回 `\\?\C:\...` 形式,
        // 既不适合展示,也会让「目录前缀」过滤匹配不上(见 crate 注释)。
        let normalized = dunce::canonicalize(&path).unwrap_or_else(|_| absolute(&path));
        if seen.insert(normalized.clone()) {
            files.push(normalized);
            on_found(files.len());
        }
    }
    check()?;
    files.sort();
    check()?;
    Ok(files)
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
        cancel: &AtomicBool,
    ) -> anyhow::Result<Self> {
        check_cancel(cancel)?;
        let mut entries: HashMap<_, ReuseEntry> = HashMap::new();
        for prior in existing.values() {
            check_cancel(cancel)?;
            if !prior.parsed || prior.content_hash.len() != 64 {
                continue;
            }
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
        check_cancel(cancel)?;
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
        cancel: &AtomicBool,
        parse: impl FnOnce() -> Result<Result<ParsedDocument, ParseError>, Cancelled>,
    ) -> Result<(Result<Arc<ParsedDocument>, ParseError>, ReuseEntry), Cancelled> {
        check_cancel(cancel)?;
        let route = parse_route(&meta.ext);
        let entry = self
            .entries
            .lock()
            .unwrap()
            .entry((hash.to_owned(), route.to_owned()))
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        check_cancel(cancel)?;
        let mut state = entry.lock().unwrap();
        check_cancel(cancel)?;
        let cached = match &*state {
            Some(Reusable::Parsed(parsed)) => Some(Arc::clone(parsed)),
            Some(Reusable::Stored(id)) => {
                let load = || -> anyhow::Result<Option<ParsedDocument>> {
                    let mut reader = self.reader.lock().unwrap();
                    check_cancel(cancel)?;
                    let tx = reader.transaction()?;
                    // 源位置可能也在本批次被更新,必须再次核对哈希与解析状态。
                    let parsed =
                        repo::load_reusable_document(&tx, *id, hash, route, self.require_markdown)?;
                    tx.commit()?;
                    Ok(parsed)
                };
                match load() {
                    Ok(parsed) => parsed.map(Arc::new),
                    Err(error) if error.is::<Cancelled>() => return Err(Cancelled),
                    Err(error) => {
                        log::warn!("读取重复文件解析缓存失败,回退正常解析: {error:#}");
                        None
                    }
                }
            }
            None => None,
        };
        check_cancel(cancel)?;
        let result = if let Some(parsed) = cached {
            Ok(parsed)
        } else {
            // 不缓存失败;失败副本不能阻止后续文件正常重试。
            let result = parse()?.map(Arc::new);
            check_cancel(cancel)?;
            *state = result
                .as_ref()
                .ok()
                .map(|p| Reusable::Parsed(Arc::clone(p)));
            result
        };
        drop(state);
        check_cancel(cancel)?;
        let result = match result {
            Ok(parsed) => {
                let title = text::extract_title(&parsed.plain, &meta.stem());
                check_cancel(cancel)?;
                if title == parsed.title {
                    Ok(parsed)
                } else {
                    // 无正文标题时使用各自文件名,并重新生成对应的分块标题路径。
                    let mut own = (*parsed).clone();
                    check_cancel(cancel)?;
                    own.title = title;
                    own.chunks = chunk::chunk_document(&own.title, &own.plain);
                    Ok(Arc::new(own))
                }
            }
            Err(error) => Err(error),
        };
        check_cancel(cancel)?;
        Ok((result, entry))
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
    run_import_in_run(db_path, inputs, options, cancel, on_event, None)
}

/// 独立进程复用监控端预先创建的批次,进程被终止后仍可准确收口该批次。
pub(crate) fn run_import_in_run(
    db_path: &Path,
    inputs: Vec<PathBuf>,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
    on_event: &mut dyn FnMut(ImportEvent),
    mut run_id: Option<i64>,
) -> anyhow::Result<ImportCounts> {
    let mut conn = crate::store::open(db_path, crate::store::OpenMode::ReadWrite)
        .context("导入前打开索引库失败")?;

    // 文件夹设置的目标是已保存的来源路径,即使原文件夹离线也允许减少格式。
    let (kind, root) = if options.sync_folder_extensions && inputs.len() == 1 {
        ("folder", Some(inputs[0].clone()))
    } else {
        classify_inputs(&inputs)
    };
    let mut counts = ImportCounts::default();
    let mut folder_settings_saved = false;
    // 统一收口准备阶段和处理阶段的取消,包括尚未创建 run 的目录扫描。
    let result = (|| -> anyhow::Result<()> {
        check_cancel(&cancel)?;
        if let Some(id) = run_id {
            repo::set_run_source(&conn, id, kind, root.as_deref())?;
        }
        if options.sync_folder_extensions {
            anyhow::ensure!(inputs.len() == 1, "文件夹设置只能用于单个来源文件夹");
            anyhow::ensure!(
                options.ocr_base_url.is_none(),
                "文件夹设置不能与 OCR 同时执行"
            );
            anyhow::ensure!(options.extensions.is_some(), "请指定文件夹要索引的文件类型");
        }
        let ocr = options
            .ocr_base_url
            .as_deref()
            .map(crate::ocr::Client::new)
            .transpose()?;
        let mut options = options;
        // 只有单个目录输入有唯一的文件夹归属。设置与文档都用规范化路径作键。
        let source_root = if kind == "folder" {
            root.as_ref().map(|dir| {
                let path = if options.sync_folder_extensions {
                    absolute(dir)
                } else {
                    dunce::canonicalize(dir).unwrap_or_else(|_| absolute(dir))
                };
                path.to_string_lossy().into_owned()
            })
        } else {
            None
        };
        if ocr.is_none() {
            let explicit_extensions = options.extensions.is_some();
            if options.extensions.is_none()
                && let Some(root) = &source_root
            {
                options.extensions = repo::folder_import_extensions(&conn, root)?;
            }
            if let Some(extensions) = &mut options.extensions {
                *extensions = extensions
                    .iter()
                    .map(|ext| ext.to_ascii_lowercase())
                    .collect();
                anyhow::ensure!(
                    !extensions.is_empty()
                        || options.sync_folder_extensions
                        || !explicit_extensions,
                    "请至少选择一种要索引的文件类型"
                );
                for ext in extensions.iter() {
                    anyhow::ensure!(
                        parse::supported_extensions().contains(&ext.as_str()),
                        "不支持的索引文件类型: {ext}"
                    );
                }
            }
        }
        let settings_only = if options.sync_folder_extensions {
            let root = source_root.as_deref().expect("单个来源文件夹");
            let extensions = options.extensions.as_ref().expect("已指定文件类型");
            let previous = repo::folder_import_extensions(&conn, root)?.unwrap_or_else(|| {
                parse::supported_extensions()
                    .iter()
                    .map(|ext| (*ext).to_owned())
                    .collect()
            });
            let added = extensions
                .iter()
                .any(|ext| !previous.iter().any(|old| old.eq_ignore_ascii_case(ext)));
            check_cancel(&cancel)?;
            repo::sync_folder_import_extensions(&mut conn, root, extensions)?;
            folder_settings_saved = true;
            // 与「增量更新」一致,不重试未变化的失败文件、不强制重解析。
            options.force = false;
            options.skip_unchanged_failed = true;
            options.preserve_source_root = true;
            options.detect_missing = true;
            !added
        } else {
            false
        };
        let no_files = settings_only
            || (ocr.is_none() && options.extensions.as_ref().is_some_and(BTreeSet::is_empty));
        // 必须在格式清理之后加载,避免复用刚移除文档的跳过信息或正文缓存。
        let existing = if no_files {
            HashMap::new()
        } else {
            load_existing(&conn, &cancel)?
        };
        check_cancel(&cancel)?;
        let mut files = if no_files {
            Vec::new()
        } else if ocr.is_some() {
            options.force = true;
            options.preserve_source_root = true;
            // 以库中当前状态为准,防止 GUI 缓存过期后重写已成功的文档。
            let requested: HashSet<_> = inputs
                .iter()
                .take_while(|_| !cancel.load(Ordering::Relaxed))
                .map(|p| dunce::canonicalize(p).unwrap_or_else(|_| absolute(p)))
                .collect();
            let files: Vec<_> = existing
                .iter()
                .take_while(|_| !cancel.load(Ordering::Relaxed))
                .filter(|(path, prior)| prior.needs_ocr && requested.contains(&PathBuf::from(path)))
                .map(|(path, _)| PathBuf::from(path))
                .collect();
            files
        } else {
            let mut last_report: Option<(usize, Instant)> = None;
            let files = scan_paths_filtered(
                &inputs,
                options.extensions.as_ref(),
                || check_cancel(&cancel),
                |found| {
                    if last_report.is_none_or(|(_, at)| at.elapsed() >= SCAN_PROGRESS_INTERVAL) {
                        on_event(ImportEvent::Scanning { found });
                        last_report = Some((found, Instant::now()));
                    }
                },
            )?;
            // 小目录也要回报最终发现数,不能因节流停留在 0。
            if last_report.is_none_or(|(found, _)| found != files.len()) {
                on_event(ImportEvent::Scanning { found: files.len() });
            }
            files
        };
        check_cancel(&cancel)?;
        let missing = if !no_files && options.detect_missing && ocr.is_none() {
            collect_missing(&inputs, &existing, &cancel, options.extensions.as_ref())?
        } else {
            HashMap::new()
        };
        check_cancel(&cancel)?;
        files.extend(missing.keys().cloned());
        files.sort();
        check_cancel(&cancel)?;
        files.dedup();
        counts.total = files.len();
        check_cancel(&cancel)?;
        // force(含显式 OCR)保留逐文件重新解析语义,不复用任何旧结果。
        let reuse = if options.force || no_files {
            None
        } else {
            Some(ReuseCache::new(
                db_path,
                &existing,
                repo::save_markdown_enabled(&conn),
                &cancel,
            )?)
        };
        check_cancel(&cancel)?;
        on_event(ImportEvent::Scanned { total: files.len() });
        check_cancel(&cancel)?;

        let now = repo::now_ms();
        let id = match run_id {
            Some(id) => id,
            None => repo::create_run(&conn, kind, root.as_deref(), files.len(), now)?,
        };
        run_id = Some(id);
        check_cancel(&cancel)?;
        if ocr.is_none()
            && !options.sync_folder_extensions
            && let (Some(root), Some(extensions)) = (&source_root, &options.extensions)
        {
            repo::set_folder_import_extensions(&conn, root, extensions)?;
        }
        // 全部 item 先记 pending,跑完/失败/跳过时逐个改写。
        seed_items(&mut conn, id, &files, now, &cancel)?;

        // 有界通道:写库慢时给解析线程背压;取消/写库出错后仍排空但不写入,
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
                        let Ok(result) = process_file(
                            path,
                            options_ref,
                            existing_ref,
                            source_root_ref,
                            ocr.as_ref(),
                            cancel_ref,
                            reuse_ref,
                        ) else {
                            return;
                        };
                        result
                    };
                    // 普通导入和 OCR 都丢弃取消后的产物,不记作解析失败。
                    if cancel_ref.load(Ordering::Relaxed) {
                        return;
                    }
                    if tx.send(result).is_err() {
                        cancel_ref.store(true, Ordering::Relaxed);
                    }
                };
                if ocr.is_some() {
                    // 服务端自行并行处理页面;客户端逐文档提交,避免挤爆队列。
                    files
                        .iter()
                        .take_while(|_| !cancel_ref.load(Ordering::Relaxed))
                        .for_each(process);
                } else {
                    files
                        .par_iter()
                        .try_for_each(|path| {
                            check_cancel(cancel_ref)?;
                            process(path);
                            check_cancel(cancel_ref)
                        })
                        .ok();
                }
            });

            consume_results(&mut conn, id, rx, &mut counts, &cancel, on_event)
        })
    })();
    let first_error = result
        .err()
        .filter(|error| !error.is::<Cancelled>())
        .map(|error| {
            if folder_settings_saved {
                error.context("文件夹设置已保存,但后续增量更新失败;可稍后重试「增量更新」")
            } else {
                error
            }
        });
    // 无法启动的真正错误保持原语义:由调用方报告 Error,不伪装成取消。
    if run_id.is_none()
        && let Some(error) = first_error
    {
        return Err(error);
    }
    let run_id = match run_id {
        Some(id) => id,
        None => repo::create_run(&conn, kind, root.as_deref(), counts.total, repo::now_ms())?,
    };
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
    extensions: Option<&BTreeSet<String>>,
) -> anyhow::Result<HashMap<PathBuf, i64>> {
    let mut scopes = Vec::new();
    for input in inputs {
        check_cancel(cancel)?;
        let normalized = dunce::canonicalize(input).unwrap_or_else(|_| absolute(input));
        let known_root = existing
            .values()
            .take_while(|_| !cancel.load(Ordering::Relaxed))
            .any(|prior| {
                prior.source_root.as_deref() == Some(normalized.to_string_lossy().as_ref())
            });
        check_cancel(cancel)?;
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
        check_cancel(cancel)?;
        let path = Path::new(path);
        if !extension_selected(path, extensions) {
            continue;
        }
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
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    check_cancel(cancel)?;
    let tx = conn.transaction()?;
    for path in files {
        check_cancel(cancel)?;
        repo::upsert_item(&tx, run_id, path, "pending", None, None, None, now)?;
    }
    check_cancel(cancel)?;
    tx.commit()?;
    Ok(())
}

/// 一次读出 `canonical_path → 已有记录` 的映射(worker 的跳过判定用)。
fn load_existing(
    conn: &rusqlite::Connection,
    cancel: &AtomicBool,
) -> anyhow::Result<HashMap<String, Existing>> {
    check_cancel(cancel)?;
    let mut stmt = conn.prepare(
        "SELECT id, canonical_path, file_size, file_mtime_ms, content_hash, parse_status, source_root, parse_error_code, file_type, ext \
         FROM documents",
    )?;
    let rows = stmt.query_map([], |row| {
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
                file_missing: row.get::<_, Option<String>>(7)?.as_deref() == Some("FILE_MISSING"),
                ext: row.get(9)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        check_cancel(cancel)?;
        let (path, prior) = row?;
        map.insert(path, prior);
    }
    check_cancel(cancel)?;
    Ok(map)
}

/// worker:读文件 → 算 hash → 解析 → 文本化 → 分块。
/// 单次解析/I/O 不可中断,在阶段边界停止,不把取消写成文件失败。
fn process_file(
    path: &Path,
    options: &ImportOptions,
    existing: &HashMap<String, Existing>,
    source_root: Option<&str>,
    ocr: Option<&crate::ocr::Client>,
    cancel: &AtomicBool,
    reuse: Option<&ReuseCache>,
) -> Result<FileResult, Cancelled> {
    check_cancel(cancel)?;
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
            return Ok(FileResult::Done {
                meta: dummy_meta(path, source_root),
                hash: String::new(),
                reuse: None,
                result: Err(ParseError {
                    code: parse::ParseErrorCode::Unsupported,
                    message: "无法识别的格式,或该格式无法转换(例如纯图片 PDF)".to_owned(),
                    detail: format!("扩展名不支持: {}", path.display()),
                }),
            });
        }
        Err(error) => {
            check_cancel(cancel)?;
            if options.detect_missing
                && let Some(prior) = existing.get(path.to_string_lossy().as_ref())
                && matches!(
                    repo::original_file_missing(path, prior.source_root.as_deref()),
                    Ok(true)
                )
            {
                return Ok(FileResult::Missing {
                    path: path.to_path_buf(),
                    document_id: prior.id,
                });
            }
            return Ok(FileResult::Done {
                meta: dummy_meta(path, source_root),
                hash: String::new(),
                reuse: None,
                result: Err(ParseError {
                    code: parse::ParseErrorCode::Io,
                    message: "无法读取文件(权限/占用/路径失效)".to_owned(),
                    detail: format!("{error:#}"),
                }),
            });
        }
    };
    check_cancel(cancel)?;

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
        return Ok(FileResult::Skipped {
            meta,
            document_id: prior.id,
            touch: false,
        });
    }

    // 先读后算哈希:内容未变的文件在二级跳过就返回,不白白解析一遍。
    let bytes = match parse::read_file(path, options.max_file_bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            check_cancel(cancel)?;
            if options.detect_missing
                && let Some(prior) = prior
                && matches!(
                    repo::original_file_missing(path, prior.source_root.as_deref()),
                    Ok(true)
                )
            {
                return Ok(FileResult::Missing {
                    path: path.to_path_buf(),
                    document_id: prior.id,
                });
            }
            return Ok(FileResult::Done {
                meta,
                hash: String::new(),
                reuse: None,
                result: Err(error),
            });
        }
    };
    check_cancel(cancel)?;
    let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
    check_cancel(cancel)?;

    // 二级跳过:元数据变了但内容哈希相同 → 只更新 size/mtime。
    if !options.force
        && let Some(prior) = prior
        && !prior.file_missing
        && (prior.parsed || options.skip_unchanged_failed)
        && prior.content_hash == hash
    {
        return Ok(FileResult::Skipped {
            meta,
            document_id: prior.id,
            touch: true,
        });
    }

    let parse = || {
        parse_document(&meta, cancel, || match ocr {
            Some(client) => client.parse_pdf(bytes, cancel),
            None => parse::parse_bytes(path, &bytes),
        })
    };
    let (result, reuse) = match reuse {
        Some(cache) => {
            let (result, entry) = cache.resolve(&meta, &hash, cancel, parse)?;
            (result, Some(entry))
        }
        None => (parse()?.map(Arc::new), None),
    };
    check_cancel(cancel)?;
    Ok(FileResult::Done {
        meta,
        hash,
        result,
        reuse,
    })
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
/// 外层 Err 表示取消,内层 Err 才是可写入失败清单的解析错误。
fn parse_document(
    meta: &FileMeta,
    cancel: &AtomicBool,
    parse: impl FnOnce() -> Result<parse::Parsed, ParseError>,
) -> Result<Result<ParsedDocument, ParseError>, Cancelled> {
    check_cancel(cancel)?;
    let parsed = parse();
    check_cancel(cancel)?;
    match parsed {
        Ok(parsed) => Ok(Ok(build_document(meta, parsed, cancel)?)),
        Err(error) => Ok(Err(error)),
    }
}

fn build_document(
    meta: &FileMeta,
    parsed: parse::Parsed,
    cancel: &AtomicBool,
) -> Result<ParsedDocument, Cancelled> {
    check_cancel(cancel)?;
    let plain = text::markdown_to_plain(&parsed.markdown);
    check_cancel(cancel)?;
    let title = text::extract_title(&plain, &meta.stem());
    check_cancel(cancel)?;
    let chunks = chunk::chunk_document(&title, &plain);
    check_cancel(cancel)?;
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

/// 写库串行,只合并已到达的结果。取消或写库失败后继续排空通道但不写入,
/// 保证阻塞在有界 send 上的生产者能退出,也不提前宣告后台已结束。
fn consume_results(
    conn: &mut rusqlite::Connection,
    run_id: i64,
    rx: mpsc::Receiver<FileResult>,
    counts: &mut ImportCounts,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(ImportEvent),
) -> anyhow::Result<()> {
    let mut first_error = None;
    while let Ok(first) = rx.recv() {
        if first_error.is_some() || cancel.load(Ordering::Relaxed) {
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
        match write_batch(conn, run_id, batch, *counts, cancel) {
            Ok((new_counts, events)) => {
                *counts = new_counts;
                for (path, outcome, event_counts) in events {
                    on_event(ImportEvent::FileDone {
                        path,
                        outcome,
                        counts: event_counts,
                    });
                }
            }
            Err(error) if error.is::<Cancelled>() => {}
            Err(error) => {
                first_error = Some(error);
                cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 一批结果消费后待回放的 FileDone 事件(path、outcome、消费该条时的计数快照)。
type BatchEvents = Vec<(PathBuf, FileOutcome, ImportCounts)>;

/// 一批 worker 结果在同一个 BEGIN IMMEDIATE 事务里落库。
///
/// commit 成功才返回 (新计数, 逐条 FileDone 事件缓存);任何一步出错事务随
/// drop 回滚,计数与事件都不发布。取消也回滚整个未提交批次,保留旧缓存。
/// 提交后即使收到取消也回放本批事件,使 GUI 计数与已提交数据一致。
fn write_batch(
    conn: &mut rusqlite::Connection,
    run_id: i64,
    batch: Vec<FileResult>,
    counts: ImportCounts,
    cancel: &AtomicBool,
) -> anyhow::Result<(ImportCounts, BatchEvents)> {
    check_cancel(cancel)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .context("开启写入事务失败")?;
    let mut counts = counts;
    let mut events = Vec::with_capacity(batch.len());
    let mut published = Vec::new();
    for msg in batch {
        check_cancel(cancel)?;
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
    check_cancel(cancel)?;
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

    static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

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
            &NOT_CANCELLED,
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
        let cache = ReuseCache::new(
            &path,
            &load_existing(&conn, &NOT_CANCELLED).unwrap(),
            false,
            &NOT_CANCELLED,
        )
        .unwrap();
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
            &NOT_CANCELLED,
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
                let (parsed, _) = cache
                    .resolve(&meta, "same-hash", &NOT_CANCELLED, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        Ok(Ok(parsed_text(&meta, "# 正文标题\n\n正文")))
                    })
                    .unwrap();
                assert_eq!(parsed.unwrap().title, "正文标题");
            });
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let meta = file_meta("/d/a.csv");
        cache
            .resolve(&meta, "same-hash", &NOT_CANCELLED, || {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(Ok(parsed_text(&meta, "不同解析路由")))
            })
            .unwrap()
            .0
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn committed_duplicates_release_memory_and_reuse_persisted_blocks() {
        let (path, mut conn, cache) = test_cache();
        let hash = "a".repeat(64);
        let source = file_meta("/d/原件.txt");
        let (parsed, entry) = cache
            .resolve(&source, &hash, &NOT_CANCELLED, || {
                Ok(Ok(parsed_text(&source, "### 原件\n\n正文")))
            })
            .unwrap();
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
            &NOT_CANCELLED,
        )
        .unwrap();
        assert!(matches!(*entry.lock().unwrap(), Some(Reusable::Stored(_))));
        assert!(weak.upgrade().is_none(), "提交后不能一直缓存大文本");

        let copy = file_meta("/d/副本.md");
        // 覆盖同批提交后复用和重新启动导入后的跨批复用。
        let reopened = ReuseCache::new(
            &path,
            &load_existing(&conn, &NOT_CANCELLED).unwrap(),
            false,
            &NOT_CANCELLED,
        )
        .unwrap();
        for cache in [&cache, &reopened] {
            let (parsed, _) = cache
                .resolve(&copy, &hash, &NOT_CANCELLED, || {
                    panic!("相同内容不应再解析")
                })
                .unwrap();
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
        let (parsed, _) = cache
            .resolve(&copy, &hash, &NOT_CANCELLED, || {
                Ok(Ok(parsed_text(&copy, "回退重新解析")))
            })
            .unwrap();
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
        let (result, _) = cache
            .resolve(&meta, "hash", &NOT_CANCELLED, || {
                Ok(Err(ParseError {
                    code: parse::ParseErrorCode::Empty,
                    message: "空文件".into(),
                    detail: String::new(),
                }))
            })
            .unwrap();
        assert!(result.is_err());
        let (result, entry) = cache
            .resolve(&meta, "hash", &NOT_CANCELLED, || {
                Ok(Ok(parsed_text(&meta, "正常重试")))
            })
            .unwrap();
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
                ImportCounts::default(),
                &NOT_CANCELLED
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
        let result = write_batch(&mut conn, run_id, batch, counts, &NOT_CANCELLED);
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
        let (counts, events) =
            write_batch(&mut conn, run_id, batch, counts, &NOT_CANCELLED).unwrap();
        assert_eq!(counts.ok, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].2.processed, 1);
        assert_eq!(events[1].2.processed, 2);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 2);
    }

    /// 在指定 SQL 操作次数后置位,精确覆盖事务中途/提交前取消,不依赖机器速度。
    fn cancel_after_sql_calls(
        conn: &rusqlite::Connection,
        cancel: &Arc<AtomicBool>,
        after: usize,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let cancel = Arc::clone(cancel);
        conn.create_scalar_function(
            "cancel_import",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| {
                if called.fetch_add(1, Ordering::Relaxed) + 1 == after {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(0i64)
            },
        )
        .unwrap();
        calls
    }

    #[test]
    fn cancellation_rolls_back_uncommitted_batch_and_preserves_prior_data_and_cache() {
        // 第 1 条后取消,以及最后一条写完、commit 之前取消。
        for after in [1, 3] {
            let (_path, mut conn, _cache) = test_cache();
            let run_id = repo::create_run(&conn, "files", None, 4, 1_000).unwrap();
            let (counts, _) = write_batch(
                &mut conn,
                run_id,
                vec![done_result("/d/保留.txt", "old")],
                ImportCounts {
                    total: 4,
                    ..Default::default()
                },
                &NOT_CANCELLED,
            )
            .unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let calls = cancel_after_sql_calls(&conn, &cancel, after);
            conn.execute_batch(
                "CREATE TEMP TRIGGER cancel_item AFTER INSERT ON import_items \
                 BEGIN SELECT cancel_import(); END;",
            )
            .unwrap();
            let mut first = done_result("/d/新文件.txt", "new");
            let entry = if let FileResult::Done {
                result: Ok(parsed),
                reuse,
                ..
            } = &mut first
            {
                let entry = Arc::new(Mutex::new(Some(Reusable::Parsed(Arc::clone(parsed)))));
                *reuse = Some(Arc::clone(&entry));
                entry
            } else {
                unreachable!()
            };
            let result = write_batch(
                &mut conn,
                run_id,
                vec![
                    first,
                    done_result("/d/乙.txt", "b"),
                    done_result("/d/丙.txt", "c"),
                ],
                counts,
                &cancel,
            );
            assert!(matches!(result, Err(error) if error.is::<Cancelled>()));
            assert_eq!(calls.load(Ordering::Relaxed), after);
            assert_eq!(counts.processed, 1, "取消不能发布回滚批次的计数");
            assert!(conn.is_autocommit());
            for table in [
                "documents",
                "document_contents",
                "documents_fts",
                "import_items",
            ] {
                assert_eq!(
                    row_count(&conn, &format!("SELECT count(*) FROM {table}")),
                    1
                );
            }
            assert!(matches!(*entry.lock().unwrap(), Some(Reusable::Parsed(_))));
            assert!(
                crate::maintain::check_integrity(&conn)
                    .unwrap()
                    .is_consistent()
            );
        }
    }

    #[test]
    fn cancellation_during_seed_rolls_back_pending_items() {
        for after in [1, 3] {
            let mut conn = store::open_in_memory().unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let calls = cancel_after_sql_calls(&conn, &cancel, after);
            conn.execute_batch(
                "CREATE TEMP TRIGGER cancel_pending AFTER INSERT ON import_items \
                 BEGIN SELECT cancel_import(); END;",
            )
            .unwrap();
            let id = repo::create_run(&conn, "files", None, 3, 1_000).unwrap();
            let files = vec!["/a.txt".into(), "/b.txt".into(), "/c.txt".into()];
            let error = seed_items(&mut conn, id, &files, 1_000, &cancel).unwrap_err();
            assert!(error.is::<Cancelled>());
            assert_eq!(calls.load(Ordering::Relaxed), after);
            assert_eq!(row_count(&conn, "SELECT count(*) FROM import_items"), 0);
            assert!(conn.is_autocommit());
        }
    }

    #[test]
    fn cancellation_while_loading_existing_stops_reading_rows() {
        let mut conn = store::open_in_memory().unwrap();
        let id = repo::create_run(&conn, "files", None, 3, 1_000).unwrap();
        write_batch(
            &mut conn,
            id,
            vec![
                done_result("/a.txt", "a"),
                done_result("/b.txt", "b"),
                done_result("/c.txt", "c"),
            ],
            ImportCounts::default(),
            &NOT_CANCELLED,
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let calls = cancel_after_sql_calls(&conn, &cancel, 2);
        // 只在本连接替换读取视图,每读一行都会执行一次测试函数。
        conn.execute_batch(
            "CREATE TEMP VIEW documents AS SELECT id + cancel_import() AS id, \
             canonical_path, file_size, file_mtime_ms, content_hash, parse_status, \
             source_root, parse_error_code, file_type, ext FROM main.documents",
        )
        .unwrap();
        assert!(matches!(load_existing(&conn, &cancel), Err(error) if error.is::<Cancelled>()));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn scan_progress_counts_only_selected_unique_files() {
        let (db, _conn, _cache) = test_cache();
        let dir = db.parent().unwrap().join("docs");
        for subdir in ["nested", ".hidden", "node_modules"] {
            std::fs::create_dir_all(dir.join(subdir)).unwrap();
        }
        for name in [
            "a.TXT",
            "nested/b.txt",
            "ignored.md",
            "unknown.xyz",
            ".hidden.txt",
            ".hidden/c.txt",
            "node_modules/d.txt",
        ] {
            std::fs::write(dir.join(name), "正文").unwrap();
        }
        let inputs = [dir.clone(), dir.join("a.TXT"), dir.join("nested")];
        let extensions = BTreeSet::from(["TXT".to_owned()]);
        let mut found = Vec::new();
        let files = scan_paths_filtered(
            &inputs,
            Some(&extensions),
            || Ok(()),
            |count| found.push(count),
        )
        .unwrap();
        assert_eq!(found, [0, 1, 2]);
        assert_eq!(files.len(), 2);
        assert!(files.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn scanning_events_precede_processing_and_include_final_count() {
        for matching in [false, true] {
            let (db, _conn, _cache) = test_cache();
            let dir = db.parent().unwrap().join("docs");
            std::fs::create_dir(&dir).unwrap();
            for name in ["a.txt", "b.txt"] {
                std::fs::write(dir.join(name), "正文").unwrap();
            }
            let expected = if matching { 2 } else { 0 };
            let mut found = Vec::new();
            let mut scanned = false;
            let counts = run_import(
                &db,
                vec![dir],
                ImportOptions {
                    extensions: Some([if matching { "txt" } else { "md" }.to_owned()].into()),
                    ..Default::default()
                },
                Arc::new(AtomicBool::new(false)),
                &mut |event| match event {
                    ImportEvent::Scanning { found: count } => {
                        assert!(!scanned);
                        found.push(count);
                    }
                    ImportEvent::Scanned { total } => {
                        assert_eq!(total, expected);
                        assert_eq!(found.last(), Some(&total));
                        scanned = true;
                    }
                    ImportEvent::FileDone { .. } => assert!(scanned),
                    ImportEvent::Finished { cancelled, .. } => assert!(!cancelled),
                    ImportEvent::Error { message } => panic!("{message}"),
                },
            )
            .unwrap();
            assert!(scanned);
            assert_eq!(found.first(), Some(&0));
            assert!(found.windows(2).all(|pair| pair[0] <= pair[1]));
            assert_eq!((counts.total, counts.ok), (expected, expected));
        }
    }

    #[test]
    fn scan_progress_callback_can_cancel_a_partial_scan() {
        let (db, _conn, _cache) = test_cache();
        let dir = db.parent().unwrap().join("docs");
        std::fs::create_dir(&dir).unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.join(name), "正文").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut found = Vec::new();
        let result = scan_paths_filtered(
            &[dir],
            None,
            || check_cancel(&cancel),
            |count| {
                found.push(count);
                if count == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(matches!(result, Err(Cancelled)));
        assert_eq!(found, [0, 1]);
    }

    #[test]
    fn cancellation_from_scanning_event_does_not_process_files() {
        let (db, conn, _cache) = test_cache();
        let file = db.parent().unwrap().join("a.txt");
        std::fs::write(&file, "正文").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut finished = false;
        let counts = run_import(
            &db,
            vec![file],
            ImportOptions::default(),
            Arc::clone(&cancel),
            &mut |event| match event {
                ImportEvent::Scanning { found } => {
                    assert_eq!(found, 0);
                    cancel.store(true, Ordering::Relaxed);
                }
                ImportEvent::Finished { cancelled, .. } => finished = cancelled,
                _ => panic!("扫描期间取消不应处理文件或报告错误"),
            },
        )
        .unwrap();
        assert!(finished);
        assert_eq!((counts.total, counts.processed), (0, 0));
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 0);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM import_items"), 0);
    }

    #[test]
    fn cancellation_during_directory_enumeration_discards_partial_scan() {
        let (db, _conn, _cache) = test_cache();
        let dir = db.parent().unwrap().join("docs");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..40 {
            std::fs::write(dir.join(format!("{i}.txt")), "正文").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut checks = 0;
        let scanned = scan_paths_filtered(
            std::slice::from_ref(&dir),
            None,
            || {
                checks += 1;
                // 已进入 read_dir,但尚未遍历完目录。
                if checks == 10 {
                    cancel.store(true, Ordering::Relaxed);
                }
                check_cancel(&cancel)
            },
            |_| {},
        );
        assert!(scanned.is_err());
        assert_eq!(checks, 10);
        assert_eq!(scan_paths(&[dir]).len(), 40);
    }

    #[test]
    fn cancelled_parser_return_is_discarded_not_converted_or_cached() {
        let (_path, _conn, cache) = test_cache();
        let meta = file_meta("/d/甲.txt");
        for succeeds in [true, false] {
            let cancel = AtomicBool::new(false);
            let result = cache.resolve(&meta, "hash", &cancel, || {
                parse_document(&meta, &cancel, || {
                    // 模拟不可中断解析器返回前收到取消,覆盖成功与失败返回。
                    cancel.store(true, Ordering::Relaxed);
                    if succeeds {
                        Ok(parse::Parsed {
                            markdown: "正文".into(),
                            warnings: vec![],
                            parser_name: "test",
                            parser_version: "1",
                        })
                    } else {
                        Err(ParseError {
                            code: parse::ParseErrorCode::Malformed,
                            message: "损坏".into(),
                            detail: String::new(),
                        })
                    }
                })
            });
            assert!(matches!(result, Err(Cancelled)));
            let entry = cache
                .entries
                .lock()
                .unwrap()
                .get(&("hash".into(), "text".into()))
                .unwrap()
                .clone();
            assert!(entry.lock().unwrap().is_none(), "取消产物不能发布到缓存");
            assert!(
                cache
                    .resolve(&meta, "hash", &cancel, || panic!("取消后不能重试副本"))
                    .is_err()
            );
        }
    }

    #[test]
    fn duplicate_waiters_do_not_retry_after_inflight_parse_is_cancelled() {
        let (_path, _conn, cache) = test_cache();
        let cancel = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let ready = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let cache = &cache;
            let cancel = &cancel;
            let calls = &calls;
            scope.spawn(move || {
                let meta = file_meta("/d/原件.txt");
                let result = cache.resolve(&meta, "same", cancel, || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    // 此时仍持有内容键的锁,其他副本不能开始解析。
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(Err(ParseError {
                        code: parse::ParseErrorCode::Malformed,
                        message: "损坏".into(),
                        detail: String::new(),
                    }))
                });
                assert!(matches!(result, Err(Cancelled)));
            });
            started
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            for _ in 0..3 {
                let ready = &ready;
                scope.spawn(move || {
                    ready.wait();
                    let meta = file_meta("/d/副本.txt");
                    let result = cache.resolve(&meta, "same", cancel, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        panic!("取消后拿到缓存锁也不能重试失败副本")
                    });
                    assert!(matches!(result, Err(Cancelled)));
                });
            }
            ready.wait();
            cancel.store(true, Ordering::Relaxed);
            release.send(()).unwrap();
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn actual_write_error_is_not_hidden_by_concurrent_cancellation() {
        let mut conn = store::open_in_memory().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        cancel_after_sql_calls(&conn, &cancel, 1);
        conn.execute_batch(
            "CREATE TEMP TRIGGER cancel_and_fail BEFORE INSERT ON import_items \
             BEGIN SELECT cancel_import(); SELECT RAISE(ABORT, '实际写库错误'); END;",
        )
        .unwrap();
        let id = repo::create_run(&conn, "files", None, 2, 1_000).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(done_result("/a.txt", "a")).unwrap();
        tx.send(done_result("/b.txt", "b")).unwrap();
        drop(tx);
        let mut counts = ImportCounts::default();
        let error = consume_results(&mut conn, id, rx, &mut counts, &cancel, &mut |_| {
            panic!("不能发布失败事务的事件")
        })
        .unwrap_err();
        assert!(!error.is::<Cancelled>());
        assert!(format!("{error:#}").contains("实际写库错误"));
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(counts.processed, 0);
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 0);
    }

    #[test]
    fn cancellation_drains_full_channel_without_writing_or_blocking_sender() {
        let mut conn = store::open_in_memory().unwrap();
        let id = repo::create_run(&conn, "files", None, 4, 1_000).unwrap();
        let cancel = AtomicBool::new(false);
        let mut counts = ImportCounts {
            total: 4,
            ..Default::default()
        };
        let (tx, rx) = mpsc::sync_channel(1);
        let (start, wait) = mpsc::channel();
        let (full, filled) = mpsc::channel();
        let (ended, joined) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            tx.send(done_result("/a.txt", "a")).unwrap();
            wait.recv().unwrap();
            tx.send(done_result("/b.txt", "b")).unwrap();
            full.send(()).unwrap();
            // 队列已满,这里的发送只能等消费者取消后继续排空才能返回。
            tx.send(done_result("/c.txt", "c")).unwrap();
            tx.send(done_result("/d.txt", "d")).unwrap();
            ended.send(()).unwrap();
        });
        let mut events = 0;
        consume_results(&mut conn, id, rx, &mut counts, &cancel, &mut |_| {
            events += 1;
            start.send(()).unwrap();
            filled
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap();
        joined
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        sender.join().unwrap();
        assert_eq!(events, 1);
        assert_eq!((counts.processed, counts.ok, counts.failed), (1, 1, 0));
        assert_eq!(row_count(&conn, "SELECT count(*) FROM documents"), 1);
    }
}
