//! Natives for the `WebAssembly` global on wasmi. Calls into Wasm are resumable: a JavaScript
//! import makes the call pause with a host error, the import runs while the store is free (so
//! it may call back into Wasm), and the call resumes with its results.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use rquickjs::{Array, BigInt, Ctx, Function, Object, Persistent, Value};
use wasmi::core::{HostError, ValType};
use wasmi::{
    Config, Engine, Error, Extern, ExternRef, ExternType, Func, FuncRef, FuncType, Instance, Module, ResumableCall,
    Store, Val,
};

use super::set_fn;
use super::wasm_objects::{self, HIDDEN, START_EXPORT};
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, throw};
use crate::runtime::state::state;

/// The engine and store of this runtime, with ids for the entities JavaScript holds.
pub(crate) struct Wasm {
    engine: Engine,
    pub store: RefCell<Store<()>>,
    modules: RefCell<Vec<ModuleEntry>>,
    pub externs: RefCell<Registry>,
    /// Host functions made for JavaScript imports, by function id, to the `refs` id of the import.
    imports: RefCell<HashMap<u32, u64>>,
    /// Memories whose buffer JavaScript holds: `(refs id, data pointer, length)` by memory id.
    pub buffers: RefCell<HashMap<u32, (u64, usize, usize)>>,
}

struct ModuleEntry {
    module: Module,
    has_start: bool,
    exports: Vec<String>,
    imports: Vec<usize>,
}

/// Functions, memories, tables, and globals by id; the same entity always gets the same id.
#[derive(Default)]
pub(crate) struct Registry {
    items: Vec<Extern>,
    keys: HashMap<String, u32>,
}

impl Registry {
    pub fn id(&mut self, item: impl Into<Extern>) -> u32 {
        let item = item.into();
        let key = format!("{item:?}");
        if let Some(id) = self.keys.get(&key) {
            return *id;
        }
        self.items.push(item);
        let id = self.items.len() as u32 - 1;
        self.keys.insert(key, id);
        id
    }

    pub fn get(&self, id: u32) -> Option<Extern> {
        self.items.get(id as usize).cloned()
    }
}

/// The host error a JavaScript import raises to pause the Wasm call.
#[derive(Debug)]
struct JsCall {
    function: u64,
    params: Vec<Val>,
}

impl std::fmt::Display for JsCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("call to a JavaScript import")
    }
}

impl HostError for JsCall {}

thread_local! {
    static WASM: Cell<u64> = const { Cell::new(0) };
}

/// This runtime's Wasm state, created on first use and released with the other resources.
pub(crate) fn wasm() -> Rc<Wasm> {
    let state = state();
    if let Ok(found) = state.with_resource(WASM.with(Cell::get), |wasm: &mut Rc<Wasm>| wasm.clone()) {
        return found;
    }
    let engine = Engine::new(&Config::default());
    let wasm = Rc::new(Wasm {
        store: RefCell::new(Store::new(&engine, ())),
        engine,
        modules: RefCell::default(),
        externs: RefCell::default(),
        imports: RefCell::default(),
        buffers: RefCell::default(),
    });
    WASM.with(|slot| slot.set(state.insert_resource(Box::new(wasm.clone()))));
    wasm
}

/// An error JavaScript turns into `WebAssembly[kind]` (or a RangeError or TypeError).
pub(crate) fn wasm_error(kind: &str, message: impl Into<String>) -> JsError {
    JsError::new(message).with_code("ERR_PI_WASM").with_data("wasmKind", kind)
}

fn trap_error(error: &Error) -> JsError {
    use wasmi::core::TrapCode;
    let Some(code) = error.as_trap_code() else {
        return wasm_error("RuntimeError", error.to_string());
    };
    let message = match code {
        TrapCode::UnreachableCodeReached => "unreachable",
        TrapCode::MemoryOutOfBounds => "memory access out of bounds",
        TrapCode::TableOutOfBounds => "table index is out of bounds",
        TrapCode::IndirectCallToNull | TrapCode::BadSignature => "null function or function signature mismatch",
        TrapCode::IntegerDivisionByZero => "divide by zero",
        TrapCode::IntegerOverflow => "divide result unrepresentable",
        TrapCode::BadConversionToInteger => "float unrepresentable in integer range",
        TrapCode::StackOverflow => return wasm_error("RangeError", "Maximum call stack size exceeded"),
        other => return wasm_error("RuntimeError", other.to_string()),
    };
    wasm_error("RuntimeError", message)
}

