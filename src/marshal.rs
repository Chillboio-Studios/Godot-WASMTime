//! Marshaling between wasmtime `Val`s and GDScript `Variant`s, plus guest
//! linear-memory helpers (string/bytes read/write, guest alloc).
//!
//! All context-taking helpers are generic over `impl AsContextMut<Data =
//! StoreData>` so they work identically with a `Store` (host-side calls like
//! `call_guest`) and a `Caller` (inside host-function closures invoked by the
//! guest).

use anyhow::{anyhow, bail, Result};
use wasmtime::{AsContextMut, Func, Memory, Val, ValType};

use godot::builtin::{GString, PackedByteArray, Variant, VarArray};
use godot::prelude::*;

/// Store-local state shared by a guest instance.
pub struct StoreData {
    /// Guest linear memory (exported by the guest, or the memory we provided
    /// for imported-memory modules). Set right after instantiation.
    pub memory: Option<Memory>,
    /// The instantiated guest module handle (Copy), used to find allocator
    /// exports for string passing.
    pub instance: Option<wasmtime::Instance>,
    /// Resource limits enforced by the store limiter.
    pub limits: wasmtime::StoreLimits,
    /// WASIp1 context, present only when `allow_wasi` was requested.
    pub wasi_p1: Option<wasmtime_wasi::p1::WasiP1Ctx>,
    /// Whether fuel metering is enabled for this instance.
    pub enable_fuel: bool,
}

// ---------------------------------------------------------------------------
// Guest memory access
// ---------------------------------------------------------------------------

fn guest_memory<C>(ctx: &mut C) -> Result<Memory>
where
    C: AsContextMut<Data = StoreData>,
{
    // Prefer the live `memory` export (covers re-exports), then the memory
    // recorded in store data (covers imported memories).
    if let Some(inst) = ctx.as_context().data().instance {
        if let Some(m) = inst.get_memory(&mut *ctx, "memory") {
            return Ok(m);
        }
    }
    ctx.as_context()
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest module does not provide linear memory"))
}

/// Read `len` bytes at `ptr` from guest memory.
pub fn mem_read(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let mem = guest_memory(&mut ctx)?;
    let data = mem.data(&ctx);
    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or_else(|| anyhow!("bad range"))?;
    if end > data.len() {
        bail!("guest memory read out of bounds: {ptr}+{len} > {}", data.len());
    }
    Ok(data[start..end].to_vec())
}

/// Write bytes at `ptr` in guest memory.
pub fn mem_write(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, bytes: &[u8]) -> Result<()> {
    let mem = guest_memory(&mut ctx)?;
    let data = mem.data_mut(&mut ctx);
    let start = ptr as usize;
    let end = start.checked_add(bytes.len()).ok_or_else(|| anyhow!("bad range"))?;
    if end > data.len() {
        bail!("guest memory write out of bounds: {ptr}+{} > {}", bytes.len(), data.len());
    }
    data[start..end].copy_from_slice(bytes);
    Ok(())
}

/// Read a UTF-8 string from guest memory.
pub fn mem_read_string(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, len: u32) -> Result<GString> {
    let bytes = mem_read(&mut ctx, ptr, len)?;
    Ok(GString::from(String::from_utf8_lossy(&bytes).as_ref()))
}

/// Write a string into guest memory at `ptr`; returns bytes written.
pub fn mem_write_string(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, s: &str) -> Result<u32> {
    let bytes = s.as_bytes();
    mem_write(&mut ctx, ptr, bytes)?;
    Ok(bytes.len() as u32)
}

pub fn mem_read_bytes(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, len: u32) -> Result<PackedByteArray> {
    Ok(PackedByteArray::from(&mem_read(&mut ctx, ptr, len)?[..]))
}

pub fn mem_write_bytes(mut ctx: impl AsContextMut<Data = StoreData>, ptr: u32, bytes: &PackedByteArray) -> Result<()> {
    mem_write(&mut ctx, ptr, bytes.as_slice())
}

// ---------------------------------------------------------------------------
// Guest allocation (calls guest-exported malloc/allocate)
// ---------------------------------------------------------------------------

/// Names we probe, in order, for the guest's allocator export.
const ALLOC_NAMES: &[&str] = &["malloc", "allocate", "guest_alloc", "alloc"];
const FREE_NAMES: &[&str] = &["free", "deallocate", "guest_free", "dealloc"];

