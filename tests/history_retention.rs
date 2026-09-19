mod common;

use rsou_lib::{repo, store};
use store::OpenMode;

#[test]
fn writable_open_prunes_old_history_but_readonly_open_does_not() {
    let path = common::temp_dir("history-retention").join("index.sqlite3");
    let old_run;
    let running;
    {
        let conn = store::open(&path, OpenMode::ReadWrite).unwrap();
        let old_time = repo::now_ms() - 91 * 86_400_000;
        old_run = repo::create_run(&conn, "files", None, 1, old_time).unwrap();
        repo::upsert_item(
            &conn,
            old_run,
            std::path::Path::new("/old.txt"),
            "skipped",
            None,
            None,
            None,
            old_time,
        )
        .unwrap();
        repo::finish_run(
            &conn,
            old_run,
            "done",
            &repo::ImportCounts::default(),
            None,
            old_time,
        )
        .unwrap();
        running = repo::create_run(&conn, "files", None, 0, old_time).unwrap();
    }
    {
        let conn = store::open(&path, OpenMode::ReadOnly).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM import_runs WHERE id = ?1)",
                [old_run],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "只读查询不应删除历史");
    }
    for _ in 0..2 {
        let conn = store::open(&path, OpenMode::ReadWrite).unwrap();
        let runs: Vec<i64> = conn
            .prepare("SELECT id FROM import_runs ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(runs, [running]);
        let items: i64 = conn
            .query_row("SELECT count(*) FROM import_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(items, 0, "过期任务明细应级联清理");
    }
}
