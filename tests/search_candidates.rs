use rsou_lib::chunk::Chunk;
use rsou_lib::parse::FileType;
use rsou_lib::repo::{self, FileMeta, ParsedDocument};
use rsou_lib::search::{self, Filters, SearchRequest};
use rsou_lib::text::PlainText;
use rusqlite::Connection;

fn save(conn: &mut Connection, path: &str, title: &str, pieces: &[&str]) -> i64 {
    let mut text = String::new();
    let mut chunks = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        let start = text.len();
        text.push_str(piece);
        chunks.push(Chunk {
            index,
            start,
            end: text.len(),
            context_header: format!("{title} › 第 {index} 节"),
        });
    }
    let meta = FileMeta {
        path: path.into(),
        canonical_path: path.into(),
        file_name: path.rsplit('/').next().unwrap().into(),
        ext: "txt".into(),
        file_type: FileType::Text,
        file_size: text.len() as u64,
        file_mtime_ms: 1_000,
        source_root: None,
    };
    let parsed = ParsedDocument {
        title: title.into(),
        markdown: text.clone(),
        plain: PlainText {
            text,
            blocks: Vec::new(),
        },
        chunks,
        warnings: Vec::new(),
        parser_name: "text",
        parser_version: "text.v1",
    };
    repo::save_parsed(conn, &meta, "hash", &parsed, 1_000).unwrap()
}

fn request() -> SearchRequest {
    SearchRequest {
        query: "文档".into(),
        ..SearchRequest::default()
    }
}

fn old_ranking(conn: &Connection) -> Vec<(i64, f64)> {
    conn.prepare(
        "SELECT rowid, bm25(documents_fts, 5.0, 1.0) AS score \
         FROM documents_fts WHERE documents_fts MATCH '\"文档\"' \
         ORDER BY score, rowid",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn fts_total_includes_punctuation_false_positives() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    save(&mut conn, "/d/false.txt", "说明", &["文、档"]);
    let valid = save(&mut conn, "/d/valid.txt", "说明", &["文 档"]);
    let response = search::search(&conn, &request()).unwrap();
    assert_eq!(response.total_documents, 2);
    assert_eq!(response.documents.len(), 1);
    assert_eq!(response.documents[0].document.id, valid);
    assert_eq!(response.total_hits, 1);
}

#[test]
fn false_positives_in_first_200_candidates_do_not_trigger_backfill() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    for index in 0..205 {
        save(&mut conn, &format!("/d/{index}.txt"), "说明", &["文、档"]);
    }
    let long_body = format!("{}文档", "无关内容 ".repeat(500));
    let valid = save(&mut conn, "/d/last.txt", "说明", &[&long_body]);
    let ranking = old_ranking(&conn);
    assert_eq!(ranking.last().unwrap().0, valid);
    assert!(ranking[199].1 < ranking.last().unwrap().1);

    // 将候选外正文替换为读取即报错的表达式,确保不仅是不展示,也没有读取它。
    conn.execute_batch(&format!(
        "ALTER TABLE document_contents RENAME TO stored_contents;
         CREATE VIEW document_contents AS
         SELECT document_id,
                CASE WHEN document_id = {valid} THEN abs(-9223372036854775808)
                     ELSE plain_text END AS plain_text
         FROM stored_contents;"
    ))
    .unwrap();
    assert!(
        conn.query_row(
            "SELECT plain_text FROM document_contents WHERE document_id = ?1",
            [valid],
            |row| row.get::<_, String>(0),
        )
        .is_err()
    );

    let response = search::search(
        &conn,
        &SearchRequest {
            max_documents: 500,
            ..request()
        },
    )
    .unwrap();
    assert_eq!(response.total_documents, 206);
    assert!(response.documents.is_empty(), "不得读取后续候选补满结果");
    assert_eq!(response.total_hits, 0);
}