pub(crate) fn type_name(ty: ValType) -> &'static str {
    match ty {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        ValType::V128 => "v128",
        ValType::FuncRef => "funcref",
        ValType::ExternRef => "externref",
    }
}

fn to_int32(number: f64) -> i32 {
    if !number.is_finite() {
        return 0;
    }
    (number.trunc().rem_euclid(4294967296.0) as u64) as u32 as i32
}

/// A JavaScript value, already coerced by the caller, as a Wasm value of type `ty`. References
/// arrive as ids: a function id for funcref, an extern id for externref, or null.
pub(crate) fn to_val(ctx: &Ctx<'_>, wasm: &Wasm, value: &Value<'_>, ty: ValType) -> rquickjs::Result<Val> {
    let number = || value.as_number().unwrap_or(f64::NAN);
    Ok(match ty {
        ValType::I32 => Val::I32(value.as_int().unwrap_or_else(|| to_int32(number()))),
        ValType::I64 => Val::I64(value.as_big_int().map(|big| big.clone().to_i64()).transpose()?.unwrap_or_default()),
        ValType::F32 => Val::F32((number() as f32).into()),
        ValType::F64 => Val::F64(number().into()),
        ValType::FuncRef => match value.as_number() {
            None => Val::FuncRef(FuncRef::null()),
            Some(id) => match wasm.externs.borrow().get(id as u32) {
                Some(Extern::Func(func)) => Val::FuncRef(FuncRef::new(func)),
                _ => return Err(throw(ctx, &JsError::type_error("type incompatibility when transforming from/to JS"))),
            },
        },
        ValType::ExternRef => match value.as_number() {
            None => Val::ExternRef(ExternRef::null()),
            Some(id) => Val::ExternRef(ExternRef::new(&mut *wasm.store.borrow_mut(), id as u64)),
        },
        ValType::V128 => {
            return Err(throw(ctx, &JsError::type_error("type incompatibility when transforming from/to JS")));
        }
    })
}

pub(crate) fn from_val<'js>(ctx: &Ctx<'js>, wasm: &Wasm, val: &Val) -> rquickjs::Result<Value<'js>> {
    Ok(match val {
        Val::I32(number) => Value::new_int(ctx.clone(), *number),
        Val::I64(number) => BigInt::from_i64(ctx.clone(), *number)?.into_value(),
        Val::F32(number) => Value::new_number(ctx.clone(), f64::from(f32::from(*number))),
        Val::F64(number) => Value::new_number(ctx.clone(), f64::from(*number)),
        Val::FuncRef(func) => match func.func() {
            Some(func) => Value::new_int(ctx.clone(), wasm.externs.borrow_mut().id(*func) as i32),
            None => Value::new_null(ctx.clone()),
        },
        Val::ExternRef(reference) => {
            let store = wasm.store.borrow();
            match reference.data(&*store).and_then(|data| data.downcast_ref::<u64>().copied()) {
                Some(id) => Value::new_number(ctx.clone(), id as f64),
                None => Value::new_null(ctx.clone()),
            }
        }
        Val::V128(_) => {
            return Err(throw(ctx, &JsError::type_error("type incompatibility when transforming from/to JS")));
        }
    })
}

fn module_entry<T>(ctx: &Ctx<'_>, wasm: &Wasm, id: u32, f: impl FnOnce(&ModuleEntry) -> T) -> rquickjs::Result<T> {
    let modules = wasm.modules.borrow();
    let entry = modules.get(id as usize).ok_or_else(|| throw(ctx, &JsError::type_error("not a WebAssembly.Module")))?;
    Ok(f(entry))
}

fn validate(bytes: rquickjs::TypedArray<'_, u8>) -> bool {
    Module::validate(&wasm().engine, &bytes_of(&bytes)).is_ok()
}

