//! `WasmModule` - compiled WASM bytecode resource, plus the instantiation
//! engine (shared `Engine`, `Linker` construction, host-function closures).

use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use godot::builtin::{GString, PackedByteArray, VarDictionary, VariantType};
use godot::classes::RefCounted;
use godot::obj::{Base, Gd, InstanceId};
use godot::prelude::*;
use wasmtime::{
    AsContextMut, Caller, Config, Engine, Extern, ExternType, FuncType, Instance, Linker,
    Memory, MemoryType, Module, Store, StoreLimitsBuilder, Val,
};

use crate::api::{self, ResolvedTarget};
use crate::instance::{GuestFault, WasmInstance};
use crate::marshal::StoreData;
use crate::runtime::WasmRuntime;

/// Shared wasmtime engine for all compiled modules. Fuel metering is enabled
/// engine-wide; per-instance `enable_fuel` selects the budget (or `u64::MAX`).
fn shared_engine() -> Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.consume_fuel(true);
            // Cranelift is the default optimizing compiler.
            Engine::new(&config).expect("failed to create wasmtime engine")
        })
        .clone()
}

/// Compiled, validated WASM machine code. Can be shared across multiple
/// isolated `WasmInstance`s.
#[derive(GodotClass)]
#[class(base = RefCounted, init)]
pub struct WasmModule {
    base: Base<RefCounted>,
    module: Option<Module>,
    /// Original byte source, kept for error messages / re-export.
    #[allow(dead_code)]
    source_name: GString,
}

/// Compile WASM/WAT bytes with the shared engine.
pub(crate) fn compile_module_bytes(bytes: &[u8]) -> Result<Module> {
    Module::new(&shared_engine(), bytes).map_err(|e| anyhow!("compile failed: {e}"))
}

impl WasmModule {
    pub(crate) fn set_module(&mut self, module: Module) {
        self.module = Some(module);
    }
}

#[godot_api]
impl WasmModule {
    /// Compile raw WASM bytecode (`.wasm`) or text format (`.wat`).
    /// Returns `null` on failure.
    #[func]
    pub fn compile(&mut self, bytes: PackedByteArray) -> Option<Gd<WasmModule>> {
        match compile_module_bytes(bytes.as_slice()) {
            Ok(module) => {
                let mut gd = Gd::<WasmModule>::default();
                gd.bind_mut().set_module(module);
                Some(gd)
            }
            Err(e) => {
                godot_error!("WasmModule: compile failed: {e}");
                None
            }
        }
    }

    /// Create an isolated execution sandbox instance.
    /// Pass `{}` for default configuration.
    #[func]
    pub fn instantiate(&self, config: VarDictionary) -> Gd<WasmInstance> {
        let mut inst = Gd::<WasmInstance>::default();
        match self.try_instantiate(&config) {
            Ok(bundle) => {
                let pending_fault = bundle.pending_fault;
                {
                    let mut b = inst.bind_mut();
                    b.store = Some(bundle.store);
                    b.guest = Some(bundle.instance);
                    b.is_alive = bundle.alive;
                    b.enable_fuel = bundle.enable_fuel;
                    b.fuel_remaining = bundle.fuel_remaining;
                    b.memory_size_bytes = bundle.memory_size;
                    b.peak_memory_bytes = bundle.memory_size;
                }
                if let Some(fault) = pending_fault {
                    let mut obj: Gd<godot::classes::Object> = inst.clone().upcast();
                    crate::instance::emit_fault_on(&mut obj, fault);
                }
                WasmRuntime::singleton().bind_mut().track_instance(&inst);
            }
            Err(e) => {
                godot_error!("WasmModule.instantiate failed: {e}");
                inst.bind_mut().is_alive = false;
            }
        }
        inst
    }

