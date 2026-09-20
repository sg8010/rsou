//! 入库路径的一次性归一化:去掉多余的 Windows `\\?\` 前缀。
//!
//! 早期版本用 `std::fs::canonicalize`,它在本机 Windows 上返回
//! `\\?\C:\Users\...` 形式(extended-length 前缀)。那串前缀既会显示在检索
//! 结果里,也会让「目录前缀」过滤匹配不上——用户不会去敲 `\\?\`。
//!
//! 现在 `import::scan_paths` 与 `repo::FileMeta::of` 都改用 `dunce::canonicalize`,
//! 新导入的路径已经是干净的;本模块负责把**已有行**一并归一化。
//!
//! 三条必须守住的约束:
//!
//! 1. **两列一起改**。跳过判定用 `canonical_path` 比对
//!    (`import::existing_by_canonical`),显示与目录过滤用 `path`;只改一列会
//!    让同一文件在下次导入时被当成两个文件。
//! 2. **不能让 `path` 撞 UNIQUE**。`scan_paths` 里 canonicalize 失败会退回
//!    `absolute()`,所以同一文件确实可能已以 `C:\...` 与 `\\?\C:\...` 两种拼写
//!    各存一行。归一化后它们会变成同一个值,直接 UPDATE 会触发
//!    `documents.path` 的 UNIQUE 约束、让整个迁移失败。这时保留先到的那行,
//!    把后到的重复行删掉(连带 FTS)。
//! 3. **只碰数据库、不碰文件系统**。迁移发生在 `store::open` 里,那时库里记的
//!    文件可能早被删/移走;`dunce::simplified` 恰好是纯字符串操作,满足这一点。
//!
//! 剥离用 `dunce` 自己的安全规则而不是手剔前缀:盘符路径、无 `.`/`..`、每段
//! 是合法文件名、非 DOS 保留名、长度 < 260 时才剥,否则原样保留。

use anyhow::Context;
use rusqlite::Connection;

/// 一次迁移里对一篇文档的动作。
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// 改写两列。
    Rewrite {
        id: i64,
        path: String,
        canonical_path: String,
    },
    /// 归一化后与另一行重复:删掉这一行(连带它的 FTS 行)。
    DropDuplicate { id: i64 },
}

/// 执行归一化(幂等)。返回被改写/删除的行数。
pub fn normalize(connection: &Connection) -> anyhow::Result<usize> {
    normalize_with(connection, simplify)
}

/// 把库里全部路径读出来,算出要做的改动;不改库。
///
/// 拆成纯函数是为了能在 Linux CI 上测:真正的剥离在非 Windows 平台是直通,
/// 但这个「算改动」的过程——两列口径、UNIQUE 冲突、幂等——与平台无关,
/// 也正是容易写错的地方。
fn plan(changes: Vec<(i64, String, String)>, simplify: fn(&str) -> String) -> Vec<Change> {
    let mut plan: Vec<Change> = Vec::new();
    // 归一化后的 path → 已经保留的那一行 id(用于发现重复)。
    // **每一行都要登记**,包括本来就干净、不需要改写的行:否则「一行已干净 +
    // 一行待剥离且剥完与之同名」这种撞 UNIQUE 的情形会被漏掉(Rewrite 会直接
    // 撞 `documents.path` 的 UNIQUE 约束,让整个迁移失败)。
    let mut kept: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    for (id, path, canonical_path) in changes {
        let new_path = simplify(&path);
        let new_canonical = simplify(&canonical_path);
        match kept.get(&new_path) {
            // 归一化后与前面某行撞 UNIQUE:删掉这一行(即使它本来“不需要改”)。
            Some(existing) => {
                log::warn!(
                    "路径归一化后发现重复文档(id {id} 与 {existing} 同为 {new_path}),保留先到的"
                );
                plan.push(Change::DropDuplicate { id });
            }
            None => {
                kept.insert(new_path.clone(), id);
                // 只在真的变了才记改动,保证幂等(每次打开库都会跑一次)。
                if new_path != path || new_canonical != canonical_path {
                    plan.push(Change::Rewrite {
                        id,
                        path: new_path,
                        canonical_path: new_canonical,
                    });
                }
            }
        }
    }
    plan
}

