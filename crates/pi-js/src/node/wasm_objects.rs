//! WebAssembly memories, tables, and globals for the `WebAssembly` global, and the module rewrite
//! that exports the start function and every function by index under hidden names.
#![allow(unsafe_code)]

use std::rc::Rc;

use rquickjs::{ArrayBuffer, ArrayBufferSource, Ctx, Object, Persistent, Value};
use wasmi::core::ValType;
use wasmi::{Extern, Global, Memory, MemoryType, Mutability, Table, TableType};

use super::set_fn;
use super::wasm::{Wasm, from_val, to_val, type_name, wasm, wasm_error};
use crate::error::JsError;
use crate::runtime::convert::throw;
use crate::runtime::state::state;

/// Hidden exports start with this: `"\0start"` is the start function, and `"\0f<index>"` is a
/// function by index, which names its JavaScript wrapper as V8 does.
pub(crate) const HIDDEN: char = '\u{0}';
pub(crate) const START_EXPORT: &str = "\u{0}start";

fn read_leb(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
    }
    None
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Option<u32> {
    read_leb(bytes, at).and_then(|value| u32::try_from(value).ok())
}

fn write_leb(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

fn skip_limits(payload: &[u8], at: &mut usize) -> Option<()> {
    let flags = *payload.get(*at)?;
    *at += 1;
    read_leb(payload, at)?;
    if flags & 1 != 0 {
        read_leb(payload, at)?;
    }
    Some(())
}

/// The kind of each import in an import section, in order.
fn import_kinds(payload: &[u8]) -> Option<Vec<u8>> {
    let mut at = 0;
    let mut kinds = Vec::new();
    for _ in 0..read_u32(payload, &mut at)? {
        for _ in 0..2 {
            let length = read_u32(payload, &mut at)? as usize;
            at += length;
        }
        let kind = *payload.get(at)?;
        at += 1;
        match kind {
            0 => drop(read_leb(payload, &mut at)?),
            1 => {
                at += 1;
                skip_limits(payload, &mut at)?;
            }
            2 => skip_limits(payload, &mut at)?,
            3 => at += 2,
            _ => return None,
        }
        kinds.push(kind);
    }
    Some(kinds)
}

fn export_entry(section: &mut Vec<u8>, name: &str, index: u32) {
    write_leb(section, name.len() as u32);
    section.extend_from_slice(name.as_bytes());
    section.push(0);
    write_leb(section, index);
}

/// The names in an export section, in order.
fn export_names(payload: &[u8]) -> Option<Vec<String>> {
    let mut at = 0;
    let mut names = Vec::new();
    for _ in 0..read_u32(payload, &mut at)? {
        let length = read_u32(payload, &mut at)? as usize;
        let end = at + length;
        names.push(std::str::from_utf8(payload.get(at..end)?).ok()?.to_string());
        at = end + 1;
        read_leb(payload, &mut at)?;
    }
    Some(names)
}

/// Where a section goes in a module, by section id (custom sections, id 0, can go anywhere).
fn rank(id: u8) -> usize {
    [1, 2, 3, 4, 5, 13, 6, 7, 8, 9, 12, 10, 11].iter().position(|known| *known == id).unwrap_or(usize::MAX)
}

/// A module rewritten by `prepare_module`.
pub(crate) struct Prepared {
    pub bytes: Vec<u8>,
    pub has_start: bool,
    /// Export names in module order (wasmi lists them sorted).
    pub exports: Vec<String>,
    /// For each import in module order, its position in wasmi's list (grouped by kind).
    pub imports: Vec<usize>,
}

/// Rewrite a validated module with the hidden exports, dropping its start section so that
/// instantiation does not run it.
pub(crate) fn prepare_module(bytes: &[u8]) -> Option<Prepared> {
    let mut sections = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let id = bytes[at];
        at += 1;
        let size = read_u32(bytes, &mut at)? as usize;
        let end = at + size;
        sections.push((id, bytes.get(at..end)?));
        at = end;
    }
    let payload = |wanted: u8| sections.iter().find(|(id, _)| *id == wanted).map(|(_, payload)| *payload);
    let start = payload(8).and_then(|payload| read_u32(payload, &mut 0));
    let defined = payload(3).map_or(Some(0), |payload| read_u32(payload, &mut 0))?;
    let kinds = payload(2).map_or(Some(Vec::new()), import_kinds)?;
    let functions = kinds.iter().filter(|kind| **kind == 0).count() as u32 + defined;
    let imports = (0..kinds.len())
        .map(|i| {
            kinds.iter().filter(|kind| **kind < kinds[i]).count()
                + kinds[..i].iter().filter(|kind| **kind == kinds[i]).count()
        })
        .collect();
    let exports = payload(7).unwrap_or(&[0]);
    let names = export_names(exports)?;
    let mut count_at = 0;
    let count = read_u32(exports, &mut count_at)?;
    let mut section = Vec::new();
    write_leb(&mut section, count + functions + u32::from(start.is_some()));
    section.extend_from_slice(&exports[count_at..]);
    for index in 0..functions {
        export_entry(&mut section, &format!("{HIDDEN}f{index}"), index);
    }
    if let Some(start) = start {
        export_entry(&mut section, START_EXPORT, start);
    }
    let mut out = bytes[..8].to_vec();
    let mut placed = false;
    for (id, payload) in sections.iter().filter(|(id, _)| *id != 8) {
        let (id, payload) = match id {
            7 => (7, section.as_slice()),
            _ => (*id, *payload),
        };
        if !placed && (id == 7 || (id != 0 && rank(id) > rank(7))) {
            placed = true;
            if id != 7 {
                push_section(&mut out, 7, &section);
            }
        }
        push_section(&mut out, id, payload);
    }
    if !placed {
        push_section(&mut out, 7, &section);
    }
    Some(Prepared { bytes: out, has_start: start.is_some(), exports: names, imports })
}

