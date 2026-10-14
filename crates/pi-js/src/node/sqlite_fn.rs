//! User-defined SQL functions and aggregates for `node:sqlite`, which call JavaScript while
//! SQLite runs a statement, and `sqlite.backup`.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ptr::NonNull;

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Function, Object, Persistent, TypedArray, Value, qjs};
use rusqlite::functions::{Aggregate, Context, FunctionFlags, WindowAggregate};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};

use super::set_fn;
use super::sqlite::{database, sql_error, to_js, without_path};
use crate::runtime::convert::{bytes_of, js_bytes};
use crate::runtime::state::{state, try_state};

thread_local! {
    /// The context of the native call that is running SQLite, for callbacks into JavaScript.
    static CURRENT: Cell<Option<NonNull<qjs::JSContext>>> = const { Cell::new(None) };
    /// A `refs` id holding what a user function threw, rethrown when the statement fails.
    static PENDING: Cell<u64> = const { Cell::new(0) };
}

/// Marks `ctx` as the context SQLite callbacks use until dropped.
pub(crate) struct Enter(Option<NonNull<qjs::JSContext>>);

impl Enter {
    pub(crate) fn new(ctx: &Ctx<'_>) -> Self {
        Self(CURRENT.with(|current| current.replace(Some(ctx.as_raw()))))
    }
}

impl Drop for Enter {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.0));
    }
}

fn with_current<R>(f: impl for<'js> FnOnce(&Ctx<'js>) -> rusqlite::Result<R>) -> rusqlite::Result<R> {
    let Some(raw) = CURRENT.with(Cell::get) else {
        return Err(user_error("JavaScript is not running"));
    };
    // SAFETY: `Enter` stores the context of a native call that is still on the stack, so the
    // context is alive and its runtime lock is held by this thread.
    let ctx = unsafe { Ctx::from_raw(raw) };
    f(&ctx)
}

fn user_error(message: &str) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into())
}

fn keep<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> u64 {
    let state = state();
    let id = state.next_id();
    state.refs.borrow_mut().insert(id, Persistent::save(ctx, value));
    id
}

fn kept<'js>(ctx: &Ctx<'js>, id: u64) -> rquickjs::Result<Value<'js>> {
    let saved = state().refs.borrow().get(&id).cloned();
    match saved {
        Some(saved) => saved.restore(ctx),
        None => Ok(Value::new_undefined(ctx.clone())),
    }
}

/// Drop kept values; safe while the runtime shuts down.
pub(crate) fn release(ids: &[u64]) {
    if let Some(state) = try_state() {
        let mut refs = state.refs.borrow_mut();
        for id in ids {
            refs.remove(id);
        }
    }
}

/// What a user function threw, if one did since the statement started.
pub(crate) fn take_pending<'js>(ctx: &Ctx<'js>) -> Option<Value<'js>> {
    let id = PENDING.with(|pending| pending.replace(0));
    if id == 0 {
        return None;
    }
    let value = kept(ctx, id).ok();
    release(&[id]);
    value
}

/// Call a kept JavaScript function; a throw is saved for `take_pending`.
fn call_kept<'js>(ctx: &Ctx<'js>, id: u64, args: Vec<Value<'js>>) -> rusqlite::Result<Value<'js>> {
    let function = kept(ctx, id).ok().and_then(|value| value.into_function());
    let Some(function) = function else {
        return Err(user_error("function is gone"));
    };
    match function.call::<_, Value>((Rest(args),)) {
        Ok(value) => Ok(value),
        Err(rquickjs::Error::Exception) => {
            let thrown = ctx.catch();
            let id = keep(ctx, thrown);
            let previous = PENDING.with(|pending| pending.replace(id));
            release(&[previous]);
            Err(user_error("JavaScript exception"))
        }
        Err(other) => Err(user_error(&other.to_string())),
    }
}

fn arguments<'js>(ctx: &Ctx<'js>, call: &Context<'_>, big: bool) -> rusqlite::Result<Vec<Value<'js>>> {
    (0..call.len())
        .map(|index| {
            to_js(ctx, call.get_raw(index), big).map_err(|_| {
                let thrown = ctx.catch();
                let message = thrown.as_object().and_then(|error| error.get::<_, String>("message").ok());
                user_error(&message.unwrap_or_default())
            })
        })
        .collect()
}

