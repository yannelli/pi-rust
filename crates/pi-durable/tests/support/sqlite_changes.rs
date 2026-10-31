//! Upstream chord `test/sqlite/helpers.ts` `runChanges`: the rows each kind of statement changes, as `run_counted`
//! reports them, on a table with two indexes, so a driver that counted index writes would report more.

use pi_durable::DurableResult;
use pi_durable::storage::sqlite::{SqliteExecutor, SqliteValue};

/// What `run_changes` reports on every driver (upstream `RUN_CHANGES`).
pub const RUN_CHANGES: [(&str, u64); 7] =
    [("insert", 1), ("update", 1), ("none", 0), ("bulk", 10), ("upsert", 1), ("delete", 1), ("bulkDelete", 10)];

fn text(value: &str) -> SqliteValue {
    SqliteValue::Text(value.into())
}

pub async fn run_changes(database: &dyn SqliteExecutor) -> DurableResult<Vec<(&'static str, u64)>> {
    database
        .exec("CREATE TABLE changes_probe (id TEXT PRIMARY KEY, path TEXT NOT NULL, n INTEGER NOT NULL) STRICT, WITHOUT ROWID")
        .await?;
    database.exec("CREATE INDEX changes_probe_path ON changes_probe (path, id)").await?;
    database.exec("CREATE INDEX changes_probe_n ON changes_probe (n, id)").await?;
    for i in 0..10 {
        let row = [text(&format!("k{i}")), text(&format!("a/{i}")), SqliteValue::Integer(i)];
        database.run("INSERT INTO changes_probe VALUES (?, ?, ?)", &row).await?;
    }
    let statements: [(&str, &str, Vec<SqliteValue>); 7] = [
        (
            "insert",
            "INSERT INTO changes_probe VALUES (?, ?, ?)",
            vec![text("x"), text("a/x"), SqliteValue::Integer(99)],
        ),
        ("update", "UPDATE changes_probe SET path = ? WHERE id = ?", vec![text("b/x"), text("x")]),
        ("none", "UPDATE changes_probe SET n = 0 WHERE id = ?", vec![text("absent")]),
        ("bulk", "UPDATE changes_probe SET path = 'c/' || substr(path, 3) WHERE path >= 'a/' AND path < 'a0'", vec![]),
        (
            "upsert",
            "INSERT INTO changes_probe VALUES (?, ?, ?) ON CONFLICT (id) DO UPDATE SET n = excluded.n",
            vec![text("x"), text("b/x"), SqliteValue::Integer(5)],
        ),
        ("delete", "DELETE FROM changes_probe WHERE id = ?", vec![text("x")]),
        ("bulkDelete", "DELETE FROM changes_probe WHERE path >= 'c/' AND path < 'c0'", vec![]),
    ];
    let mut changes = Vec::new();
    for (name, sql, params) in statements {
        changes.push((name, database.run_counted(sql, &params).await?));
    }
    Ok(changes)
}

/// Not upstream: an insert whose trigger writes a second row still counts 1, as `changes()` does; a count taken from
/// `total_changes()` before and after would report 2. Returns the count and the rows the trigger wrote.
pub async fn trigger_changes(database: &dyn SqliteExecutor) -> DurableResult<(u64, i64)> {
    database.exec("CREATE TABLE changes_log (id TEXT NOT NULL) STRICT").await?;
    database
        .exec("CREATE TRIGGER changes_logged AFTER INSERT ON changes_probe BEGIN INSERT INTO changes_log VALUES (new.id); END")
        .await?;
    let row = [text("g"), text("g/1"), SqliteValue::Integer(1)];
    let counted = database.run_counted("INSERT INTO changes_probe VALUES (?, ?, ?)", &row).await?;
    let logged = database.get("SELECT count(*) AS logged FROM changes_log", &[]).await?;
    Ok((counted, logged.map_or(Ok(0), |row| row.integer("logged"))?))
}