fn find_named_func<C>(ctx: &mut C, names: &[&str]) -> Option<(String, Func)>
where
    C: AsContextMut<Data = StoreData>,
{
    let inst = ctx.as_context().data().instance?;
    for name in names {
        if let Some(f) = inst.get_func(&mut *ctx, name) {
            return Some((name.to_string(), f));
        }
    }
    None
}

/// Allocate `size` bytes in guest memory via its exported allocator.
/// Returns the guest pointer, or -1 when the guest provides no allocator.
pub fn guest_alloc(mut ctx: impl AsContextMut<Data = StoreData>, size: u32) -> Result<i64> {
    let Some((_, func)) = find_named_func(&mut ctx, ALLOC_NAMES) else {
        return Ok(-1);
    };
    let ty = func.ty(ctx.as_context());
    let params: Vec<ValType> = ty.params().collect();
    let results: Vec<ValType> = ty.results().collect();
    if params.len() != 1 || results.len() != 1 {
        bail!("guest allocator has unexpected signature");
    }
    let arg = match params[0] {
        ValType::I32 => Val::I32(size as i32),
        ValType::I64 => Val::I64(size as i64),
        _ => bail!("guest allocator param must be i32/i64"),
    };
    let mut out = [Val::I32(0)];
    func.call(&mut ctx, &[arg], &mut out)
        .map_err(|e| anyhow!("guest allocator call failed: {e}"))?;
    Ok(match out[0] {
        Val::I32(v) => v as i64,
        Val::I64(v) => v,
        _ => -1,
    })
}

/// Free a guest pointer via its exported deallocator (best-effort).
pub fn guest_free(mut ctx: impl AsContextMut<Data = StoreData>, ptr: i64) -> Result<()> {
    let Some((_, func)) = find_named_func(&mut ctx, FREE_NAMES) else {
        return Ok(());
    };
    let ty = func.ty(ctx.as_context());
    let params: Vec<ValType> = ty.params().collect();
    if params.len() != 1 {
        return Ok(());
    }
    let arg = match params[0] {
        ValType::I32 => Val::I32(ptr as i32),
        ValType::I64 => Val::I64(ptr),
        _ => return Ok(()),
    };
    let mut out: [Val; 0] = [];
    func.call(&mut ctx, &[arg], &mut out)
        .map_err(|e| anyhow!("guest deallocator call failed: {e}"))?;
    Ok(())
}

/// Write a Rust string into freshly guest-allocated memory.
/// Returns (ptr, len).
pub fn write_string_to_guest(mut ctx: impl AsContextMut<Data = StoreData>, s: &str) -> Result<(u32, u32)> {
    let bytes = s.as_bytes();
    let ptr = guest_alloc(&mut ctx, bytes.len() as u32)?;
    if ptr < 0 {
        bail!("guest_alloc failed (guest exports no allocator?)");
    }
    mem_write(&mut ctx, ptr as u32, bytes)?;
    Ok((ptr as u32, bytes.len() as u32))
}

// ---------------------------------------------------------------------------
// wasm Val <-> Variant conversion
// ---------------------------------------------------------------------------

/// Convert a single wasm `Val` to a GDScript `Variant`.
pub fn val_to_variant(val: &Val) -> Result<Variant> {
    Ok(match val {
        Val::I32(v) => Variant::from(i64::from(*v)),
        Val::I64(v) => Variant::from(*v),
        Val::F32(v) => Variant::from(f64::from(f32::from_bits(*v))),
        Val::F64(v) => Variant::from(f64::from_bits(*v)),
        _ => bail!("unsupported wasm value type in host call"),
    })
}