/// A JavaScript return value as a SQLite value, with Node's errors.
fn sql_value<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> rusqlite::Result<SqlValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(SqlValue::Null);
    }
    if let Some(number) = value.as_number() {
        return Ok(SqlValue::Real(number));
    }
    if let Some(text) = value.as_string() {
        return Ok(SqlValue::Text(text.to_string().unwrap_or_default()));
    }
    if value.is_big_int() {
        let text = value.get::<rquickjs::Coerced<String>>().map(|text| text.0).unwrap_or_default();
        return text.parse().map(SqlValue::Integer).map_err(|_| user_error("BigInt value is too large for SQLite"));
    }
    if value.is_promise() {
        return Err(user_error("Asynchronous user-defined functions are not supported"));
    }
    if let Ok(array) = TypedArray::<u8>::from_value(value.clone()) {
        return Ok(SqlValue::Blob(bytes_of(&array)));
    }
    let is_view = ctx
        .globals()
        .get::<_, Object>("ArrayBuffer")
        .and_then(|buffer| buffer.get::<_, Function>("isView"))
        .and_then(|is_view| is_view.call::<_, bool>((value.clone(),)))
        .unwrap_or(false);
    if is_view {
        if let Ok(Some(bytes)) = js_bytes(ctx, &value) {
            return Ok(SqlValue::Blob(bytes));
        }
    }
    Err(user_error("Returned JavaScript value cannot be converted to a SQLite value"))
}

fn function_flags(flags: u32) -> FunctionFlags {
    let mut result = FunctionFlags::SQLITE_UTF8;
    if flags & 1 != 0 {
        result |= FunctionFlags::SQLITE_DETERMINISTIC;
    }
    if flags & 2 != 0 {
        result |= FunctionFlags::SQLITE_DIRECTONLY;
    }
    result
}

const BIG_ARGUMENTS: u32 = 4;

fn function<'js>(
    ctx: Ctx<'js>,
    id: u64,
    name: String,
    callback: Function<'js>,
    arity: i32,
    flags: u32,
) -> rquickjs::Result<()> {
    let db = database(&ctx, id)?;
    let kept = keep(&ctx, callback.into_value());
    db.callbacks.borrow_mut().push(kept);
    let big = flags & BIG_ARGUMENTS != 0;
    let registered = db.conn.create_scalar_function(name.as_str(), arity, function_flags(flags), move |call| {
        with_current(|ctx| {
            let args = arguments(ctx, call, big)?;
            let value = call_kept(ctx, kept, args)?;
            sql_value(ctx, value)
        })
    });
    registered.map_err(|error| sql_error(&ctx, error))
}

/// A JavaScript aggregate: `start` is a value or a function, the rest are kept functions.
#[derive(Clone, Copy)]
struct JsAggregate {
    start: u64,
    start_is_function: bool,
    step: u64,
    result: Option<u64>,
    inverse: Option<u64>,
    big: bool,
}

impl JsAggregate {
    fn initial<'js>(&self, ctx: &Ctx<'js>) -> rusqlite::Result<u64> {
        let value = if self.start_is_function {
            call_kept(ctx, self.start, Vec::new())?
        } else {
            kept(ctx, self.start).map_err(|error| user_error(&error.to_string()))?
        };
        Ok(keep(ctx, value))
    }

    fn fold(&self, call: &Context<'_>, function: u64, acc: &mut u64) -> rusqlite::Result<()> {
        with_current(|ctx| {
            let mut args = vec![kept(ctx, *acc).map_err(|error| user_error(&error.to_string()))?];
            args.extend(arguments(ctx, call, self.big)?);
            let next = call_kept(ctx, function, args)?;
            release(&[*acc]);
            *acc = keep(ctx, next);
            Ok(())
        })
    }

    fn output(&self, acc: Option<u64>, finish: bool) -> rusqlite::Result<SqlValue> {
        with_current(|ctx| {
            let acc = match acc {
                Some(acc) => acc,
                None => self.initial(ctx)?,
            };
            let value = kept(ctx, acc).map_err(|error| user_error(&error.to_string()))?;
            if finish {
                release(&[acc]);
            }
            let value = match self.result {
                Some(result) => call_kept(ctx, result, vec![value])?,
                None => value,
            };
            sql_value(ctx, value)
        })
    }
}

