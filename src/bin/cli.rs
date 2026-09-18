//! rsou 命令行工具:在无桌面环境(CI / WSL 无 X)下直接验证索引库。
//!
//! 用法:`rsou-cli [--db PATH] <docs|stats|schema>`
//! 所有子命令都经 `store::open` 打开库——这同时是「检索必须注册 tokenizer」
//! 这条约束的活文档:用系统 sqlite3 CLI 对库做 MATCH 会因找不到 tokenizer 报错。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rsou_lib::store::{self, OpenMode};

const USAGE: &str = "用法: rsou-cli [--db PATH] <命令>

命令:
  docs     打印 documents 表(id/文件名/类型/状态/分块数)
  stats    打印文档数、分块数、索引文件大小与数据目录
  schema   打印 sqlite_master 中的建表语句(验证 FTS 表已建)

选项:
  --db PATH   索引库路径,缺省用应用数据目录下的 index.sqlite3";

fn main() -> ExitCode {
    let mut db_path: Option<PathBuf> = None;
    let mut command: Option<String> = None;
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
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if command.is_none() && !arg.starts_with('-') => command = Some(arg),
            _ => {
                eprintln!("无法识别的参数: {arg}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(command) = command else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let db_path = db_path.unwrap_or_else(|| store::data_dirs().db_path);

    let result = match command.as_str() {
        "docs" => run(&db_path, cmd_docs),
        "stats" => run(&db_path, cmd_stats),
        "schema" => run(&db_path, cmd_schema),
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
