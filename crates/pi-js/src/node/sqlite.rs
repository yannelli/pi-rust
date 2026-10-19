//! Natives for `node:sqlite`: connections on rusqlite with the bundled SQLite. Statements are
//! prepared through the connection's statement cache on each call, so a `StatementSync` keeps
//! only its SQL text.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use rquickjs::{Array, BigInt, Ctx, Object, TypedArray, Value};
use rusqlite::config::DbConfig;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, Statement};

use super::set_fn;
use super::sqlite_fn::{self, Enter};
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, throw};
use crate::runtime::state::state;

const READ_BIG_INTS: u32 = 1;
const RETURN_ARRAYS: u32 = 2;
const ALLOW_BARE: u32 = 4;
const ALLOW_UNKNOWN: u32 = 8;

// Query modes; 0 returns every row.
const MODE_GET: u8 = 1;
const MODE_RUN: u8 = 2;
const MODE_EXPAND: u8 = 3;

const MAX_SAFE: u64 = (1 << 53) - 1;

/// An open connection plus the callbacks its user functions hold.
pub(crate) struct Db {
    pub conn: Connection,
    pub callbacks: std::cell::RefCell<Vec<u64>>,
}

impl Drop for Db {
    fn drop(&mut self) {
        let callbacks = std::mem::take(&mut *self.callbacks.borrow_mut());
        sqlite_fn::release(&callbacks);
    }
}

/// The message `sqlite3_errstr` gives for a result code.
pub(crate) fn errstr(code: i32) -> &'static str {
    match code {
        516 => return "abort due to ROLLBACK",
        100 => return "another row available",
        101 => return "no more rows available",
        _ => {}
    }
    const MESSAGES: [&str; 29] = [
        "not an error",
        "SQL logic error",
        "",
        "access permission denied",
        "query aborted",
        "database is locked",
        "database table is locked",
        "out of memory",
        "attempt to write a readonly database",
        "interrupted",
        "disk I/O error",
        "database disk image is malformed",
        "unknown operation",
        "database or disk is full",
        "unable to open database file",
        "locking protocol",
        "",
        "database schema has changed",
        "string or blob too big",
        "constraint failed",
        "datatype mismatch",
        "bad parameter or other API misuse",
        "",
        "authorization denied",
        "",
        "column index out of range",
        "file is not a database",
        "notification message",
        "warning message",
    ];
    match MESSAGES.get((code & 0xff) as usize) {
        Some(message) if !message.is_empty() => message,
        _ => "unknown error",
    }
}

pub(crate) fn state_error(message: impl Into<String>) -> JsError {
    JsError::new(message).with_code("ERR_INVALID_STATE")
}

fn sqlite_failure(message: String, code: i32) -> JsError {
    JsError::new(message).with_code("ERR_SQLITE_ERROR").with_data("errcode", code).with_data("errstr", errstr(code))
}

/// A SQLite failure as Node reports it; a JavaScript exception from a user function wins.
pub(crate) fn sql_error(ctx: &Ctx<'_>, error: rusqlite::Error) -> rquickjs::Error {
    if let Some(thrown) = sqlite_fn::take_pending(ctx) {
        return ctx.throw(thrown);
    }
    let error = match error {
        rusqlite::Error::SqliteFailure(failure, message) => {
            let code = failure.extended_code;
            sqlite_failure(message.unwrap_or_else(|| errstr(code).to_string()), code)
        }
        rusqlite::Error::SqlInputError { error, msg, .. } => sqlite_failure(msg, error.extended_code),
        rusqlite::Error::InvalidPath(_) | rusqlite::Error::NulError(_) => {
            JsError::type_error("The \"path\" argument must be a string, Uint8Array, or URL without null bytes.")
                .with_code("ERR_INVALID_ARG_TYPE")
        }
        rusqlite::Error::MultipleStatement => sqlite_failure("multiple statements".into(), 1),
        other => sqlite_failure(other.to_string(), 1),
    };
    throw(ctx, &error)
}