impl Aggregate<u64, SqlValue> for JsAggregate {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<u64> {
        with_current(|ctx| self.initial(ctx))
    }

    fn step(&self, call: &mut Context<'_>, acc: &mut u64) -> rusqlite::Result<()> {
        self.fold(call, self.step, acc)
    }

    fn finalize(&self, _: &mut Context<'_>, acc: Option<u64>) -> rusqlite::Result<SqlValue> {
        self.output(acc, true)
    }
}

impl WindowAggregate<u64, SqlValue> for JsAggregate {
    fn value(&self, acc: Option<&mut u64>) -> rusqlite::Result<SqlValue> {
        self.output(acc.copied(), false)
    }

    fn inverse(&self, call: &mut Context<'_>, acc: &mut u64) -> rusqlite::Result<()> {
        self.fold(call, self.inverse.unwrap_or(self.step), acc)
    }
}

/// `functions` is `[start, step, result, inverse]`; `result` and `inverse` may be undefined.
fn aggregate<'js>(
    ctx: Ctx<'js>,
    id: u64,
    name: String,
    functions: rquickjs::Array<'js>,
    arity: i32,
    flags: u32,
) -> rquickjs::Result<()> {
    let db = database(&ctx, id)?;
    let keep_slot = |index: usize| -> rquickjs::Result<Option<u64>> {
        let value: Value = functions.get(index)?;
        if index > 1 && value.is_undefined() {
            return Ok(None);
        }
        let kept = keep(&ctx, value);
        db.callbacks.borrow_mut().push(kept);
        Ok(Some(kept))
    };
    let start_is_function = functions.get::<Value>(0)?.is_function();
    let aggregate = JsAggregate {
        start: keep_slot(0)?.unwrap_or_default(),
        start_is_function,
        step: keep_slot(1)?.unwrap_or_default(),
        result: keep_slot(2)?,
        inverse: keep_slot(3)?,
        big: flags & BIG_ARGUMENTS != 0,
    };
    let flags = function_flags(flags);
    let registered = if aggregate.inverse.is_some() {
        db.conn.create_window_function(name.as_str(), arity, flags, aggregate)
    } else {
        db.conn.create_aggregate_function(name.as_str(), arity, flags, aggregate)
    };
    registered.map_err(|error| sql_error(&ctx, error))
}

/// Copy database `source` of connection `id` into `target` of the file at `path`, `rate`
/// pages at a time, calling `progress` between steps; returns the page count.
fn backup<'js>(
    ctx: Ctx<'js>,
    id: u64,
    path: String,
    source: String,
    destination: String,
    rate: i32,
    progress: Opt<Function<'js>>,
) -> rquickjs::Result<i32> {
    use rusqlite::backup::{Backup, StepResult};
    let db = database(&ctx, id)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI;
    let mut target =
        Connection::open_with_flags(&path, flags).map_err(|error| sql_error(&ctx, without_path(error, &path)))?;
    let job = Backup::new_with_names(&db.conn, source.as_str(), &mut target, destination.as_str())
        .map_err(|error| sql_error(&ctx, error))?;
    loop {
        let step = job.step(rate).map_err(|error| sql_error(&ctx, error))?;
        let status = job.progress();
        match step {
            StepResult::Done => return Ok(status.pagecount),
            StepResult::More => {
                if let Some(progress) = &progress.0 {
                    let info = Object::new(ctx.clone())?;
                    info.set("totalPages", status.pagecount)?;
                    info.set("remainingPages", status.remaining)?;
                    progress.call::<_, ()>((info,))?;
                }
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
}

pub(crate) fn install(object: &Object<'_>) -> rquickjs::Result<()> {
    set_fn(object, "function", function)?;
    set_fn(object, "aggregate", aggregate)?;
    set_fn(object, "backup", backup)?;
    Ok(())
}
