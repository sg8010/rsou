//! CLI 在迁移待重建期间不返回不完整检索结果，导入完成后保留重建提示。

mod common;

use std::path::Path;
use std::process::{Command, Output};

use rsou_lib::repo;
use rsou_lib::store::{self, OpenMode};

fn cli(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rsou-cli"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("运行 CLI")
}

fn mark_pending(db: &Path) {
    let conn = store::open(db, OpenMode::ReadWrite).unwrap();
    repo::set_setting(&conn, "fts_rebuild_pending", "1").unwrap();
}

#[test]
fn pending_search_fails_until_rebuild_completes() {
    let dir = common::temp_dir("cli-pending-search");
    let db = dir.join("index.sqlite3");
    let input = dir.join("sample.txt");
    std::fs::write(&input, "索引恢复后可以检索的独有词").unwrap();
    let imported = cli(&db, &["import", input.to_str().unwrap()]);
    assert!(imported.status.success(), "{imported:?}");
    mark_pending(&db);

    let blocked = cli(&db, &["search", "独有词"]);
    assert!(!blocked.status.success());
    assert!(blocked.stdout.is_empty(), "不得输出正常检索结果");
    let error = String::from_utf8(blocked.stderr).unwrap();
    assert!(error.contains("全文索引待重建"), "{error}");
    assert!(error.contains("rebuild"), "{error}");

    let rebuilt = cli(&db, &["rebuild"]);
    assert!(rebuilt.status.success(), "{rebuilt:?}");
    let found = cli(&db, &["search", "独有词"]);
    assert!(found.status.success(), "{found:?}");
    assert!(
        String::from_utf8(found.stdout)
            .unwrap()
            .contains("sample.txt")
    );
}

#[test]
fn import_warns_when_existing_index_still_needs_rebuild() {
    let dir = common::temp_dir("cli-pending-import");
    let db = dir.join("index.sqlite3");
    mark_pending(&db);
    let input = dir.join("new.txt");
    std::fs::write(&input, "本次导入不会补齐旧全文索引").unwrap();

    let imported = cli(&db, &["import", input.to_str().unwrap()]);
    assert!(imported.status.success(), "{imported:?}");
    let output = String::from_utf8(imported.stdout).unwrap();
    assert!(output.contains("导入完成:成功 1"), "{output}");
    let warning = String::from_utf8(imported.stderr).unwrap();
    assert!(warning.contains("全文索引待重建"), "{warning}");
    assert!(warning.contains("rebuild"), "{warning}");
    let conn = store::open(&db, OpenMode::ReadOnly).unwrap();
    assert!(rsou_lib::maintain::needs_fts_rebuild(&conn).unwrap());
}