fn push_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    write_leb(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

/// A memory's bytes viewed by a JavaScript ArrayBuffer, keeping the store alive.
struct WasmBytes {
    ptr: *mut u8,
    len: usize,
    _wasm: Rc<Wasm>,
}

// SAFETY: the bytes stay valid while the store lives (the Rc keeps it), and `refresh_buffers`
// detaches the ArrayBuffer before JavaScript can run after the memory moves or grows.
unsafe impl ArrayBufferSource for WasmBytes {
    fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

fn memory_of(ctx: &Ctx<'_>, wasm: &Wasm, id: u32) -> rquickjs::Result<Memory> {
    match wasm.externs.borrow().get(id) {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(throw(ctx, &JsError::type_error("not a WebAssembly.Memory"))),
    }
}

fn detach(ctx: &Ctx<'_>, reference: u64) -> rquickjs::Result<()> {
    let saved = state().refs.borrow_mut().remove(&reference);
    if let Some(saved) = saved {
        if let Some(mut buffer) = ArrayBuffer::from_value(saved.restore(ctx)?) {
            buffer.detach();
        }
    }
    Ok(())
}

/// Detach the buffers of memories that moved or grew, so stale views read nothing.
pub(crate) fn refresh_buffers(ctx: &Ctx<'_>, wasm: &Wasm) -> rquickjs::Result<()> {
    let stale: Vec<(u32, u64)> = {
        let store = wasm.store.borrow();
        let externs = wasm.externs.borrow();
        wasm.buffers
            .borrow()
            .iter()
            .filter(|(id, (_, ptr, len))| match externs.get(**id) {
                Some(Extern::Memory(memory)) => {
                    memory.data_ptr(&*store) as usize != *ptr || memory.data_size(&*store) != *len
                }
                _ => true,
            })
            .map(|(id, (reference, _, _))| (*id, *reference))
            .collect()
    };
    for (id, reference) in stale {
        wasm.buffers.borrow_mut().remove(&id);
        detach(ctx, reference)?;
    }
    Ok(())
}

fn limit(value: Option<f64>) -> Option<u32> {
    value.filter(|value| *value >= 0.0).map(|value| value as u32)
}

fn memory_new(ctx: Ctx<'_>, initial: f64, maximum: Option<f64>) -> rquickjs::Result<u32> {
    let wasm = wasm();
    let ty = MemoryType::new(initial as u32, limit(maximum));
    let memory = Memory::new(&mut *wasm.store.borrow_mut(), ty)
        .map_err(|_| throw(&ctx, &wasm_error("RangeError", "WebAssembly.Memory(): could not allocate memory")))?;
    Ok(wasm.externs.borrow_mut().id(memory))
}

fn memory_buffer<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Value<'js>> {
    let wasm = wasm();
    let memory = memory_of(&ctx, &wasm, id)?;
    refresh_buffers(&ctx, &wasm)?;
    let existing = wasm.buffers.borrow().get(&id).map(|(reference, _, _)| *reference);
    if let Some(saved) = existing.and_then(|reference| state().refs.borrow().get(&reference).cloned()) {
        return saved.restore(&ctx);
    }
    let (ptr, len) = {
        let store = wasm.store.borrow();
        (memory.data_ptr(&*store), memory.data_size(&*store))
    };
    let buffer = ArrayBuffer::from_source(ctx.clone(), WasmBytes { ptr, len, _wasm: wasm.clone() })?.into_value();
    let state = state();
    let reference = state.next_id();
    state.refs.borrow_mut().insert(reference, Persistent::save(&ctx, buffer.clone()));
    wasm.buffers.borrow_mut().insert(id, (reference, ptr as usize, len));
    Ok(buffer)
}

fn memory_grow(ctx: Ctx<'_>, id: u32, delta: f64) -> rquickjs::Result<f64> {
    let wasm = wasm();
    let memory = memory_of(&ctx, &wasm, id)?;
    let old = memory.grow(&mut *wasm.store.borrow_mut(), delta as u64).map_err(|_| {
        throw(&ctx, &wasm_error("RangeError", "WebAssembly.Memory.grow(): Maximum memory size exceeded"))
    })?;
    let reference = wasm.buffers.borrow_mut().remove(&id).map(|(reference, _, _)| reference);
    if let Some(reference) = reference {
        detach(&ctx, reference)?;
    }
    Ok(old as f64)
}

/// `{ minimum, maximum, size, element? }` of a memory or table.
fn limits<'js>(ctx: &Ctx<'js>, minimum: u64, maximum: Option<u64>, size: u64) -> rquickjs::Result<Object<'js>> {
    let info = Object::new(ctx.clone())?;
    info.set("minimum", minimum as f64)?;
    info.set("maximum", maximum.map(|max| max as f64))?;
    info.set("size", size as f64)?;
    Ok(info)
}

