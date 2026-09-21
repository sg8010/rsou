//! 合成语料性能探针。比较两个版本时将此文件复制到基线的 tests/ 下，
//! 分别 release 编译，再交替执行测试二进制取中位数；编译时间不计入。
//! token_ms 包含 token_spans 的词元物化开销，不能视为纯流式分词耗时。
use rsou_lib::{maintain, search, store, tokenize};
use std::{hint::black_box, time::Instant};
#[test]
#[ignore = "手动性能测试：cargo test -p rsou --release --test tokenizer_benchmark -- --ignored --nocapture"]
fn tokenizer_corpus_benchmark() {
    for (label, phrase, query) in [
        ("中文", "合同资料管理正文检索归档审批流程 ", "合同"),
        (
            "英文",
            "document search archive approval process ",
            "document",
        ),
        ("编号", "A4 GB2024 Win7 AB1234 ZX9876 ", "GB2024"),
        (
            "混合",
            "合同 document A4打印纸 GB2024标准 Win7系统 ",
            "Win7",
        ),
    ] {
        let body = phrase.repeat(80);
        let now = Instant::now();
        for _ in 0..3000 {
            black_box(tokenize::token_spans(black_box(&body)));
        }
        let token_ms = now.elapsed().as_secs_f64() * 1000.;
        let mut conn = store::open_in_memory().unwrap();
        let tx = conn.transaction().unwrap();
        for id in 1..=1000i64 {
            tx.execute("INSERT INTO documents(id,path,canonical_path,file_name,title,ext,file_type,file_size,file_mtime_ms,content_hash,parse_status,created_at,updated_at) VALUES(?1,?2,?2,?2,'资料','txt','text',1,1,?2,'parsed',1,1)", (id,format!("/doc{id}"))).unwrap();
            tx.execute(
                "INSERT INTO document_contents(document_id,markdown,plain_text) VALUES(?1,?2,?2)",
                (id, &body),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let now = Instant::now();
        maintain::rebuild_fts(&mut conn, &mut |_, _| {}).unwrap();
        let build_ms = now.elapsed().as_secs_f64() * 1000.;
        let size: i64 = conn
            .query_row(
                "SELECT sum(length(block)) FROM documents_fts_data",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let req = search::SearchRequest {
            query: query.into(),
            ..Default::default()
        };
        let response = search::search_for_listing(&conn, &req).unwrap();
        let now = Instant::now();
        for _ in 0..20 {
            black_box(search::search_for_listing(&conn, &req).unwrap());
        }
        let search_ms = now.elapsed().as_secs_f64() * 1000. / 20.;
        println!(
            "{label},{token_ms:.3},{build_ms:.3},{size},{search_ms:.3},{}",
            response.total_documents
        );
    }
}