#[test]
fn default_returns_100_and_larger_requests_still_stop_at_200_candidates() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    for index in 0..230 {
        save(&mut conn, &format!("/d/{index}.txt"), "说明", &["文档"]);
    }
    for (max_documents, expected) in [(100, 100), (500, 200), (0, 0)] {
        let response = search::search(
            &conn,
            &SearchRequest {
                max_documents,
                ..request()
            },
        )
        .unwrap();
        assert_eq!(response.total_documents, 230);
        assert_eq!(response.documents.len(), expected);
        assert_eq!(response.total_hits, expected);
    }
    assert_eq!(SearchRequest::default().max_documents, 100);
}

#[test]
fn each_structural_filter_is_applied_before_counting_and_candidate_limit() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    // 每组单独足以占满前 200 名,且只违反一个过滤条件。
    for group in 0..4 {
        for index in 0..201 {
            let directory = if group == 0 { "/elsewhere" } else { "/docs_%" };
            let id = save(
                &mut conn,
                &format!("{directory}/{group}-{index}.txt"),
                "说明",
                &["文档"],
            );
            let file_type = if group == 1 { "pdf" } else { "text" };
            let mtime = match group {
                2 => 999,
                3 => 2_001,
                _ => 1_000,
            };
            conn.execute(
                "UPDATE documents SET file_type = ?1, file_mtime_ms = ?2 WHERE id = ?3",
                rusqlite::params![file_type, mtime, id],
            )
            .unwrap();
        }
    }
    let long_body = format!("{}文档", "无关内容 ".repeat(500));
    let valid = save(&mut conn, "/docs_%/valid.txt", "说明", &[&long_body]);
    assert_eq!(old_ranking(&conn).last().unwrap().0, valid);
    let response = search::search(
        &conn,
        &SearchRequest {
            filters: Filters {
                file_types: vec!["text".into()],
                mtime_from_ms: Some(1_000),
                mtime_to_ms: Some(2_000),
                path_prefix: Some("/docs_%/".into()),
            },
            ..request()
        },
    )
    .unwrap();
    assert_eq!(response.total_documents, 1);
    assert_eq!(response.documents.len(), 1);
    assert_eq!(response.documents[0].document.id, valid);
}

#[test]
fn weighted_rank_and_title_body_byte_offsets_are_preserved() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    let title_only = save(&mut conn, "/d/title.txt", "文档", &["只有正文"]);
    let body = save(
        &mut conn,
        "/d/body.txt",
        "说明",
        &["序言🙂", "甲文", "档乙", "后文档末"],
    );
    save(
        &mut conn,
        "/d/long.txt",
        "说明",
        &[&format!("{}文档", "余文 ".repeat(100))],
    );
    let ranking = old_ranking(&conn);
    let response = search::search(&conn, &request()).unwrap();
    assert_eq!(response.documents.len(), ranking.len());
    for (document, (id, rank)) in response.documents.iter().zip(ranking) {
        assert_eq!(document.document.id, id);
        assert!((document.best_rank - rank).abs() < 1e-15);
    }
    let title = response
        .documents
        .iter()
        .find(|doc| doc.document.id == title_only)
        .unwrap();
    assert!(title.hits.is_empty());
    assert_eq!(title.title_highlights, [search::Span { start: 0, end: 6 }]);
    let document = response
        .documents
        .iter()
        .find(|doc| doc.document.id == body)
        .unwrap();
    assert!(document.title_highlights.is_empty());
    assert_eq!(document.hits.len(), 2);
    let plain = repo::get_plain_text(&conn, body).unwrap().unwrap();
    assert_eq!(document.hits[0].content, "甲文档乙");
    assert_eq!(document.hits[0].start_offset, "序言🙂".len());
    for hit in &document.hits {
        assert_eq!(&plain[hit.start_offset..hit.end_offset], hit.content);
        for span in &hit.highlights {
            assert_eq!(&hit.content[span.start..span.end], "文档");
            assert_eq!(
                &plain[hit.start_offset + span.start..hit.start_offset + span.end],
                "文档"
            );
        }
    }
}

fn set_hash(conn: &Connection, id: i64, hash: &str) {
    conn.execute(
        "UPDATE documents SET content_hash = ?1 WHERE id = ?2",
        (hash, id),
    )
    .unwrap();
}