fn memory_info<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Object<'js>> {
    let wasm = wasm();
    let memory = memory_of(&ctx, &wasm, id)?;
    let store = wasm.store.borrow();
    let ty = memory.ty(&*store);
    limits(&ctx, ty.minimum(), ty.maximum(), memory.size(&*store))
}

fn table_of(ctx: &Ctx<'_>, wasm: &Wasm, id: u32) -> rquickjs::Result<Table> {
    match wasm.externs.borrow().get(id) {
        Some(Extern::Table(table)) => Ok(table),
        _ => Err(throw(ctx, &JsError::type_error("not a WebAssembly.Table"))),
    }
}

fn element_type(name: &str) -> ValType {
    if name == "externref" { ValType::ExternRef } else { ValType::FuncRef }
}

fn table_new(
    ctx: Ctx<'_>,
    element: String,
    initial: f64,
    maximum: Option<f64>,
    init: Value<'_>,
) -> rquickjs::Result<u32> {
    let wasm = wasm();
    let ty = element_type(&element);
    let init = to_val(&ctx, &wasm, &init, ty)?;
    let table = Table::new(&mut *wasm.store.borrow_mut(), TableType::new(ty, initial as u32, limit(maximum)), init)
        .map_err(|_| throw(&ctx, &wasm_error("RangeError", "WebAssembly.Table(): could not allocate table")))?;
    Ok(wasm.externs.borrow_mut().id(table))
}

fn table_get<'js>(ctx: Ctx<'js>, id: u32, index: f64) -> rquickjs::Result<Value<'js>> {
    let wasm = wasm();
    let table = table_of(&ctx, &wasm, id)?;
    let value = table.get(&*wasm.store.borrow(), index as u64);
    match value {
        Some(value) => from_val(&ctx, &wasm, &value),
        None => Err(throw(&ctx, &wasm_error("RangeError", "table index is out of bounds"))),
    }
}