fn normalize_with(connection: &Connection, simplify: fn(&str) -> String) -> anyhow::Result<usize> {
    // 先收集(改的时候不能还在读同一个语句的游标)。
    let mut rows: Vec<(i64, String, String)> = Vec::new();
    {
        let mut stmt = connection
            .prepare("SELECT id, path, canonical_path FROM documents ORDER BY id")
            .context("读取文档路径失败")?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("读取文档路径失败")?;
        for row in mapped {
            rows.push(row.context("读取文档路径失败")?);
        }
    }

    let plan = plan(rows, simplify);
    if plan.is_empty() {
        return Ok(0);
    }

    apply(connection, &plan)?;
    Ok(plan.len())
}

/// 一个事务做完：要么全成，要么全不动。
///
/// 不能只依赖 `Transactional` 的 drop 回滚:这里是在 `store::open` 里对一条已
/// 打开的连接操作，必须自己保证失败时不留半新半旧的库。
fn apply(connection: &Connection, plan: &[Change]) -> anyhow::Result<()> {
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .context("开启路径归一化事务失败")?;
    let result = (|| -> anyhow::Result<()> {
        let mut update = connection
            .prepare("UPDATE documents SET path = ?2, canonical_path = ?3 WHERE id = ?1")?;
        let mut delete_fts = connection.prepare("DELETE FROM documents_fts WHERE rowid = ?1")?;
        let mut delete_doc = connection.prepare("DELETE FROM documents WHERE id = ?1")?;
        for change in plan {
            match change {
                Change::Rewrite {
                    id,
                    path,
                    canonical_path,
                } => {
                    update.execute(rusqlite::params![id, path, canonical_path])?;
                }
                Change::DropDuplicate { id } => {
                    // 与 repo::delete_document 同序;contentless-delete 的 FTS 删除
                    // 按 rowid 清词元,不读旧值,顺序本身不承重。
                    delete_fts.execute([id])?;
                    delete_doc.execute([id])?;
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            connection
                .execute_batch("COMMIT")
                .context("提交路径归一化事务失败")?;
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error).context("归一化文档路径失败")
        }
    }
}

/// 对单个已入库路径字符串做前缀剥离；保持不变时返回原值。
///
/// 传入的是 SQLite 里的 TEXT,一定是合法 UTF-8;`dunce::simplified` 对非
/// Unicode 路径不转换,所以这里用 `to_str` 判断而不是 `to_string_lossy`
/// (后者会把无法还原的路径悄悄改成替换字符)。
fn simplify(stored: &str) -> String {
    let simplified = dunce::simplified(std::path::Path::new(stored));
    match simplified.to_str() {
        Some(s) if s != stored => s.to_owned(),
        _ => stored.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟 Windows 上剥掉 `\\?\` 的效果。
    fn strip_fake(stored: &str) -> String {
        stored.strip_prefix(r"\\?\").unwrap_or(stored).to_owned()
    }

    fn rows(pairs: &[(i64, &str, &str)]) -> Vec<(i64, String, String)> {
        pairs
            .iter()
            .map(|(id, p, c)| (*id, (*p).to_owned(), (*c).to_owned()))
            .collect()
    }

    #[test]
    fn clean_paths_produce_no_changes() {
        let plan = plan(
            rows(&[
                (1, r"C:\docs\a.docx", r"C:\docs\a.docx"),
                (2, "/home/ljw/docs/b.pdf", "/home/ljw/docs/b.pdf"),
            ]),
            strip_fake,
        );
        assert!(plan.is_empty(), "干净路径不应产生改动: {plan:?}");
    }

    #[test]
    fn prefixed_paths_are_rewritten_on_both_columns() {
        let plan = plan(
            rows(&[(1, r"\\?\C:\docs\a.docx", r"\\?\C:\docs\a.docx")]),
            strip_fake,
        );
        assert_eq!(
            plan,
            [Change::Rewrite {
                id: 1,
                path: r"C:\docs\a.docx".to_owned(),
                canonical_path: r"C:\docs\a.docx".to_owned(),
            }]
        );
    }

    #[test]
    fn one_sided_prefix_is_normalized_too() {
        // 最危险的情形:path 已干净、canonical_path 还是旧口径。
        // 跳过判定看 canonical_path,只改一列会让同一文件重复导入。
        let plan = plan(
            rows(&[(1, r"C:\docs\a.docx", r"\\?\C:\docs\a.docx")]),
            strip_fake,
        );
        assert_eq!(
            plan,
            [Change::Rewrite {
                id: 1,
                path: r"C:\docs\a.docx".to_owned(),
                canonical_path: r"C:\docs\a.docx".to_owned(),
            }]
        );
    }

    #[test]
    fn collision_keeps_first_row_and_drops_the_duplicate() {
        // canonicalize 失败时会退回无前缀的 absolute(),所以同一文件可能已以
        // 两种拼写各存一行;归一化后撞 UNIQUE,必须删掉后到的那行而不是让
        // 整个迁移失败(那会让程序再也打不开自己的索引库)。
        let plan = plan(
            rows(&[
                (1, r"C:\docs\a.docx", r"C:\docs\a.docx"),
                (2, r"\\?\C:\docs\a.docx", r"\\?\C:\docs\a.docx"),
            ]),
            strip_fake,
        );
        assert_eq!(plan, [Change::DropDuplicate { id: 2 }], "应保留先到的行");
    }

    #[test]
    fn collision_among_prefixed_rows_keeps_first() {
        let plan = plan(
            rows(&[
                (5, r"\\?\C:\docs\b.docx", r"\\?\C:\docs\b.docx"),
                (9, r"\\?\C:\docs\b.docx", r"\\?\C:\docs\b.docx"),
            ]),
            strip_fake,
        );
        assert_eq!(
            plan,
            [
                Change::Rewrite {
                    id: 5,
                    path: r"C:\docs\b.docx".to_owned(),
                    canonical_path: r"C:\docs\b.docx".to_owned(),
                },
                Change::DropDuplicate { id: 9 },
            ]
        );
    }

    #[test]
    fn re_running_over_a_rewritten_plan_is_a_noop() {
        // 幂等:第二次跑不应再产生任何改动(每次 open 都会跑)。
        let after = rows(&[(1, r"C:\docs\a.docx", r"C:\docs\a.docx")]);
        assert!(plan(after, strip_fake).is_empty());
    }

    /// 往库里插一行完整的文档(含 contents 与 FTS)。
    fn insert_full_doc(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO documents(path, canonical_path, file_name, title, ext, file_type, \
             file_size, file_mtime_ms, content_hash, parse_status, created_at, updated_at) \
             VALUES (?1, ?1, 'f.txt', '标题', 'txt', 'text', 1, 1, 'h', 'parsed', 1, 1)",
            [path],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO document_contents(document_id, markdown, plain_text) VALUES (?1, 'x', '正文')",
            [id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, '标题', '正文')",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn migration_rewrites_and_removes_duplicates_in_the_real_database() {
        // 端到端:走真实连接,验证改写、删重复、以及重复行的 FTS/contents 被级联清掉。
        let conn = crate::store::open_in_memory().unwrap();
        insert_full_doc(&conn, r"C:\docs\a.txt");
        insert_full_doc(&conn, r"\\?\C:\docs\a.txt");
        insert_full_doc(&conn, r"\\?\C:\docs\b.txt");

        let changed = normalize_with(&conn, strip_fake).unwrap();
        // 一行删重复 + 一行改写 = 2 个动作。
        assert_eq!(changed, 2);

        let mut stmt = conn
            .prepare("SELECT path FROM documents ORDER BY id")
            .unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        drop(stmt);
        assert_eq!(paths, [r"C:\docs\a.txt", r"C:\docs\b.txt"]);

        // 重复行被删掉后,它的 contents 与 FTS 行都不应残留。
        let contents: i64 = conn
            .query_row("SELECT count(*) FROM document_contents", [], |r| r.get(0))
            .unwrap();
        let fts: i64 = conn
            .query_row("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(contents, 2, "重复行的 contents 应随文档删除");
        assert_eq!(fts, 2, "重复行的 FTS 行应被删掉");

        // 完整性检查应通过(没有孤儿 FTS 行)。
        let report = crate::maintain::check_integrity(&conn).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());

        // 再跑一次:幂等。
        assert_eq!(normalize_with(&conn, strip_fake).unwrap(), 0);
    }
}