fn compile(ctx: Ctx<'_>, bytes: rquickjs::TypedArray<'_, u8>) -> rquickjs::Result<u32> {
    let wasm = wasm();
    let bytes = bytes_of(&bytes);
    let compile_error = |error: Error| throw(&ctx, &wasm_error("CompileError", error.to_string()));
    Module::validate(&wasm.engine, &bytes).map_err(compile_error)?;
    let entry = match wasm_objects::prepare_module(&bytes) {
        Some(prepared) => {
            let module = Module::new(&wasm.engine, prepared.bytes).map_err(compile_error)?;
            ModuleEntry { module, has_start: prepared.has_start, exports: prepared.exports, imports: prepared.imports }
        }
        None => {
            let module = Module::new(&wasm.engine, bytes).map_err(compile_error)?;
            let exports = module.exports().map(|export| export.name().to_string()).collect();
            let imports = (0..module.imports().len()).collect();
            ModuleEntry { module, has_start: false, exports, imports }
        }
    };
    let mut modules = wasm.modules.borrow_mut();
    modules.push(entry);
    Ok(modules.len() as u32 - 1)
}

fn kind_name(ty: &ExternType) -> &'static str {
    match ty {
        ExternType::Func(_) => "function",
        ExternType::Table(_) => "table",
        ExternType::Memory(_) => "memory",
        ExternType::Global(_) => "global",
    }
}

/// `[{ module, name, kind, type?, mutable? }]`; globals carry their value type and mutability.
fn module_imports<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Array<'js>> {
    let wasm = wasm();
    let list = Array::new(ctx.clone())?;
    module_entry(&ctx, &wasm, id, |entry| -> rquickjs::Result<()> {
        let imports: Vec<_> = entry.module.imports().collect();
        for (index, import) in entry.imports.iter().map(|at| &imports[*at]).enumerate() {
            let item = Object::new(ctx.clone())?;
            item.set("module", import.module())?;
            item.set("name", import.name())?;
            item.set("kind", kind_name(import.ty()))?;
            if let ExternType::Global(global) = import.ty() {
                item.set("type", type_name(global.content()))?;
                item.set("mutable", global.mutability().is_mut())?;
            }
            list.set(index, item)?;
        }
        Ok(())
    })??;
    Ok(list)
}

fn module_exports<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Array<'js>> {
    let wasm = wasm();
    let list = Array::new(ctx.clone())?;
    module_entry(&ctx, &wasm, id, |entry| -> rquickjs::Result<()> {
        for name in &entry.exports {
            let Some(ty) = entry.module.get_export(name) else { continue };
            let item = Object::new(ctx.clone())?;
            item.set("name", name.as_str())?;
            item.set("kind", kind_name(&ty))?;
            list.set(list.len(), item)?;
        }
        Ok(())
    })??;
    Ok(list)
}

fn custom_sections<'js>(ctx: Ctx<'js>, id: u32, name: String) -> rquickjs::Result<Array<'js>> {
    let wasm = wasm();
    let list = Array::new(ctx.clone())?;
    let sections: Vec<Vec<u8>> = module_entry(&ctx, &wasm, id, |entry| {
        entry
            .module
            .custom_sections()
            .filter(|section| section.name() == name)
            .map(|section| section.data().to_vec())
            .collect()
    })?;
    for (index, data) in sections.into_iter().enumerate() {
        list.set(index, rquickjs::ArrayBuffer::new(ctx.clone(), data)?)?;
    }
    Ok(list)
}