fn table_set(ctx: Ctx<'_>, id: u32, index: f64, value: Value<'_>) -> rquickjs::Result<()> {
    let wasm = wasm();
    let table = table_of(&ctx, &wasm, id)?;
    let ty = table.ty(&*wasm.store.borrow()).element();
    let value = to_val(&ctx, &wasm, &value, ty)?;
    let result = table.set(&mut *wasm.store.borrow_mut(), index as u64, value);
    result.map_err(|_| throw(&ctx, &wasm_error("RangeError", "table index is out of bounds")))
}

fn table_grow(ctx: Ctx<'_>, id: u32, delta: f64, init: Value<'_>) -> rquickjs::Result<f64> {
    let wasm = wasm();
    let table = table_of(&ctx, &wasm, id)?;
    let ty = table.ty(&*wasm.store.borrow()).element();
    let init = to_val(&ctx, &wasm, &init, ty)?;
    let old = table.grow(&mut *wasm.store.borrow_mut(), delta as u64, init);
    old.map(|old| old as f64)
        .map_err(|_| throw(&ctx, &wasm_error("RangeError", "WebAssembly.Table.grow(): failed to grow table")))
}

fn table_info<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Object<'js>> {
    let wasm = wasm();
    let table = table_of(&ctx, &wasm, id)?;
    let store = wasm.store.borrow();
    let ty = table.ty(&*store);
    let info = limits(&ctx, ty.minimum(), ty.maximum(), table.size(&*store))?;
    info.set("element", type_name(ty.element()))?;
    Ok(info)
}

fn global_of(ctx: &Ctx<'_>, wasm: &Wasm, id: u32) -> rquickjs::Result<Global> {
    match wasm.externs.borrow().get(id) {
        Some(Extern::Global(global)) => Ok(global),
        _ => Err(throw(ctx, &JsError::type_error("not a WebAssembly.Global"))),
    }
}

fn value_type(name: &str) -> ValType {
    match name {
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "anyfunc" | "funcref" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        "v128" => ValType::V128,
        _ => ValType::I32,
    }
}

fn global_new(ctx: Ctx<'_>, ty: String, mutable: bool, value: Value<'_>) -> rquickjs::Result<u32> {
    let wasm = wasm();
    let value = to_val(&ctx, &wasm, &value, value_type(&ty))?;
    let mutability = if mutable { Mutability::Var } else { Mutability::Const };
    let global = Global::new(&mut *wasm.store.borrow_mut(), value, mutability);
    Ok(wasm.externs.borrow_mut().id(global))
}

fn global_get<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Value<'js>> {
    let wasm = wasm();
    let value = global_of(&ctx, &wasm, id)?.get(&*wasm.store.borrow());
    from_val(&ctx, &wasm, &value)
}

fn global_set(ctx: Ctx<'_>, id: u32, value: Value<'_>) -> rquickjs::Result<()> {
    let wasm = wasm();
    let global = global_of(&ctx, &wasm, id)?;
    let ty = global.ty(&*wasm.store.borrow()).content();
    let value = to_val(&ctx, &wasm, &value, ty)?;
    let result = global.set(&mut *wasm.store.borrow_mut(), value);
    result.map_err(|error| throw(&ctx, &JsError::type_error(error.to_string())))
}

fn global_info<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<Object<'js>> {
    let wasm = wasm();
    let ty = global_of(&ctx, &wasm, id)?.ty(&*wasm.store.borrow());
    let info = Object::new(ctx)?;
    info.set("value", type_name(ty.content()))?;
    info.set("mutable", ty.mutability().is_mut())?;
    Ok(info)
}

pub(crate) fn install(object: &Object<'_>) -> rquickjs::Result<()> {
    set_fn(object, "memoryNew", memory_new)?;
    set_fn(object, "memoryBuffer", memory_buffer)?;
    set_fn(object, "memoryGrow", memory_grow)?;
    set_fn(object, "memoryInfo", memory_info)?;
    set_fn(object, "tableNew", table_new)?;
    set_fn(object, "tableGet", table_get)?;
    set_fn(object, "tableSet", table_set)?;
    set_fn(object, "tableGrow", table_grow)?;
    set_fn(object, "tableInfo", table_info)?;
    set_fn(object, "globalNew", global_new)?;
    set_fn(object, "globalGet", global_get)?;
    set_fn(object, "globalSet", global_set)?;
    set_fn(object, "globalInfo", global_info)?;
    Ok(())
}