/// Convert a GDScript `Variant` return value into wasm result `Val`s,
/// writing into `out` according to the guest-declared result types.
///
/// String returns are supported as:
/// - `(result i32 i32)`  -> (ptr, len) into guest memory
/// - `(result i64)`      -> packed `(ptr << 32) | len`
/// - `(result i32)`      -> low 32 bits of the same packed value
pub fn variant_to_results(
    mut ctx: impl AsContextMut<Data = StoreData>,
    value: &Variant,
    result_types: &[ValType],
    out: &mut [Val],
) -> Result<()> {
    match value.get_type() {
        VariantType::STRING | VariantType::STRING_NAME | VariantType::NODE_PATH => {
            let s = value.to::<GString>().to_string();
            let (ptr, len) = write_string_to_guest(&mut ctx, &s)?;
            let packed = ((ptr as u64) << 32) | len as u64;
            write_scalar_results(result_types, out, packed as i64, ptr as i64, len as i64)
        }
        VariantType::BOOL => {
            let b = if value.to::<bool>() { 1i64 } else { 0i64 };
            write_scalar_results(result_types, out, b, b, b)
        }
        VariantType::INT => {
            let i = value.to::<i64>();
            write_scalar_results(result_types, out, i, i, i)
        }
        VariantType::FLOAT => {
            let f = value.to::<f64>();
            write_typed_float_results(result_types, out, f)
        }
        VariantType::NIL => {
            for (o, t) in out.iter_mut().zip(result_types.iter()) {
                *o = default_val(t);
            }
            Ok(())
        }
        _ => {
            // Arrays, dictionaries, objects, vectors, etc. are JSON-ish
            // encoded and passed as strings.
            let s = variant_to_jsonish(value);
            let (ptr, len) = write_string_to_guest(&mut ctx, &s)?;
            let packed = ((ptr as u64) << 32) | len as u64;
            write_scalar_results(result_types, out, packed as i64, ptr as i64, len as i64)
        }
    }
}

fn default_val(t: &ValType) -> Val {
    match t {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0f32.to_bits()),
        ValType::F64 => Val::F64(0f64.to_bits()),
        _ => Val::I32(0),
    }
}

fn write_scalar_results(
    result_types: &[ValType],
    out: &mut [Val],
    packed_or_int: i64,
    ptr: i64,
    len: i64,
) -> Result<()> {
    match result_types {
        [] => Ok(()),
        [ValType::I32, ValType::I32] => {
            out[0] = Val::I32(ptr as i32);
            out[1] = Val::I32(len as i32);
            Ok(())
        }
        [ValType::I64] => {
            out[0] = Val::I64(packed_or_int);
            Ok(())
        }
        [ValType::I32] => {
            out[0] = Val::I32(packed_or_int as i32);
            Ok(())
        }
        [t] => {
            out[0] = default_val(t);
            Ok(())
        }
        _ => bail!("unsupported host function result signature"),
    }
}

fn write_typed_float_results(result_types: &[ValType], out: &mut [Val], f: f64) -> Result<()> {
    match result_types {
        [] => Ok(()),
        [ValType::F32] => {
            out[0] = Val::F32((f as f32).to_bits());
            Ok(())
        }
        [ValType::F64] => {
            out[0] = Val::F64(f.to_bits());
            Ok(())
        }
        [t] => {
            out[0] = default_val(t);
            Ok(())
        }
        _ => bail!("unsupported host function float result signature"),
    }
}