/// rusqlite adds the path to open failures; SQLite's own message does not have it.
pub(crate) fn without_path(error: rusqlite::Error, path: &str) -> rusqlite::Error {
    match error {
        rusqlite::Error::SqliteFailure(failure, Some(message)) => {
            let message =
                message.strip_suffix(path).and_then(|rest| rest.strip_suffix(": ")).unwrap_or(&message).to_string();
            rusqlite::Error::SqliteFailure(failure, Some(message))
        }
        other => other,
    }
}

pub(crate) fn database(ctx: &Ctx<'_>, id: u64) -> rquickjs::Result<Rc<Db>> {
    state()
        .with_resource(id, |db: &mut Rc<Db>| db.clone())
        .map_err(|_| throw(ctx, &state_error("database is not open")))
}

fn open(
    ctx: Ctx<'_>,
    path: String,
    read_only: bool,
    foreign_keys: bool,
    dqs: bool,
    timeout: f64,
) -> rquickjs::Result<u64> {
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    };
    let conn =
        Connection::open_with_flags(&path, flags).map_err(|error| sql_error(&ctx, without_path(error, &path)))?;
    let configure = || -> rusqlite::Result<()> {
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, foreign_keys)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, dqs)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, dqs)?;
        conn.busy_timeout(Duration::from_millis(timeout.max(0.0) as u64))
    };
    configure().map_err(|error| sql_error(&ctx, error))?;
    conn.set_prepared_statement_cache_capacity(64);
    let db = Rc::new(Db { conn, callbacks: Default::default() });
    Ok(state().insert_resource(Box::new(db)))
}

fn close(ctx: Ctx<'_>, id: u64) -> rquickjs::Result<()> {
    let removed = state().resources.borrow_mut().remove(&id);
    match removed {
        Some(_) => Ok(()),
        None => Err(throw(&ctx, &state_error("database is not open"))),
    }
}

fn exec(ctx: Ctx<'_>, id: u64, sql: String) -> rquickjs::Result<()> {
    let db = database(&ctx, id)?;
    let _enter = Enter::new(&ctx);
    db.conn.execute_batch(&sql).map_err(|error| sql_error(&ctx, error))
}

fn prepare(ctx: Ctx<'_>, id: u64, sql: String) -> rquickjs::Result<()> {
    let db = database(&ctx, id)?;
    db.conn.prepare_cached(&sql).map(drop).map_err(|error| sql_error(&ctx, error))
}

/// A JavaScript value as a value for parameter `index`; `None` when it cannot be bound.
fn bindable(value: &Value<'_>) -> Option<SqlValue> {
    if value.is_null() {
        return Some(SqlValue::Null);
    }
    if let Some(number) = value.as_number() {
        return Some(SqlValue::Real(number));
    }
    if let Some(text) = value.as_string() {
        return text.to_string().ok().map(SqlValue::Text);
    }
    if let Some(big) = value.as_big_int() {
        return big.clone().to_i64().ok().map(SqlValue::Integer);
    }
    TypedArray::<u8>::from_value(value.clone()).ok().map(|array| SqlValue::Blob(bytes_of(&array)))
}

fn bind_one(ctx: &Ctx<'_>, stmt: &mut Statement<'_>, index: usize, value: &Value<'_>) -> rquickjs::Result<()> {
    let Some(value) = bindable(value) else {
        let message = format!("Provided value cannot be bound to SQLite parameter {index}.");
        return Err(throw(ctx, &JsError::type_error(message).with_code("ERR_INVALID_ARG_TYPE")));
    };
    stmt.raw_bind_parameter(index, value).map_err(|error| sql_error(ctx, error))
}

/// Bare names (without `:`, `$`, or `@`) to the statement's full parameter names.
fn bare_names(stmt: &Statement<'_>) -> Result<HashMap<String, String>, JsError> {
    let mut names = HashMap::new();
    for index in 1..=stmt.parameter_count() {
        let Some(full) = stmt.parameter_name(index).filter(|name| !name.starts_with('?')) else { continue };
        let bare = full.get(1..).unwrap_or_default().to_string();
        if let Some(other) = names.get(&bare).filter(|other: &&String| *other != full) {
            return Err(state_error(format!(
                "Cannot create bare named parameter '{bare}' because of conflicting names '{other}' and '{full}'."
            )));
        }
        names.insert(bare, full.to_string());
    }
    Ok(names)
}

