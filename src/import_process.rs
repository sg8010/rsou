//! 可终止的导入进程。先协作取消,超过宽限时间则终止并等待进程退出。
//! SQLite 会回滚未提交事务;最终计数从本批已提交的 import_items 恢复。

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::import::{self, ImportCounts, ImportEvent, ImportOptions};
use crate::{repo, store};

const WORKER_ARG: &str = "--rsou-import-worker";
const MESSAGE_PREFIX: &str = "RSOU_IMPORT:";
const CANCEL_GRACE: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
struct Request {
    #[serde(with = "crate::ipc_path")]
    db_path: PathBuf,
    #[serde(with = "crate::ipc_path::list")]
    inputs: Vec<PathBuf>,
    options: ImportOptions,
    run_id: i64,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Event(ImportEvent),
    Complete(Result<ImportCounts, String>),
}

fn write_message(message: Message) -> anyhow::Result<()> {
    // 序列化失败不能污染管道,否则下一条消息也会被当作破损 JSON。
    let encoded = serde_json::to_vec(&message)?;
    let mut out = io::stdout().lock();
    // 第三方解析器即使输出了未换行的日志,也不能吞掉后续协议消息。
    out.write_all(b"\n")?;
    out.write_all(MESSAGE_PREFIX.as_bytes())?;
    out.write_all(&encoded)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn monitor_control(mut input: impl BufRead, cancel: &AtomicBool) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                // 父进程已退出/管道断开:不能再依赖父进程强杀。
                // 给协作退出同样的宽限时间,然后结束整个子进程,不等待解析线程。
                cancel.store(true, Ordering::Relaxed);
                std::thread::sleep(CANCEL_GRACE);
                std::process::exit(1);
            }
            Ok(_) => cancel.store(true, Ordering::Relaxed),
        }
    }
}

/// 必须在 GUI 初始化/CLI 参数解析之前调用;普通启动返回 None。
pub fn worker_main() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(WORKER_ARG)) {
        return None;
    }
    let result = (|| -> anyhow::Result<()> {
        let mut input = BufReader::new(io::stdin());
        let mut line = String::new();
        input.read_line(&mut line)?;
        let request: Request = serde_json::from_str(&line).context("读取导入任务失败")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("rsou-import-control".into())
            .spawn(move || monitor_control(input, &flag))?;
        let result = import::run_import_in_run(
            &request.db_path,
            request.inputs,
            request.options,
            Arc::clone(&cancel),
            &mut |event| {
                if write_message(Message::Event(event)).is_err() {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            Some(request.run_id),
        );
        write_message(Message::Complete(
            result.map_err(|error| format!("{error:#}")),
        ))
    })();
    match result {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("导入进程失败: {error:#}");
            Some(1)
        }
    }
}

/// executable 须为支持 worker_main 的 rsou 或 rsou-cli,不依赖旁置辅助程序。
pub fn run_import(
    executable: &Path,
    db_path: &Path,
    inputs: Vec<PathBuf>,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
    on_event: &mut dyn FnMut(ImportEvent),
) -> anyhow::Result<ImportCounts> {
    let mut command = Command::new(executable);
    command.arg(WORKER_ARG);
    run_with_command(command, db_path, inputs, options, cancel, on_event)
}