/// Turn one resolved import into an extern: `{ kind: "js", fn }` becomes a host function of the
/// import's type, `{ kind: "value", value }` an immutable global, and others are ids.
fn import_extern<'js>(ctx: &Ctx<'js>, wasm: &Wasm, ty: &ExternType, given: &Object<'js>) -> rquickjs::Result<Extern> {
    let kind: String = given.get("kind")?;
    let prefix: String = given.get("prefix")?;
    let link_error = |message: &str| throw(ctx, &wasm_error("LinkError", format!("{prefix}{message}")));
    match (kind.as_str(), ty) {
        ("js", ExternType::Func(func_type)) => {
            let function: Function = given.get("fn")?;
            let function = keep(ctx, function.into_value());
            let func = Func::new(&mut *wasm.store.borrow_mut(), func_type.clone(), move |_, params, _| {
                Err(Error::host(JsCall { function, params: params.to_vec() }))
            });
            let id = wasm.externs.borrow_mut().id(func);
            wasm.imports.borrow_mut().insert(id, function);
            Ok(Extern::Func(func))
        }
        ("value", ExternType::Global(global_type)) => {
            if global_type.mutability().is_mut() {
                return Err(link_error("imported mutable global must be a WebAssembly.Global object"));
            }
            let value = to_val(ctx, wasm, &given.get("value")?, global_type.content())?;
            Ok(Extern::Global(wasmi::Global::new(&mut *wasm.store.borrow_mut(), value, global_type.mutability())))
        }
        (_, expected) => {
            let item = wasm.externs.borrow().get(given.get("id")?).ok_or_else(|| link_error("unknown import"))?;
            if let (Extern::Func(func), ExternType::Func(func_type)) = (&item, expected) {
                if func.ty(&*wasm.store.borrow()) != *func_type {
                    return Err(link_error("imported function does not match the expected type"));
                }
            }
            Ok(item)
        }
    }
}

fn keep<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> u64 {
    let state = state();
    let id = state.next_id();
    state.refs.borrow_mut().insert(id, Persistent::save(ctx, value));
    id
}

/// Instantiate module `id` with `imports` in import order; returns `{ exports, functions, start }`
/// where exports are `{ name, kind, id }`, functions are `[id, index]`, and start is an id.
fn instantiate<'js>(ctx: Ctx<'js>, id: u32, imports: Array<'js>) -> rquickjs::Result<Object<'js>> {
    let wasm = wasm();
    let (module, has_start, names, order) = module_entry(&ctx, &wasm, id, |entry| {
        (entry.module.clone(), entry.has_start, entry.exports.clone(), entry.imports.clone())
    })?;
    let types: Vec<ExternType> = module.imports().map(|import| import.ty().clone()).collect();
    let mut externs = vec![None; types.len()];
    for (index, at) in order.iter().enumerate() {
        let given: Object = imports.get(index)?;
        externs[*at] = Some(import_extern(&ctx, &wasm, &types[*at], &given)?);
    }
    let externs: Vec<Extern> = externs.into_iter().flatten().collect();
    let instance = Instance::new(&mut *wasm.store.borrow_mut(), &module, &externs).map_err(|error| {
        let kind = if error.as_trap_code().is_some() { "RuntimeError" } else { "LinkError" };
        throw(&ctx, &wasm_error(kind, error.to_string()))
    })?;
    let result = Object::new(ctx.clone())?;
    let exports = Array::new(ctx.clone())?;
    let functions = Array::new(ctx.clone())?;
    let mut listed: Vec<(String, Extern)> = instance
        .exports(&*wasm.store.borrow())
        .filter(|export| export.name().starts_with(HIDDEN))
        .map(|export| (export.name().to_string(), export.into_extern()))
        .collect();
    for name in names {
        let item = instance.get_export(&*wasm.store.borrow(), &name);
        listed.extend(item.map(|item| (name, item)));
    }
    for (name, item) in listed {
        let kind = match item {
            Extern::Func(_) => "function",
            Extern::Table(_) => "table",
            Extern::Memory(_) => "memory",
            Extern::Global(_) => "global",
        };
        let item_id = wasm.externs.borrow_mut().id(item);
        if has_start && name == START_EXPORT {
            result.set("start", item_id)?;
            continue;
        }
        if let Some(index) = name.strip_prefix(HIDDEN).and_then(|name| name.strip_prefix('f')) {
            functions.set(functions.len(), vec![item_id, index.parse().unwrap_or_default()])?;
            continue;
        }
        let entry = Object::new(ctx.clone())?;
        entry.set("name", name)?;
        entry.set("kind", kind)?;
        entry.set("id", item_id)?;
        exports.set(exports.len(), entry)?;
    }
    result.set("exports", exports)?;
    result.set("functions", functions)?;
    Ok(result)
}

fn func_of(ctx: &Ctx<'_>, wasm: &Wasm, id: u32) -> rquickjs::Result<Func> {
    match wasm.externs.borrow().get(id) {
        Some(Extern::Func(func)) => Ok(func),
        _ => Err(throw(ctx, &JsError::type_error("not a WebAssembly function"))),
    }
}