    fn try_instantiate(&self, config: &VarDictionary) -> Result<InstanceBundle> {
        let module = self
            .module
            .clone()
            .ok_or_else(|| anyhow!("WasmModule has no compiled module"))?;
        let engine = shared_engine();

        // ---- config ----
        let cfg_i64 = |key: &str, default: i64| -> i64 {
            let v = config.get_or_nil(&key.to_variant());
            if v.get_type() == VariantType::NIL {
                default
            } else {
                v.to::<i64>()
            }
        };
        let cfg_bool = |key: &str, default: bool| -> bool {
            let v = config.get_or_nil(&key.to_variant());
            if v.get_type() == VariantType::NIL {
                default
            } else {
                v.to::<bool>()
            }
        };

        let max_memory_mb = cfg_i64("max_memory_mb", 64).clamp(1, 16384);
        let enable_fuel = cfg_bool("enable_fuel", true);
        let initial_fuel = cfg_i64("initial_fuel", 1_000_000).max(0) as u64;
        let allow_wasi = cfg_bool("allow_wasi", false);

        // ---- store ----
        let mem_cap = (max_memory_mb as usize)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow!("max_memory_mb too large"))?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(mem_cap)
            .instances(1)
            .memories(1)
            .tables(1)
            .trap_on_grow_failure(true)
            .build();

        let mut store = Store::new(
            &engine,
            StoreData {
                memory: None,
                instance: None,
                limits,
                wasi_p1: None,
                enable_fuel,
            },
        );
        store.limiter(|s| &mut s.limits);
        let fuel_budget = if enable_fuel { initial_fuel } else { u64::MAX };
        store
            .set_fuel(fuel_budget)
            .map_err(|e| anyhow!("set_fuel: {e}"))?;

        // ---- linker + host imports ----
        let mut linker = Linker::<StoreData>::new(&engine);
        let runtime_id = WasmRuntime::singleton().instance_id();

        let mut imported_memory: Option<Memory> = None;

        for import in module.imports() {
            match import.ty() {
                ExternType::Func(ft) => {
                    // Validate resolvability up-front so bad imports fail at
                    // instantiation, not on first call.
                    let resolved = resolve_import(runtime_id, import.module(), import.name());
                    if resolved.is_none() {
                        bail!(
                            "unresolved host import '{}.{}'",
                            import.module(),
                            import.name()
                        );
                    }
                    let closure = make_host_closure(
                        runtime_id,
                        import.module().to_string(),
                        import.name().to_string(),
                        ft.clone(),
                    );
                    linker
                        .func_new(import.module(), import.name(), ft.clone(), closure)
                        .map_err(|e| anyhow!("func_new '{}.{}': {e}", import.module(), import.name()))?;
                }
                ExternType::Memory(mt) => {
                    let mem = create_import_memory(&mut store, &mt)?;
                    linker
                        .define(&store, import.module(), import.name(), Extern::Memory(mem))
                        .map_err(|e| anyhow!("define memory '{}.{}': {e}", import.module(), import.name()))?;
                    imported_memory = Some(mem);
                }
                other => {
                    bail!("unsupported guest import type: {other:?}");
                }
            }
        }

        // ---- WASI (opt-in) ----
        if allow_wasi {
            let wasi = wasmtime_wasi::WasiCtxBuilder::new()
                .inherit_stdio()
                .build_p1();
            store.data_mut().wasi_p1 = Some(wasi);
            wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |d: &mut StoreData| {
                d.wasi_p1.as_mut().expect("wasi ctx missing")
            })
            .map_err(|e| anyhow!("wasi linker: {e}"))?;
        }

        // ---- instantiate ----
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| anyhow!("instantiation failed: {e}"))?;
        store.data_mut().instance = Some(instance);
        if let Some(m) = instance.get_memory(&mut store, "memory") {
            store.data_mut().memory = Some(m);
        } else if let Some(m) = imported_memory {
            store.data_mut().memory = Some(m);
        }

        // ---- start function ----
        let mut alive = true;
        let mut pending_fault: Option<GuestFault> = None;
        for start_name in ["_start", "_initialize"] {
            if let Some(f) = instance.get_func(&mut store, start_name) {
                let ty = f.ty(&store);
                let mut out = vec![Val::I32(0); ty.results().len()];
                if let Err(e) = f.call(&mut store, &[], &mut out) {
                    pending_fault = Some(WasmInstance::classify(&e, &store));
                    alive = false;
                }
                break;
            }
        }

        let fuel_remaining = if enable_fuel {
            store.get_fuel().unwrap_or(0) as i64
        } else {
            i64::MAX
        };
        let memory_size = store
            .data()
            .memory
            .as_ref()
            .map(|m| m.data(&store).len() as i64)
            .unwrap_or(0);

        Ok(InstanceBundle {
            store,
            instance,
            alive,
            pending_fault,
            enable_fuel,
            fuel_remaining,
            memory_size,
        })
    }
}