fn run_with_command(
    command: Command,
    db_path: &Path,
    inputs: Vec<PathBuf>,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
    on_event: &mut dyn FnMut(ImportEvent),
) -> anyhow::Result<ImportCounts> {
    let conn = store::open(db_path, store::OpenMode::ReadWrite)?;
    // 不在监控端探测输入路径:共享盘的 is_dir/metadata 也可能卡住。
    // 先保留可恢复的批次 ID,来源由可终止的子进程探测后补全。
    let run_id = repo::create_run(&conn, "pending", None, 0, repo::now_ms())?;
    drop(conn);
    let request = Request {
        db_path: db_path.to_path_buf(),
        inputs,
        options,
        run_id,
    };
    let mut progress = Progress::default();
    let mut result = (|| -> anyhow::Result<bool> {
        let mut worker = Worker::spawn(command, &request)?;
        let mut cancelling_since = None;
        let mut complete_since = None;
        let mut forced = false;
        let status = loop {
            if cancel.load(Ordering::Relaxed) && cancelling_since.is_none() {
                worker.cancel();
                cancelling_since = Some(Instant::now());
            }
            if cancelling_since.is_some_and(|at: Instant| at.elapsed() >= CANCEL_GRACE)
                || complete_since.is_some_and(|at: Instant| at.elapsed() >= CANCEL_GRACE)
            {
                forced = true;
                break worker.terminate()?;
            }
            match worker.messages.recv_timeout(POLL_INTERVAL) {
                Ok(message) => {
                    progress.accept(message?, on_event)?;
                    if progress.complete.is_some() && complete_since.is_none() {
                        complete_since = Some(Instant::now());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // 正常退出时 stdout 可能略早于进程关闭,继续检查退出状态。
                    if progress.complete.is_none() {
                        bail!("导入进程通信中断,请重试");
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            if let Some(status) = worker.child.try_wait()? {
                break status;
            }
        };
        // 回收进程和管道线程后才发布终态,避免下一批与旧任务重叠。
        worker.join_io();
        for message in worker.messages.try_iter() {
            progress.accept(message?, on_event)?;
        }
        match progress.complete.take() {
            Some(Err(error)) => bail!("{error}"),
            Some(Ok(_)) => {}
            None if forced => return Ok(true),
            None => bail!("导入进程未报告处理结果"),
        }
        anyhow::ensure!(status.success() || forced, "导入进程异常退出: {status}");
        progress.cancelled.context("导入进程未报告完成状态")
    })();
    // 进程已退出:以数据库为准,即使 commit 后还没来得及发送进度也不会漏计。
    let conn = store::open(db_path, store::OpenMode::ReadWrite)?;
    let counts = committed_counts(&conn, run_id, progress.total)?;
    if matches!(result, Ok(true)) {
        let (status, message): (String, Option<String>) = conn.query_row(
            "SELECT status, message FROM import_runs WHERE id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // 取消与真实错误/正常完成同时发生时,不能覆盖进程已经写下的终态。
        match status.as_str() {
            "failed" => {
                result = Err(anyhow::anyhow!(
                    message.unwrap_or_else(|| "导入失败".into())
                ))
            }
            "done" => result = Ok(false),
            _ => {}
        }
    }
    let (status, message) = match &result {
        Ok(true) => ("cancelled", None),
        Ok(false) => ("done", None),
        Err(error) => ("failed", Some(format!("{error:#}"))),
    };
    repo::finish_run(
        &conn,
        run_id,
        status,
        &counts,
        message.as_deref(),
        repo::now_ms(),
    )?;
    let cancelled = result?;
    on_event(ImportEvent::Finished {
        run_id,
        counts,
        cancelled,
    });
    Ok(counts)
}

#[derive(Default)]
struct Progress {
    total: usize,
    cancelled: Option<bool>,
    complete: Option<Result<ImportCounts, String>>,
}

impl Progress {
    fn accept(
        &mut self,
        message: Message,
        on_event: &mut dyn FnMut(ImportEvent),
    ) -> anyhow::Result<()> {
        match message {
            Message::Event(ImportEvent::Finished {
                cancelled, counts, ..
            }) => {
                self.total = counts.total;
                self.cancelled = Some(cancelled);
            }
            Message::Event(ImportEvent::Error { message }) => bail!("{message}"),
            Message::Event(event) => {
                self.total = match &event {
                    ImportEvent::Scanned { total } => *total,
                    ImportEvent::FileDone { counts, .. } => counts.total,
                    _ => self.total,
                };
                on_event(event);
            }
            Message::Complete(result) => self.complete = Some(result),
        }
        Ok(())
    }
}

fn committed_counts(
    conn: &rusqlite::Connection,
    run_id: i64,
    total: usize,
) -> anyhow::Result<ImportCounts> {
    let stored_total: i64 = conn.query_row(
        "SELECT total_files FROM import_runs WHERE id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    let counts = conn.query_row(
        "SELECT count(*), coalesce(sum(status = 'ok'), 0), \
         coalesce(sum(status = 'failed'), 0), coalesce(sum(status = 'skipped'), 0) \
         FROM import_items WHERE run_id = ?1",
        [run_id],
        |row| {
            let seeded = row.get::<_, i64>(0)? as usize;
            let ok = row.get::<_, i64>(1)? as usize;
            let failed = row.get::<_, i64>(2)? as usize;
            let skipped = row.get::<_, i64>(3)? as usize;
            Ok(ImportCounts {
                total: total.max(seeded).max(stored_total as usize),
                processed: ok + failed + skipped,
                ok,
                failed,
                skipped,
            })
        },
    )?;
    Ok(counts)
}

struct Worker {
    child: Child,
    control: Option<mpsc::Sender<()>>,
    messages: mpsc::Receiver<anyhow::Result<Message>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(mut command: Command, request: &Request) -> anyhow::Result<Self> {
        let mut encoded = serde_json::to_vec(request)?;
        encoded.push(b'\n');
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW,兼容 Windows 7。
        }
        let child = command.spawn().context("无法启动独立导入进程")?;
        let (send, messages) = mpsc::channel();
        let mut worker = Self {
            child,
            control: None,
            messages,
            reader: None,
            writer: None,
        };
        let output = worker.child.stdout.take().context("无法读取导入进程")?;
        worker.reader = Some(
            std::thread::Builder::new()
                .name("rsou-import-events".into())
                .spawn(move || {
                    let mut output = BufReader::new(output);
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        let message = match output.read_until(b'\n', &mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                // 终止进程可能截断最后一条消息,未写完的进度不能作为解析错误。
                                // 必须先判断完整性再解码,末尾可能只留下半个 UTF-8 字符。
                                if !line.ends_with(b"\n") {
                                    break;
                                }
                                let Some(json) = line.strip_prefix(MESSAGE_PREFIX.as_bytes())
                                else {
                                    continue;
                                };
                                serde_json::from_slice(json).context("导入进度消息无效")
                            }
                            Err(error) => Err(error).context("读取导入进度失败"),
                        };
                        let failed = message.is_err();
                        if send.send(message).is_err() || failed {
                            break;
                        }
                    }
                })?,
        );
        let mut input = worker.child.stdin.take().context("无法控制导入进程")?;
        let (control, requests) = mpsc::channel();
        worker.control = Some(control);
        worker.writer = Some(
            std::thread::Builder::new()
                .name("rsou-import-request".into())
                .spawn(move || {
                    // 传送大量路径也不能阻塞监控循环;终止进程会解除管道写入等待。
                    if input
                        .write_all(&encoded)
                        .and_then(|_| input.flush())
                        .is_err()
                    {
                        return;
                    }
                    // 发出取消后仍保持 stdin 打开,让 EOF 只表示监控端已退出。
                    while requests.recv().is_ok() {
                        if input
                            .write_all(b"cancel\n")
                            .and_then(|_| input.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                })?,
        );
        Ok(worker)
    }

    fn cancel(&self) {
        if let Some(control) = &self.control {
            let _ = control.send(());
        }
    }

    fn terminate(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(status);
        }
        if let Err(error) = self.child.kill() {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            return Err(error);
        }
        self.child.wait()
    }

    fn join_io(&mut self) {
        self.control = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.terminate();
        self.join_io();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::FileOutcome;
    use crate::parse::{ParseError, ParseErrorCode};

    fn documents(tag: &str) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!(
            "rsou-process-{tag}-{}-{}",
            std::process::id(),
            repo::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<_> = ["保留.txt", "待处理.txt"]
            .iter()
            .map(|name| dir.join(name))
            .collect();
        for path in &paths {
            std::fs::write(path, "# 已提交标题\n\n保留检索正文").unwrap();
        }
        (dir.join("index.db"), paths)
    }

    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "import_process::tests::blocked_worker_fixture",
            "--nocapture",
            "--quiet",
        ]);
        command.env("RSOU_TEST_IMPORT_FIXTURE", mode);
        command
    }

    // 子进程测试夹具:真实提交一篇,再持有修改正文/FTS 的事务,且故意不响应取消。
    #[test]
    fn blocked_worker_fixture() {
        let Ok(mode) = std::env::var("RSOU_TEST_IMPORT_FIXTURE") else {
            return;
        };
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        let request: Request = serde_json::from_str(&line).unwrap();
        if mode == "startup" {
            let conn = store::open(&request.db_path, store::OpenMode::ReadOnly).unwrap();
            let source: (String, Option<String>) = conn
                .query_row(
                    "SELECT kind, root_path FROM import_runs WHERE id = ?1",
                    [request.run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            // 即使输入是目录,监控端也不应自行探测;这里模拟探测阶段永久阻塞。
            assert_eq!(source, ("pending".into(), None));
            write_message(Message::Event(ImportEvent::Scanned { total: 0 })).unwrap();
            loop {
                std::thread::park();
            }
        }
        import::run_import_in_run(
            &request.db_path,
            vec![request.inputs[0].clone()],
            ImportOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
            Some(request.run_id),
        )
        .unwrap();
        let mut conn = store::open(&request.db_path, store::OpenMode::ReadWrite).unwrap();
        conn.execute(
            "UPDATE import_runs SET status = 'running', finished_at = NULL, total_files = 2 WHERE id = ?1",
            [request.run_id],
        ).unwrap();
        repo::upsert_item(
            &conn,
            request.run_id,
            &request.inputs[1],
            "pending",
            None,
            None,
            None,
            repo::now_ms(),
        )
        .unwrap();
        repo::set_setting(&conn, "test_worker_pid", &std::process::id().to_string()).unwrap();
        let doc = repo::find_document_by_path(&conn, &request.inputs[0])
            .unwrap()
            .unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        repo::save_failed_in(
            &tx,
            &repo::FileMeta::of(&request.inputs[0]).unwrap().unwrap(),
            "uncommitted",
            &ParseError {
                code: ParseErrorCode::Malformed,
                message: "不能保留的修改".into(),
                detail: String::new(),
            },
            repo::now_ms(),
        )
        .unwrap();
        // 独占事务已开始,读者此时仍只能看到之前已提交的正文。
        println!();
        write_message(Message::Event(ImportEvent::Scanned { total: 2 })).unwrap();
        if mode != "no-progress" && !mode.starts_with("partial") {
            write_message(Message::Event(ImportEvent::FileDone {
                path: request.inputs[0].clone(),
                outcome: FileOutcome::Ok {
                    document_id: doc.id,
                },
                counts: ImportCounts {
                    total: 2,
                    processed: 1,
                    ok: 1,
                    ..Default::default()
                },
            }))
            .unwrap();
        }
        if mode == "crash" {
            std::process::exit(17);
        }
        if mode == "partial" {
            print!("{MESSAGE_PREFIX}{{\"Event\":");
            io::stdout().flush().unwrap();
        }
        if mode == "partial-utf8" {
            let mut out = io::stdout().lock();
            out.write_all(MESSAGE_PREFIX.as_bytes()).unwrap();
            out.write_all(b"{\"Event\":{\"Error\":{\"message\":\"\xe4\xb8")
                .unwrap();
            out.flush().unwrap();
        }
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn stuck_worker_is_terminated_and_uncommitted_changes_roll_back() {
        for mode in ["stuck", "no-progress", "partial", "partial-utf8"] {
            let (db, paths) = documents(mode);
            let cancel = Arc::new(AtomicBool::new(false));
            let start = Instant::now();
            let mut finished = 0;
            let counts = run_with_command(
                fixture_command(mode),
                &db,
                paths.clone(),
                ImportOptions::default(),
                Arc::clone(&cancel),
                &mut |event| match event {
                    ImportEvent::Scanned { .. } if mode != "stuck" => {
                        cancel.store(true, Ordering::Relaxed)
                    }
                    ImportEvent::FileDone { .. } => cancel.store(true, Ordering::Relaxed),
                    ImportEvent::Finished {
                        cancelled, counts, ..
                    } => {
                        assert!(cancelled);
                        assert_eq!(counts.processed, 1);
                        finished += 1;
                    }
                    _ => {}
                },
            )
            .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "取消不能无限等待解析器"
            );
            assert_eq!(finished, 1);
            assert_eq!(
                (counts.total, counts.processed, counts.ok, counts.failed),
                (2, 1, 1, 0)
            );
            let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
            let history: (String, i64, i64) = conn
                .query_row(
                    "SELECT status, total_files, processed_files FROM import_runs",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(history, ("cancelled".into(), 2, 1));
            let doc = repo::find_document_by_path(&conn, &paths[0])
                .unwrap()
                .unwrap();
            assert_eq!(doc.parse_status, "parsed", "未提交的覆盖必须回滚");
            assert!(
                repo::find_document_by_path(&conn, &paths[1])
                    .unwrap()
                    .is_none()
            );
            assert!(
                crate::maintain::check_integrity(&conn)
                    .unwrap()
                    .is_consistent()
            );
            #[cfg(target_os = "linux")]
            {
                let pid = repo::get_setting(&conn, "test_worker_pid")
                    .unwrap()
                    .unwrap();
                assert!(
                    !Path::new(&format!("/proc/{pid}")).exists(),
                    "终态前必须回收进程"
                );
            }
            drop(conn);
            let resumed = import::run_import(
                &db,
                paths,
                ImportOptions::default(),
                Arc::new(AtomicBool::new(false)),
                &mut |_| {},
            )
            .unwrap();
            assert_eq!((resumed.processed, resumed.ok, resumed.skipped), (2, 1, 1));
        }
    }

    #[test]
    fn cancellation_during_startup_does_not_wait_for_input_classification() {
        let (db, _) = documents("startup");
        let cancel = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let mut finished = false;
        let counts = run_with_command(
            fixture_command("startup"),
            &db,
            vec![db.parent().unwrap().to_path_buf()],
            ImportOptions::default(),
            Arc::clone(&cancel),
            &mut |event| match event {
                ImportEvent::Scanned { .. } => cancel.store(true, Ordering::Relaxed),
                ImportEvent::Finished { cancelled, .. } => finished = cancelled,
                _ => {}
            },
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(finished);
        assert_eq!((counts.total, counts.processed), (0, 0));
    }

    #[test]
    fn request_and_event_paths_round_trip_without_unicode_loss() {
        let mut paths = vec![PathBuf::from("普通路径/换行\n\".txt")];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            paths.push(std::ffi::OsString::from_vec(b"invalid-\xff.txt".to_vec()).into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            paths.push(std::ffi::OsString::from_wide(&[0xd800, b'.' as u16, b't' as u16]).into());
        }
        for path in &paths {
            let request = Request {
                db_path: path.clone(),
                inputs: paths.clone(),
                options: ImportOptions::default(),
                run_id: 42,
            };
            let encoded = serde_json::to_vec(&request).unwrap();
            let decoded: Request = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.db_path, request.db_path);
            assert_eq!(decoded.inputs, request.inputs);
            let event = Message::Event(ImportEvent::FileDone {
                path: path.clone(),
                outcome: FileOutcome::Skipped,
                counts: ImportCounts::default(),
            });
            let encoded = serde_json::to_vec(&event).unwrap();
            let decoded: Message = serde_json::from_slice(&encoded).unwrap();
            let Message::Event(ImportEvent::FileDone { path: decoded, .. }) = decoded else {
                panic!("应保留事件类型");
            };
            assert_eq!(&decoded, path);
        }
    }

    #[test]
    fn crashed_worker_is_reported_as_failure_not_left_running() {
        let (db, paths) = documents("crash");
        let error = run_with_command(
            fixture_command("crash"),
            &db,
            paths,
            ImportOptions::default(),
            Arc::new(AtomicBool::new(false)),
            &mut |event| assert!(!matches!(event, ImportEvent::Finished { .. })),
        )
        .unwrap_err();
        assert!(error.to_string().contains("进程"));
        let conn = store::open(&db, store::OpenMode::ReadWrite).unwrap();
        let history: (String, i64) = conn
            .query_row(
                "SELECT status, processed_files FROM import_runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(history, ("failed".into(), 1));
        assert!(
            crate::maintain::check_integrity(&conn)
                .unwrap()
                .is_consistent()
        );
    }
}
