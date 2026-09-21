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

use rsou_lib::dict;
use rsou_lib::import::{self, FileOutcome, ImportEvent, ImportOptions};
use rsou_lib::maintain;
use rsou_lib::query::Scope;
use rsou_lib::search::{self, Filters, SearchRequest, Span};
use rsou_lib::store::{self, OpenMode};
use rsou_lib::{chunk, parse, text};

const USAGE: &str = "用法: rsou-cli [--db PATH] <命令> [参数]

命令:
  docs                  打印 documents 表(id/文件名/类型/状态/分块数)
  stats                 打印文档数、分块数、索引文件大小与数据目录
  schema                打印 sqlite_master 中的建表语句(验证 FTS 表已建)
  import <路径...>      导入文件或目录(目录递归);--force 全部重解析
  parse <文件>          解析单个文件,把 Markdown 打到 stdout
  text <文件>           解析单个文件,打印 plain_text 与分块边界摘要
  search <查询>         全文检索,片段用【】标出高亮段
  check                 完整性检查;索引不一致时退出码 1
  rebuild               重建全文索引(打印进度与行数)
  optimize              FTS optimize + WAL 截断 + VACUUM
  purge-markdown        清空已入库的 Markdown 原文并压缩索引文件
  clear                 清空资料库(需 --yes;保留 settings)

选项:
  --db PATH   索引库路径,缺省用应用数据目录下的 index.sqlite3
  --force     import:忽略 hash/mtime 跳过,全部重解析
  --loose     search:宽松模式(jieba 切词;无该 feature 时等同精确)
  --scope S   search:检索范围 all|title|content(缺省 all)
  --type T    search:限定类型,逗号分隔(如 word,pdf 或扩展名)
  --limit N   search:最多返回 N 组内容(缺省 100,仅取排名前 200 组,保留组内文件位置)
  --yes       clear:确认清空(不可恢复)";

