use rsou_tokenizer_spike::register;
use rusqlite::{Connection, params};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let document_count = std::env::var("RSOU_BENCH_DOCS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100_000);
    let database_path = std::env::var("RSOU_BENCH_DB").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join(format!("rsou-stage0-scale-{}.sqlite3", std::process::id()))
            .display()
            .to_string()
    });
    let _ = std::fs::remove_file(&database_path);

    let open_started = Instant::now();
    let mut connection = Connection::open(&database_path)?;
    register(&connection)?;
    connection.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         CREATE VIRTUAL TABLE chunks_fts USING fts5(content, tokenize = 'rsou 0');",
    )?;
    let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;

    let insert_started = Instant::now();
    let transaction = connection.transaction()?;
    {
        let mut insert =
            transaction.prepare("INSERT INTO chunks_fts(rowid, content) VALUES (?1, ?2)")?;
        for id in 1..=document_count {
            let content = format!(
                "第{id}份合同包含采购合同条款、付款条件和发票编号 A{id}. 这是用于阶段零基准的中文正文。"
            );
            insert.execute(params![id as i64, content])?;
        }
    }
    transaction.commit()?;
    let insert_ms = insert_started.elapsed().as_secs_f64() * 1000.0;

    let query_started = Instant::now();
    let hit_count: i64 = connection.query_row(
        "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
        ["\"合同\""],
        |row| row.get(0),
    )?;
    let mut ranked = connection.prepare(
        "SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT 100",
    )?;
    let ranked_count = ranked
        .query_map(["\"合同\""], |row| row.get::<_, i64>(0))?
        .count();
    let query_ms = query_started.elapsed().as_secs_f64() * 1000.0;

    let index_bytes = std::fs::metadata(&database_path)?.len();
    println!("documents={document_count}");
    println!("open_ms={open_ms:.3}");
    println!("insert_ms={insert_ms:.3}");
    println!("query_ms={query_ms:.3}");
    println!("hit_count={hit_count}");
    println!("ranked_rows={ranked_count}");
    println!("index_bytes={index_bytes}");
    println!("database={database_path}");

    Ok(())
}