struct InstanceBundle {
    store: Store<StoreData>,
    instance: Instance,
    alive: bool,
    pending_fault: Option<GuestFault>,
    enable_fuel: bool,
    fuel_remaining: i64,
    memory_size: i64,
}

/// Create a memory for an imported-memory guest, clamped to our sandbox cap.
fn create_import_memory(
    store: &mut Store<StoreData>,
    mt: &wasmtime::MemoryType,
) -> Result<Memory> {
    if mt.is_shared() {
        bail!("shared memory imports are not supported");
    }
    let min = mt.minimum();
    let max = mt.maximum();
    let mem_ty = if mt.is_64() {
        MemoryType::new64(min, max).map_err(|e| anyhow!("{e}"))?
    } else {
        MemoryType::new(
            u32::try_from(min).map_err(|_| anyhow!("imported memory minimum too large"))?,
            max.map(|m| u32::try_from(m).unwrap_or(u32::MAX)),
        )
        .map_err(|e| anyhow!("{e}"))?
    };
    Memory::new(store, mem_ty).map_err(|e| anyhow!("imported memory: {e}"))
}

// ---------------------------------------------------------------------------
// Host import resolution + closures
// ---------------------------------------------------------------------------

/// Candidate dot-paths for a guest import `(module, name)`.
fn import_candidates(module: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if name.contains('.') {
        out.push(name.to_string());
    } else {
        // module-scoped style: `playersapi` / `get_count`
        if !matches!(module, "host" | "env" | "game" | "wasi_snapshot_preview1" | "") {
            out.push(format!("{module}.{name}"));
        }
        out.push(name.to_string());
    }
    out
}

/// Resolve a guest import to a registered host target, consulting the live
/// `WasmRuntime` API trie. Supports dot-paths, module-scoped names and
/// kebab-case (WIT-style) flattened names.
fn resolve_import(
    runtime_id: InstanceId,
    module: &str,
    name: &str,
) -> Option<ResolvedTarget> {
    let runtime = Gd::<WasmRuntime>::try_from_instance_id(runtime_id).ok()?;
    let api = &runtime.bind().api;

    for cand in import_candidates(module, name) {
        if let Some(t) = api::resolve(api, &cand) {
            return Some(t);
        }
        // Kebab-case fallback: `playersapi-get-count` -> `playersapi.get_count`.
        let kebab = cand.replace('.', "-");
        if let Some((dot, is_set)) = api::kebab_to_dot_path(&api.root, &kebab) {
            let path = if is_set {
                format!("{dot}.set")
            } else {
                dot
            };
            if let Some(t) = api::resolve(api, &path) {
                return Some(t);
            }
        }
    }
    None
}