fn bind<'js>(
    ctx: &Ctx<'js>,
    stmt: &mut Statement<'_>,
    args: &Array<'js>,
    named: Option<&Object<'js>>,
    flags: u32,
) -> rquickjs::Result<()> {
    if let Some(named) = named {
        let mut bare = None;
        for key in named.keys::<String>() {
            let key = key?;
            let full = if key.starts_with([':', '$', '@']) {
                Some(key.clone())
            } else if flags & ALLOW_BARE != 0 {
                if bare.is_none() {
                    bare = Some(bare_names(stmt).map_err(|error| throw(ctx, &error))?);
                }
                bare.as_ref().and_then(|names| names.get(&key).cloned())
            } else {
                None
            };
            let index = full.and_then(|full| stmt.parameter_index(&full).ok().flatten());
            match index {
                Some(index) => bind_one(ctx, stmt, index, &named.get::<_, Value>(key.as_str())?)?,
                None if flags & ALLOW_UNKNOWN != 0 => {}
                None => return Err(throw(ctx, &state_error(format!("Unknown named parameter '{key}'")))),
            }
        }
    }
    let count = stmt.parameter_count();
    let mut index = 1;
    for value in args.iter::<Value>() {
        while index <= count && stmt.parameter_name(index).is_some_and(|name| !name.starts_with('?')) {
            index += 1;
        }
        bind_one(ctx, stmt, index, &value?)?;
        index += 1;
    }
    Ok(())
}

/// A column or function argument value as JavaScript.
pub(crate) fn to_js<'js>(ctx: &Ctx<'js>, value: ValueRef<'_>, big: bool) -> rquickjs::Result<Value<'js>> {
    Ok(match value {
        ValueRef::Null => Value::new_null(ctx.clone()),
        ValueRef::Integer(number) if big => BigInt::from_i64(ctx.clone(), number)?.into_value(),
        ValueRef::Integer(number) if number.unsigned_abs() > MAX_SAFE => {
            let message = format!("Value is too large to be represented as a JavaScript number: {number}");
            return Err(throw(ctx, &JsError::range_error(message).with_code("ERR_OUT_OF_RANGE")));
        }
        ValueRef::Integer(number) => Value::new_number(ctx.clone(), number as f64),
        ValueRef::Real(number) => Value::new_number(ctx.clone(), number),
        ValueRef::Text(text) => rquickjs::String::from_str(ctx.clone(), &String::from_utf8_lossy(text))?.into_value(),
        ValueRef::Blob(bytes) => TypedArray::<u8>::new(ctx.clone(), bytes.to_vec())?.into_value(),
    })
}

fn row_value<'js>(
    ctx: &Ctx<'js>,
    row: &rusqlite::Row<'_>,
    names: &[String],
    flags: u32,
) -> rquickjs::Result<Value<'js>> {
    let big = flags & READ_BIG_INTS != 0;
    let cell = |index: usize| -> rquickjs::Result<Value<'js>> {
        let value = row.get_ref(index).map_err(|error| sql_error(ctx, error))?;
        to_js(ctx, value, big)
    };
    if flags & RETURN_ARRAYS != 0 {
        let array = Array::new(ctx.clone())?;
        for index in 0..names.len() {
            array.set(index, cell(index)?)?;
        }
        return Ok(array.into_value());
    }
    let object = Object::new(ctx.clone())?;
    object.set_prototype(None)?;
    for (index, name) in names.iter().enumerate() {
        object.set(name.as_str(), cell(index)?)?;
    }
    Ok(object.into_value())
}

fn count<'js>(ctx: &Ctx<'js>, value: i64, big: bool) -> rquickjs::Result<Value<'js>> {
    if big {
        Ok(BigInt::from_i64(ctx.clone(), value)?.into_value())
    } else {
        Ok(Value::new_number(ctx.clone(), value as f64))
    }
}