fn func_type<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Object<'js>> {
    let wasm = wasm();
    let ty = func_of(&ctx, &wasm, id)?.ty(&*wasm.store.borrow());
    let result = Object::new(ctx)?;
    result.set("params", ty.params().iter().copied().map(type_name).collect::<Vec<_>>())?;
    result.set("results", ty.results().iter().copied().map(type_name).collect::<Vec<_>>())?;
    Ok(result)
}

/// Run a JavaScript import for a paused call and convert what it returns to `ty`'s results.
fn call_import<'js>(ctx: &Ctx<'js>, wasm: &Wasm, call: &JsCall, ty: &FuncType) -> rquickjs::Result<Vec<Val>> {
    let saved = state().refs.borrow().get(&call.function).cloned();
    let function = saved.ok_or_else(|| throw(ctx, &JsError::new("import is gone")))?.restore(ctx)?;
    let mut args = Vec::with_capacity(call.params.len());
    for param in &call.params {
        args.push(from_val(ctx, wasm, param)?);
    }
    let names = |types: &[ValType]| types.iter().copied().map(type_name).collect::<Vec<_>>();
    let convert: Function = state().internal(ctx)?.get("wasmCallImport")?;
    let results: Array = convert.call((function, args, names(ty.params()), names(ty.results())))?;
    ty.results().iter().enumerate().map(|(index, result)| to_val(ctx, wasm, &results.get(index)?, *result)).collect()
}

/// Call function `id` with coerced `args`; JavaScript imports run while the call is paused.
fn call<'js>(ctx: Ctx<'js>, id: u32, args: Array<'js>) -> rquickjs::Result<Value<'js>> {
    let wasm = wasm();
    let func = func_of(&ctx, &wasm, id)?;
    let ty = func.ty(&*wasm.store.borrow());
    let mut inputs = Vec::with_capacity(ty.params().len());
    for (index, param) in ty.params().iter().enumerate() {
        inputs.push(to_val(&ctx, &wasm, &args.get(index)?, *param)?);
    }
    let mut outputs: Vec<Val> = ty.results().iter().map(|result| Val::default(*result)).collect();
    let import = wasm.imports.borrow().get(&id).copied();
    if let Some(function) = import {
        outputs = call_import(&ctx, &wasm, &JsCall { function, params: inputs }, &ty)?;
    } else {
        let mut step = func.call_resumable(&mut *wasm.store.borrow_mut(), &inputs, &mut outputs);
        loop {
            wasm_objects::refresh_buffers(&ctx, &wasm)?;
            match step {
                Ok(ResumableCall::Finished) => break,
                Ok(ResumableCall::HostTrap(paused)) => {
                    let request = match paused.host_error().downcast_ref::<JsCall>() {
                        Some(request) => JsCall { function: request.function, params: request.params.clone() },
                        None => return Err(throw(&ctx, &trap_error(paused.host_error()))),
                    };
                    let host_ty = paused.host_func().ty(&*wasm.store.borrow());
                    let results = call_import(&ctx, &wasm, &request, &host_ty)?;
                    step = paused.resume(&mut *wasm.store.borrow_mut(), &results, &mut outputs);
                }
                Ok(ResumableCall::OutOfFuel(_)) => return Err(throw(&ctx, &wasm_error("RuntimeError", "out of fuel"))),
                Err(error) => return Err(throw(&ctx, &trap_error(&error))),
            }
        }
    }
    match outputs.len() {
        0 => Ok(Value::new_undefined(ctx.clone())),
        1 => from_val(&ctx, &wasm, &outputs[0]),
        _ => {
            let list = Array::new(ctx.clone())?;
            for (index, output) in outputs.iter().enumerate() {
                list.set(index, from_val(&ctx, &wasm, output)?)?;
            }
            Ok(list.into_value())
        }
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "validate", validate)?;
    set_fn(&object, "compile", compile)?;
    set_fn(&object, "moduleImports", module_imports)?;
    set_fn(&object, "moduleExports", module_exports)?;
    set_fn(&object, "customSections", custom_sections)?;
    set_fn(&object, "instantiate", instantiate)?;
    set_fn(&object, "call", call)?;
    set_fn(&object, "funcType", func_type)?;
    wasm_objects::install(&object)?;
    Ok(object)
}