#[test]
fn duplicate_locations_do_not_consume_candidate_or_result_slots() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    let hash = "a".repeat(64);
    for index in 0..205 {
        let id = save(
            &mut conn,
            &format!("/copies/{index}.txt"),
            "说明",
            &["文档"],
        );
        set_hash(&conn, id, &hash);
    }
    let unique = save(
        &mut conn,
        "/unique.txt",
        "说明",
        &[&format!("{}文档", "余文 ".repeat(500))],
    );
    let response = search::search(
        &conn,
        &SearchRequest {
            max_documents: 2,
            ..request()
        },
    )
    .unwrap();
    assert_eq!(response.total_documents, 206);
    assert_eq!(response.total_groups, 2);
    assert_eq!(response.representatives().count(), 2);
    assert_eq!(response.documents.len(), 206);
    assert_eq!(
        response.locations(response.documents[0].group_id).count(),
        205
    );
    assert!(
        response
            .representatives()
            .any(|hit| hit.document.id == unique)
    );
    assert_eq!(response.total_hits, 2, "副本不累加展示命中数");

    let limited = search::search(
        &conn,
        &SearchRequest {
            max_documents: 1,
            ..request()
        },
    )
    .unwrap();
    assert_eq!(limited.representatives().count(), 1);
    assert_eq!(limited.documents.len(), 205, "结果限制不能截掉组内位置");
}

#[test]
fn grouping_preserves_each_locations_title_and_offsets_and_respects_scope() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    let title = save(&mut conn, "/outside/title.txt", "文档", &["前言只有正文"]);
    let body = save(
        &mut conn,
        "/inside/body.txt",
        "说明",
        &["序言🙂", "文档正文"],
    );
    // 相同字节的文件可因文件名、扩展名或解析版本产生不同的标题与解析结果。
    for id in [title, body] {
        set_hash(&conn, id, &"b".repeat(64));
    }
    let response = search::search(&conn, &request()).unwrap();
    assert_eq!(response.total_groups, 1);
    assert_eq!(response.documents.len(), 2);
    let representative = response.representatives().next().unwrap();
    assert_eq!(representative.document.id, title);
    assert!(!representative.title_highlights.is_empty());
    let copy = response
        .locations(representative.group_id)
        .find(|hit| hit.document.id == body)
        .unwrap();
    assert!(copy.title_highlights.is_empty());
    assert_eq!(copy.hits[0].start_offset, "序言🙂".len());

    let scoped = search::search(
        &conn,
        &SearchRequest {
            filters: Filters {
                path_prefix: Some("/inside/".into()),
                ..Filters::default()
            },
            ..request()
        },
    )
    .unwrap();
    assert_eq!(scoped.total_documents, 1);
    assert_eq!(scoped.total_groups, 1);
    assert_eq!(scoped.documents.len(), 1);
    assert_eq!(scoped.documents[0].document.id, body);

    repo::delete_document(&mut conn, title).unwrap();
    let remaining = search::search(&conn, &request()).unwrap();
    assert_eq!(remaining.documents.len(), 1);
    assert_eq!(remaining.documents[0].group_id, body);
}

#[test]
fn invalid_hashes_are_not_merged_and_different_hashes_keep_separate_results() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    for (index, hash) in [String::new(), String::new(), "a".repeat(64), "b".repeat(64)]
        .iter()
        .enumerate()
    {
        let id = save(&mut conn, &format!("/{index}.txt"), "说明", &["文档"]);
        set_hash(&conn, id, hash);
    }
    let response = search::search(&conn, &request()).unwrap();
    assert_eq!(response.total_groups, 4);
    assert_eq!(response.representatives().count(), 4);
}

#[test]
fn a_false_positive_copy_does_not_hide_a_valid_location() {
    let mut conn = rsou_lib::store::open_in_memory().unwrap();
    let false_id = save(&mut conn, "/false.txt", "文、档", &["无关"]);
    let valid = save(&mut conn, "/valid.txt", "说明", &["文档正文"]);
    for id in [false_id, valid] {
        set_hash(&conn, id, &"c".repeat(64));
    }
    let response = search::search(&conn, &request()).unwrap();
    assert_eq!(response.total_groups, 1);
    assert_eq!(response.documents.len(), 1);
    assert_eq!(response.documents[0].group_id, valid);
}