/// Bind `args` (and the `named` object) to `sql` and run it: every row, the first row,
/// `{ changes, lastInsertRowid }`, or the expanded SQL text, by `mode`.
fn query<'js>(
    ctx: Ctx<'js>,
    id: u64,
    sql: String,
    args: Array<'js>,
    named: Value<'js>,
    flags: u32,
    mode: u8,
) -> rquickjs::Result<Value<'js>> {
    let db = database(&ctx, id)?;
    let _enter = Enter::new(&ctx);
    let mut stmt = db.conn.prepare_cached(&sql).map_err(|error| sql_error(&ctx, error))?;
    bind(&ctx, &mut stmt, &args, named.as_object(), flags)?;
    if mode == MODE_EXPAND {
        let text = stmt.expanded_sql();
        return match text {
            Some(text) => Ok(rquickjs::String::from_str(ctx.clone(), &text)?.into_value()),
            None => Ok(Value::new_null(ctx.clone())),
        };
    }
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    let all = Array::new(ctx.clone())?;
    let mut first = None;
    {
        let mut rows = stmt.raw_query();
        loop {
            match rows.next() {
                Ok(Some(_)) if mode == MODE_RUN => {}
                Ok(Some(row)) => {
                    let value = row_value(&ctx, row, &names, flags)?;
                    if mode == MODE_GET {
                        first = Some(value);
                        break;
                    }
                    all.set(all.len(), value)?;
                }
                Ok(None) => break,
                Err(error) => return Err(sql_error(&ctx, error)),
            }
        }
    }
    match mode {
        MODE_GET => Ok(first.unwrap_or_else(|| Value::new_undefined(ctx.clone()))),
        MODE_RUN => {
            let big = flags & READ_BIG_INTS != 0;
            let result = Object::new(ctx.clone())?;
            result.set("lastInsertRowid", count(&ctx, db.conn.last_insert_rowid(), big)?)?;
            result.set("changes", count(&ctx, db.conn.changes() as i64, big)?)?;
            Ok(result.into_value())
        }
        _ => Ok(all.into_value()),
    }
}

fn columns<'js>(ctx: Ctx<'js>, id: u64, sql: String) -> rquickjs::Result<Array<'js>> {
    let db = database(&ctx, id)?;
    let stmt = db.conn.prepare_cached(&sql).map_err(|error| sql_error(&ctx, error))?;
    let types: Vec<Option<String>> =
        stmt.columns().iter().map(|column| column.decl_type().map(str::to_string)).collect();
    let list = Array::new(ctx.clone())?;
    for (index, meta) in stmt.columns_with_metadata().iter().enumerate() {
        let entry = Object::new(ctx.clone())?;
        entry.set_prototype(None)?;
        let text = |value: Option<&str>| -> rquickjs::Result<Value<'js>> {
            match value {
                Some(value) => Ok(rquickjs::String::from_str(ctx.clone(), value)?.into_value()),
                None => Ok(Value::new_null(ctx.clone())),
            }
        };
        entry.set("column", text(meta.origin_name())?)?;
        entry.set("database", text(meta.database_name())?)?;
        entry.set("name", meta.name())?;
        entry.set("table", text(meta.table_name())?)?;
        entry.set("type", text(types.get(index).and_then(Option::as_deref))?)?;
        list.set(index, entry)?;
    }
    Ok(list)
}

fn location(ctx: Ctx<'_>, id: u64, name: String) -> rquickjs::Result<Option<String>> {
    let db = database(&ctx, id)?;
    let mut stmt = db.conn.prepare("PRAGMA database_list").map_err(|error| sql_error(&ctx, error))?;
    let mut rows = stmt.query([]).map_err(|error| sql_error(&ctx, error))?;
    while let Some(row) = rows.next().map_err(|error| sql_error(&ctx, error))? {
        let entry: String = row.get(1).map_err(|error| sql_error(&ctx, error))?;
        if entry == name {
            let file: Option<String> = row.get(2).map_err(|error| sql_error(&ctx, error))?;
            return Ok(file.filter(|file| !file.is_empty()));
        }
    }
    Ok(None)
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "open", open)?;
    set_fn(&object, "close", close)?;
    set_fn(&object, "exec", exec)?;
    set_fn(&object, "prepare", prepare)?;
    set_fn(&object, "query", query)?;
    set_fn(&object, "columns", columns)?;
    set_fn(&object, "location", location)?;
    set_fn(&object, "isTransaction", |ctx: Ctx<'js>, id: u64| {
        Ok::<_, rquickjs::Error>(!database(&ctx, id)?.conn.is_autocommit())
    })?;
    sqlite_fn::install(&object)?;
    Ok(object)
}