/// Closure invoked by wasmtime when the guest calls a host import.
/// Everything captured is `Send + Sync` (plain data only); Godot objects are
/// re-resolved through the runtime on each call.
fn make_host_closure(
    runtime_id: InstanceId,
    import_module: String,
    import_name: String,
    ty: FuncType,
) -> impl Fn(Caller<'_, StoreData>, &[Val], &mut [Val]) -> wasmtime::Result<()> + Send + Sync + 'static
{
    move |mut caller: Caller<'_, StoreData>, params: &[Val], results: &mut [Val]| {
        let Some(target) = resolve_import(runtime_id, &import_module, &import_name) else {
            return Err(wasmtime::Error::msg(format!(
                "host import '{import_module}.{import_name}' no longer registered"
            )));
        };

        // We need &mut access to the store for memory ops; Caller provides it.
        let mut cx = caller.as_context_mut();
        let result_types: Vec<wasmtime::ValType> = ty.results().collect();

        match target {
            ResolvedTarget::Func {
                callable,
                params: hints,
                ret: _,
            } => {
                let gargs = wasm_params_to_gd(&mut cx, params, &hints)
                    .map_err(wasmtime::Error::from_anyhow)?;
                let arr: godot::builtin::VarArray = gargs.into_iter().collect();
                let ret = callable.callv(&arr);
                crate::marshal::variant_to_results(&mut cx, &ret, &result_types, results)
                    .map_err(wasmtime::Error::from_anyhow)
            }
            ResolvedTarget::VarGet { target, property } => {
                let v = target.get(&property);
                crate::marshal::variant_to_results(&mut cx, &v, &result_types, results)
                    .map_err(wasmtime::Error::from_anyhow)
            }
            ResolvedTarget::VarSet {
                mut target,
                property,
                readonly,
            } => {
                if readonly {
                    return Err(wasmtime::Error::msg(format!(
                        "host variable '{import_name}' is read-only"
                    )));
                }
                let gargs = wasm_params_to_gd(&mut cx, params, &[])
                    .map_err(wasmtime::Error::from_anyhow)?;
                if let Some(v) = gargs.first() {
                    target.set(&property, v);
                }
                for (o, t) in results.iter_mut().zip(result_types.iter()) {
                    *o = match t {
                        wasmtime::ValType::I32 => Val::I32(0),
                        wasmtime::ValType::I64 => Val::I64(0),
                        wasmtime::ValType::F32 => Val::F32(0f32.to_bits()),
                        wasmtime::ValType::F64 => Val::F64(0f64.to_bits()),
                        _ => Val::I32(0),
                    };
                }
                Ok(())
            }
            ResolvedTarget::Const { value } => {
                crate::marshal::variant_to_results(&mut cx, &value, &result_types, results)
                    .map_err(wasmtime::Error::from_anyhow)
            }
            ResolvedTarget::EnumValue { value } => {
                for (o, t) in results.iter_mut().zip(result_types.iter()) {
                    *o = match t {
                        wasmtime::ValType::I32 => Val::I32(value as i32),
                        wasmtime::ValType::I64 => Val::I64(value),
                        wasmtime::ValType::F32 => Val::F32((value as f32).to_bits()),
                        wasmtime::ValType::F64 => Val::F64((value as f64).to_bits()),
                        _ => Val::I32(value as i32),
                    };
                }
                Ok(())
            }
        }
    }
}

/// Convert wasm call params to GDScript args, honoring reflection hints so
/// that `(i32 ptr, i32 len)` pairs become `String`s.
fn wasm_params_to_gd<C>(
    cx: &mut C,
    params: &[Val],
    hints: &[api::ParamInfo],
) -> Result<Vec<Variant>>
where
    C: AsContextMut<Data = StoreData>,
{
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut hidx = 0usize;
    while i < params.len() {
        let hint = hints.get(hidx).map(|h| h.ty);
        hidx += 1;

        // String hint + two consecutive i32s -> read guest string.
        if hint == Some(api::TypeTag::String)
            && i + 1 < params.len()
            && matches!(params[i], Val::I32(_))
            && matches!(params[i + 1], Val::I32(_))
        {
            let ptr = match params[i] {
                Val::I32(v) => v as u32,
                _ => 0,
            };
            let len = match params[i + 1] {
                Val::I32(v) => v as u32,
                _ => 0,
            };
            let s = crate::marshal::mem_read_string(&mut *cx, ptr, len)
                .map_err(|e| anyhow!("host string arg: {e}"))?;
            out.push(s.to_variant());
            i += 2;
            continue;
        }
        // String hint + single i64 -> packed (ptr << 32) | len.
        if hint == Some(api::TypeTag::String) && matches!(params[i], Val::I64(_)) {
            if let Val::I64(packed) = params[i] {
                let ptr = (packed >> 32) as u32;
                let len = (packed & 0xffff_ffff) as u32;
                let s = crate::marshal::mem_read_string(&mut *cx, ptr, len)
                    .map_err(|e| anyhow!("host string arg: {e}"))?;
                out.push(s.to_variant());
            }
            i += 1;
            continue;
        }

        let v = crate::marshal::val_to_variant(&params[i])?;
        // Coerce to hinted bool.
        if hint == Some(api::TypeTag::Bool) {
            if let VariantType::INT = v.get_type() {
                out.push((v.to::<i64>() != 0).to_variant());
                i += 1;
                continue;
            }
        }
        out.push(v);
        i += 1;
    }
    Ok(out)
}