/// Best-effort JSON-ish serialization for complex Variant values.
pub fn variant_to_jsonish(v: &Variant) -> String {
    match v.get_type() {
        VariantType::ARRAY => {
            let arr = v.to::<VarArray>();
            let parts: Vec<String> = arr.iter_shared().map(|e| variant_to_jsonish(&e)).collect();
            format!("[{}]", parts.join(","))
        }
        _ => v.to::<GString>().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Guest -> host call marshaling (call_guest direction)
// ---------------------------------------------------------------------------

/// Build wasm argument `Val`s from GDScript arguments, following the guest
/// export's declared signature. String args are written into guest memory and
/// passed as `(ptr, len)` i32 pairs or a packed i64.
pub fn gd_args_to_wasm_vals(
    mut ctx: impl AsContextMut<Data = StoreData>,
    func_ty: &wasmtime::FuncType,
    args: &[Variant],
) -> Result<Vec<Val>> {
    let params: Vec<ValType> = func_ty.params().collect();
    let mut vals: Vec<Val> = Vec::with_capacity(params.len());
    let mut argi = 0usize;

    let mut i = 0usize;
    while i < params.len() {
        let pt = &params[i];
        let arg = args.get(argi).cloned().unwrap_or_else(Variant::nil);
        let is_string_like = matches!(
            arg.get_type(),
            VariantType::STRING
                | VariantType::STRING_NAME
                | VariantType::NODE_PATH
                | VariantType::ARRAY
                | VariantType::DICTIONARY
        );

        if is_string_like {
            let text = if matches!(
                arg.get_type(),
                VariantType::STRING | VariantType::STRING_NAME | VariantType::NODE_PATH
            ) {
                arg.to::<GString>().to_string()
            } else {
                variant_to_jsonish(&arg)
            };
            let (ptr, len) = write_string_to_guest(&mut ctx, &text)?;
            // Two consecutive i32 slots -> (ptr, len); single i64 -> packed.
            if i + 1 < params.len()
                && matches!(params[i], ValType::I32)
                && matches!(params[i + 1], ValType::I32)
            {
                vals.push(Val::I32(ptr as i32));
                vals.push(Val::I32(len as i32));
                i += 2;
            } else if matches!(params[i], ValType::I64) {
                vals.push(Val::I64((((ptr as u64) << 32) | len as u64) as i64));
                i += 1;
            } else {
                vals.push(Val::I32(ptr as i32));
                i += 1;
            }
            argi += 1;
            continue;
        }

        // Numeric coercion.
        match pt {
            ValType::I32 => vals.push(Val::I32(variant_to_i64(&arg) as i32)),
            ValType::I64 => vals.push(Val::I64(variant_to_i64(&arg))),
            ValType::F32 => vals.push(Val::F32((variant_to_f64(&arg) as f32).to_bits())),
            ValType::F64 => vals.push(Val::F64(variant_to_f64(&arg).to_bits())),
            _ => bail!("unsupported wasm param type in guest export"),
        }
        i += 1;
        argi += 1;
    }

    Ok(vals)
}

fn variant_to_i64(v: &Variant) -> i64 {
    match v.get_type() {
        VariantType::INT => v.to::<i64>(),
        VariantType::BOOL => i64::from(v.to::<bool>()),
        VariantType::FLOAT => v.to::<f64>() as i64,
        _ => 0,
    }
}

fn variant_to_f64(v: &Variant) -> f64 {
    match v.get_type() {
        VariantType::FLOAT => v.to::<f64>(),
        VariantType::INT => v.to::<i64>() as f64,
        VariantType::BOOL => {
            if v.to::<bool>() {
                1.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// Convert wasm result `Val`s from a guest call into a GDScript `Variant`.
/// `(i32, i32)` results are read as a (ptr, len) UTF-8 string.
pub fn wasm_results_to_variant(
    mut ctx: impl AsContextMut<Data = StoreData>,
    result_types: &[ValType],
    results: &[Val],
) -> Result<Variant> {
    match result_types {
        [] => Ok(Variant::nil()),
        [ValType::I32, ValType::I32] => {
            if results.len() >= 2 {
                let ptr = match results[0] {
                    Val::I32(v) => v as u32,
                    _ => 0,
                };
                let len = match results[1] {
                    Val::I32(v) => v as u32,
                    _ => 0,
                };
                if ptr == 0 && len == 0 {
                    return Ok(GString::from("").to_variant());
                }
                let s = mem_read_string(&mut ctx, ptr, len).unwrap_or_default();
                Ok(s.to_variant())
            } else {
                Ok(Variant::nil())
            }
        }
        [ValType::I64] => Ok(match results.first() {
            Some(Val::I64(v)) => Variant::from(*v),
            _ => Variant::nil(),
        }),
        [ValType::I32] => Ok(match results.first() {
            Some(Val::I32(v)) => Variant::from(*v as i64),
            _ => Variant::nil(),
        }),
        [ValType::F32] => Ok(match results.first() {
            Some(Val::F32(v)) => Variant::from(f64::from(f32::from_bits(*v))),
            _ => Variant::nil(),
        }),
        [ValType::F64] => Ok(match results.first() {
            Some(Val::F64(v)) => Variant::from(f64::from_bits(*v)),
            _ => Variant::nil(),
        }),
        _ => Ok(match results.first() {
            Some(Val::I32(v)) => Variant::from(i64::from(*v)),
            Some(Val::I64(v)) => Variant::from(*v),
            Some(Val::F32(v)) => Variant::from(f64::from(f32::from_bits(*v))),
            Some(Val::F64(v)) => Variant::from(f64::from_bits(*v)),
            _ => Variant::nil(),
        }),
    }
}
