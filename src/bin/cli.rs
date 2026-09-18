//! rsou 命令行工具:在无桌面环境(CI / WSL 无 X)下直接验证索引库与解析管线。
//!
//! 用法:`rsou-cli [--db PATH] <命令> [参数]`
//! 所有访问索引库的子命令都经 `store::open` 打开库——这同时是「检索必须注册
//! tokenizer」这条约束的活文档:用系统 sqlite3 CLI 对库做 MATCH 会因找不到
//! tokenizer 报错。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rsou_lib::import::{self, FileOutcome, ImportEvent, ImportOptions};
use rsou_lib::parse;
use rsou_lib::store::{self, OpenMode};
use rsou_lib::{chunk, text};

const USAGE: &str = "用法: rsou-cli [--db PATH] <命令> [参数]

命令:
  docs                  打印 documents 表(id/文件名/类型/状态/分块数)
  stats                 打印文档数、分块数、索引文件大小与数据目录
  schema                打印 sqlite_master 中的建表语句(验证 FTS 表已建)
  import <路径...>      导入文件或目录(目录递归);--force 全部重解析
  parse <文件>          解析单个文件,把 Markdown 打到 stdout
  text <文件>           解析单个文件,打印 plain_text 与分块边界摘要

选项:
  --db PATH   索引库路径,缺省用应用数据目录下的 index.sqlite3
  --force     import:忽略 hash/mtime 跳过,全部重解析";

fn main() -> ExitCode {
    let mut db_path: Option<PathBuf> = None;
    let mut force = false;
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => match args.next() {
                Some(value) => db_path = Some(PathBuf::from(value)),
                None => {
                    eprintln!("--db 需要一个路径参数\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--force" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if !arg.starts_with('-') => positional.push(arg),
            _ => {
                eprintln!("无法识别的参数: {arg}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(command) = positional.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest: Vec<PathBuf> = positional[1..].iter().map(PathBuf::from).collect();
    let db_path = db_path.unwrap_or_else(|| store::data_dirs().db_path);

    let result = match command.as_str() {
        "docs" => run(&db_path, cmd_docs),
        "stats" => run(&db_path, cmd_stats),
        "schema" => run(&db_path, cmd_schema),
        "import" => {
            if rest.is_empty() {
                eprintln!("import 需要至少一个文件或目录路径\n{USAGE}");
                return ExitCode::from(2);
            }
            cmd_import(&db_path, rest, force)
        }
        "parse" => match rest.first() {
            Some(path) => cmd_parse(path),
            None => {
                eprintln!("parse 需要一个文件路径\n{USAGE}");
                return ExitCode::from(2);
            }
        },
        "text" => match rest.first() {
            Some(path) => cmd_text(path),
            None => {
                eprintln!("text 需要一个文件路径\n{USAGE}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!("未知命令: {command}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    path: &Path,
    body: fn(&rusqlite::Connection, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let connection = store::open(path, OpenMode::ReadWrite)?;
    body(&connection, path)
}

fn cmd_docs(connection: &rusqlite::Connection, _path: &Path) -> anyhow::Result<()> {
    let mut stmt = connection.prepare(
        "SELECT id, file_name, file_type, parse_status, chunk_count FROM documents ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        println!("资料库为空");
        return Ok(());
    }
    println!("id\t文件名\t类型\t状态\t分块数");
    for (id, name, file_type, status, chunks) in rows {
        println!("{id}\t{name}\t{file_type}\t{status}\t{chunks}");
    }
    Ok(())
}

fn cmd_stats(connection: &rusqlite::Connection, path: &Path) -> anyhow::Result<()> {
    let documents: i64 =
        connection.query_row("SELECT count(*) FROM documents", [], |row| row.get(0))?;
    let chunks: i64 = connection.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))?;
    let index_bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let data_dir = path
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    println!("文档数: {documents}");
    println!("分块数: {chunks}");
    println!("索引文件大小: {index_bytes} 字节");
    println!("数据目录: {data_dir}");
    Ok(())
}

fn cmd_schema(connection: &rusqlite::Connection, _path: &Path) -> anyhow::Result<()> {
    let mut stmt =
        connection.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for sql in rows {
        println!("{sql};\n");
    }
    Ok(())
}

/// import:整库导入走 run_import(GUI 同一条流水线),逐文件打印结果。
fn cmd_import(db_path: &Path, inputs: Vec<PathBuf>, force: bool) -> anyhow::Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let counts = import::run_import(
        db_path,
        inputs,
        ImportOptions {
            force,
            ..ImportOptions::default()
        },
        cancel,
        &mut |event| match event {
            ImportEvent::Scanned { total } => {
                println!("发现 {total} 个文件");
            }
            ImportEvent::FileDone {
                path,
                outcome,
                counts,
            } => match outcome {
                FileOutcome::Ok { .. } => {
                    println!(
                        "成功 {}({}/{})",
                        path.display(),
                        counts.processed,
                        counts.total
                    );
                }
                FileOutcome::Failed { code, message } => {
                    println!("失败 {}: {message} [{code}]", path.display());
                }
                FileOutcome::Skipped => {
                    println!("跳过 {}(未变更)", path.display());
                }
            },
            ImportEvent::Finished { .. } => {}
        },
    )?;
    println!(
        "导入完成:成功 {}、失败 {}、跳过 {}(共 {})",
        counts.ok, counts.failed, counts.skipped, counts.total
    );
    Ok(())
}

/// parse:打印解析出的 Markdown;失败打印中文原因与错误码,退出码 1。
fn cmd_parse(path: &Path) -> anyhow::Result<()> {
    match parse::parse_file(path, 100 * 1024 * 1024) {
        Ok((parsed, _)) => {
            for warning in &parsed.warnings {
                eprintln!("警告: {warning}");
            }
            print!("{}", parsed.markdown);
            Ok(())
        }
        Err(error) => {
            eprintln!("解析失败 [{}]: {}", error.code.as_str(), error);
            std::process::exit(1);
        }
    }
}

/// text:打印 plain_text 与分块边界摘要(索引的文本形态与切分方式)。
fn cmd_text(path: &Path) -> anyhow::Result<()> {
    match parse::parse_file(path, 100 * 1024 * 1024) {
        Ok((parsed, _)) => {
            let plain = text::markdown_to_plain(&parsed.markdown);
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_owned());
            let title = text::extract_title(&plain, &stem);
            let chunks = chunk::chunk_document(&title, &plain);
            println!("=== plain_text ===");
            println!("{}", plain.text);
            println!("=== 分块 {} 个 ===", chunks.len());
            for c in &chunks {
                println!("#{} [{}..{}] {}", c.index, c.start, c.end, c.context_header);
            }
            Ok(())
        }
        Err(error) => {
            eprintln!("解析失败 [{}]: {}", error.code.as_str(), error);
            std::process::exit(1);
        }
    }
}
