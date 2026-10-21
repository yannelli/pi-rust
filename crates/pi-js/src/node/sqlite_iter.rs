//! `StatementSync.iterate()`: a prepared statement that steps one row per `next()`. Each
//! iterator owns its statement, kept on the connection so closing it finalizes the statement first.
#![allow(unsafe_code)]

use std::ptr::NonNull;

use rquickjs::{Array, Ctx, Object, Value};
use rusqlite::{Rows, Statement};

use super::set_fn;
use super::sqlite::{bind, database, row_value, sql_error};
use super::sqlite_fn::Enter;
use crate::runtime::state::state;

/// A running statement and the rows it has left.
pub(crate) struct Cursor {
    rows: Option<Rows<'static>>,
    stmt: NonNull<Statement<'static>>,
    names: Vec<String>,
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // Rows reset the statement when dropped, so they go before it.
        self.rows = None;
        // SAFETY: `stmt` came from `Box::leak` in `start` and nothing borrows it now.
        drop(unsafe { Box::from_raw(self.stmt.as_ptr()) });
    }
}

/// Prepare `sql`, bind the arguments, and keep the statement on the connection; returns its id.
fn start<'js>(
    ctx: Ctx<'js>,
    id: u64,
    sql: String,
    args: Array<'js>,
    named: Value<'js>,
    flags: u32,
) -> rquickjs::Result<u64> {
    let db = database(&ctx, id)?;
    let _enter = Enter::new(&ctx);
    let stmt = db.conn.prepare(&sql).map_err(|error| sql_error(&ctx, error))?;
    let names = stmt.column_names().into_iter().map(str::to_string).collect();
    // SAFETY: the statement borrows the connection inside the `Rc<Db>`, which does not move.
    // `Db::drop` drops every cursor before the connection, so the borrow never dangles.
    let stmt = unsafe { std::mem::transmute::<Statement<'_>, Statement<'static>>(stmt) };
    let mut cursor = Cursor { rows: None, stmt: NonNull::from(Box::leak(Box::new(stmt))), names };
    // SAFETY: the cursor owns the statement; `rows` is the only borrow and drops before it.
    let stmt = unsafe { &mut *cursor.stmt.as_ptr() };
    bind(&ctx, stmt, &args, named.as_object(), flags)?;
    cursor.rows = Some(stmt.raw_query());
    let cursor_id = state().next_id();
    db.cursors.borrow_mut().insert(cursor_id, cursor);
    Ok(cursor_id)
}

/// The next row of a cursor read with `flags`, or undefined once it has none; a finished or
/// failed cursor is released.
fn step<'js>(ctx: Ctx<'js>, id: u64, cursor_id: u64, flags: u32) -> rquickjs::Result<Value<'js>> {
    let db = database(&ctx, id)?;
    // Taken out while SQLite runs, so a user function can use other cursors.
    let Some(mut cursor) = db.cursors.borrow_mut().remove(&cursor_id) else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    let _enter = Enter::new(&ctx);
    let value = match cursor.rows.as_mut().map(|rows| rows.next()) {
        Some(Ok(Some(row))) => row_value(&ctx, row, &cursor.names, flags)?,
        Some(Ok(None)) | None => return Ok(Value::new_undefined(ctx.clone())),
        Some(Err(error)) => return Err(sql_error(&ctx, error)),
    };
    db.cursors.borrow_mut().insert(cursor_id, cursor);
    Ok(value)
}

fn finish(ctx: Ctx<'_>, id: u64, cursor_id: u64) -> rquickjs::Result<()> {
    let removed = database(&ctx, id)?.cursors.borrow_mut().remove(&cursor_id);
    drop(removed);
    Ok(())
}

pub(crate) fn install(object: &Object<'_>) -> rquickjs::Result<()> {
    set_fn(object, "iterate", start)?;
    set_fn(object, "step", step)?;
    set_fn(object, "finish", finish)?;
    Ok(())
}