fn main() -> ExitCode {
    let mut db_path: Option<PathBuf> = None;
    let mut force = false;
    let mut loose = false;
    let mut yes = false;
    let mut scope_arg: Option<String> = None;
    let mut type_arg: Option<String> = None;
    let mut limit_arg: Option<String> = None;
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
            "--loose" => loose = true,
            "--yes" => yes = true,
            "--scope" => match args.next() {
                Some(value) => scope_arg = Some(value),
                None => {
                    eprintln!("--scope 需要 all|title|content\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--type" => match args.next() {
                Some(value) => type_arg = Some(value),
                None => {
                    eprintln!("--type 需要类型列表,如 word,pdf\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--limit" => match args.next() {
                Some(value) => limit_arg = Some(value),
                None => {
                    eprintln!("--limit 需要一个数字\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
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
        "search" => match positional.get(1) {
            Some(query) => cmd_search(&db_path, query, loose, scope_arg, type_arg, limit_arg),
            None => {
                eprintln!("search 需要一个查询词\n{USAGE}");
                return ExitCode::from(2);
            }
        },
        "check" => run(&db_path, cmd_check),
        "rebuild" => cmd_rebuild(&db_path),
        "optimize" => run(&db_path, cmd_optimize),
        "purge-markdown" => run(&db_path, cmd_purge_markdown),
        "clear" => {
            if !yes {
                eprintln!("clear 会删除全部文档与索引,请加 --yes 确认\n{USAGE}");
                return ExitCode::from(2);
            }
            cmd_clear(&db_path)
        }
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
    warn_if_rebuild_pending(&connection);
    body(&connection, path)
}

/// 旧库迁移后索引待重建时给一句中文提示(不阻塞命令本身)。
fn warn_if_rebuild_pending(connection: &rusqlite::Connection) {
    if let Ok(true) = maintain::needs_fts_rebuild(connection) {
        eprintln!(
            "提示: 全文索引待重建，请对当前数据库运行 rsou-cli --db <数据库路径> rebuild 后再检索"
        );
    }
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
    let stats = maintain::index_stats(connection, path)?;
    let data_dir = path
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    println!("文档数: {}", stats.documents);
    println!("已索引: {}", stats.parsed);
    println!("失败: {}", stats.failed);
    println!("分块数: {}", stats.chunks);
    println!("FTS 行数: {}", stats.fts_rows);
    println!("原文字节: {}", stats.text_bytes);
    println!("索引文件大小: {} 字节", stats.db_bytes);
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
    let connection = store::open(db_path, OpenMode::ReadOnly)?;
    warn_if_rebuild_pending(&connection);
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

/// 加载词典并报告格式问题。
///
/// 数据目录按库文件所在目录反推:`--db` 未指定时那本就是应用数据目录,
/// 指定时词典跟在库旁边,与界面里把两个文件放在 `data_dir` 的约定一致。
fn load_dict(db_path: &Path) {
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let report = dict::load(dir);
    for file in [&report.user_words, &report.synonyms] {
        for problem in &file.problems {
            eprintln!("警告: 词典 {} {}", file.path.display(), problem);
        }
    }
}

/// search:只读连接跑 search::search,每篇打印标题/路径/命中数与【】高亮片段。
fn cmd_search(
    db_path: &Path,
    query: &str,
    loose: bool,
    scope_arg: Option<String>,
    type_arg: Option<String>,
    limit_arg: Option<String>,
) -> anyhow::Result<()> {
    let scope = match scope_arg.as_deref() {
        None | Some("all") => Scope::All,
        Some("title") => Scope::Title,
        Some("content") => Scope::Content,
        Some(other) => anyhow::bail!("未知的检索范围: {other}(可用 all|title|content)"),
    };
    let file_types = type_arg
        .map(|arg| {
            arg.split(',')
                .map(|token| {
                    file_type_of_token(token.trim())
                        .ok_or_else(|| anyhow::anyhow!("未知的文件类型: {}", token.trim()))
                })
                .collect::<anyhow::Result<Vec<String>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let max_documents = limit_arg
        .map(|arg| {
            arg.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("--limit 需要一个正整数: {arg}"))
        })
        .transpose()?
        .unwrap_or(100);

    let conn = store::open(db_path, OpenMode::ReadOnly)?;
    if maintain::needs_fts_rebuild(&conn)? {
        anyhow::bail!(
            "全文索引待重建，暂不能检索；请对当前数据库运行 rsou-cli --db <数据库路径> rebuild"
        );
    }
    // 词典与 GUI 共用一套加载路径:不加载的话同一个查询在两边会给出不同结果。
    load_dict(db_path);
    let response = search::search(
        &conn,
        &SearchRequest {
            query: query.to_owned(),
            scope,
            loose,
            filters: Filters {
                file_types,
                ..Filters::default()
            },
            max_documents,
            max_fragments_per_document: search::DEFAULT_MAX_FRAGMENTS,
        },
    )?;
    for doc_hit in response.representatives() {
        let summary = if doc_hit.total_hits > doc_hit.hits.len() {
            format!(
                "共 {} 个命中片段，展示前 {} 个",
                doc_hit.total_hits,
                doc_hit.hits.len()
            )
        } else if doc_hit.total_hits > 0 {
            format!("{} 个命中片段", doc_hit.total_hits)
        } else if !doc_hit.title_highlights.is_empty() {
            "标题命中".to_owned()
        } else {
            "未定位到展示片段".to_owned()
        };
        println!(
            "{} | {} | {}",
            doc_hit.document.file_name, doc_hit.document.path, summary
        );
        for location in response.locations(doc_hit.group_id).skip(1) {
            println!("  相同内容位置: {}", location.document.path);
        }
        for hit in &doc_hit.hits {
            if !hit.context_header.is_empty() {
                println!("  〔{}〕", hit.context_header);
            }
            println!("  {}", mark_snippet(&hit.content, &hit.highlights));
        }
    }
    // 被 max_documents 截断时说明一下,避免把下界当成全量。
    let truncated = response.total_documents > response.documents.len();
    println!(
        "展示 {} 组 · {} 个文件位置 · 代表文档共 {} 个命中片段 · 耗时 {:.0} ms{}",
        response.representatives().count(),
        response.documents.len(),
        response.total_hits,
        response.elapsed_ms,
        if truncated {
            "(仅展示部分 FTS 命中文档)"
        } else {
            ""
        }
    );
    Ok(())
}

/// 类型过滤 token:file_type 名(word/pdf/…)或扩展名(docx/txt/…)。
fn file_type_of_token(token: &str) -> Option<String> {
    const NAMES: [&str; 6] = ["word", "excel", "ppt", "pdf", "text", "epub"];
    if NAMES.contains(&token) {
        return Some(token.to_owned());
    }
    let probe = format!("f.{token}");
    parse::file_type_of(Path::new(&probe)).map(|ft| ft.as_str().to_owned())
}

/// 把高亮区间包上【】输出(终端里没有底色可用)。
fn mark_snippet(content: &str, spans: &[Span]) -> String {
    let mut out = String::with_capacity(content.len() + spans.len() * 4);
    let mut pos = 0usize;
    for span in spans {
        if span.start < pos || span.end > content.len() {
            continue;
        }
        out.push_str(&content[pos..span.start]);
        out.push('【');
        out.push_str(&content[span.start..span.end]);
        out.push('】');
        pos = span.end;
    }
    out.push_str(&content[pos..]);
    out
}

/// check:打印完整性报告;不一致时返回 Err(退出码 1)。
fn cmd_check(connection: &rusqlite::Connection, _path: &Path) -> anyhow::Result<()> {
    let report = maintain::check_integrity(connection)?;
    println!("{}", report.summary());
    if maintain::needs_fts_rebuild(connection)? {
        anyhow::bail!("全文索引待重建");
    }
    if !report.is_consistent() {
        anyhow::bail!("索引不一致");
    }
    Ok(())
}

/// rebuild:全量重建 documents_fts,打印进度与最终行数。
fn cmd_rebuild(db_path: &Path) -> anyhow::Result<()> {
    let mut conn = store::open(db_path, OpenMode::ReadWrite)?;
    let rows = maintain::rebuild_fts(&mut conn, &mut |done, total| {
        println!("重建进度: {done}/{total}");
    })?;
    println!("全文索引已重建: {rows} 条");
    Ok(())
}

/// optimize:FTS optimize + WAL 截断 + VACUUM。
fn cmd_optimize(connection: &rusqlite::Connection, _path: &Path) -> anyhow::Result<()> {
    maintain::optimize(connection)?;
    println!("索引已优化");
    Ok(())
}

/// purge-markdown:清掉存量 Markdown 原文,随后 optimize 回收体积。
fn cmd_purge_markdown(connection: &rusqlite::Connection, _path: &Path) -> anyhow::Result<()> {
    let cleared = maintain::purge_stored_markdown(connection)?;
    if let Err(error) = maintain::optimize(connection) {
        anyhow::bail!("已清理 {cleared} 篇文档的 Markdown 原文，但压缩索引文件失败: {error:#}");
    }
    println!("已清理 {cleared} 篇文档的 Markdown 原文并压缩索引文件");
    Ok(())
}

/// clear:清空文档与索引(调用方已校验 --yes)。
fn cmd_clear(db_path: &Path) -> anyhow::Result<()> {
    let mut conn = store::open(db_path, OpenMode::ReadWrite)?;
    maintain::clear_all(&mut conn)?;
    println!("资料库已清空");
    Ok(())
}
