//! `WasmInstance` - a live, sandboxed guest module instance.

use godot::builtin::{GString, PackedByteArray, PackedStringArray, Variant, VarArray};
use godot::classes::RefCounted;
use godot::meta::ToGodot;
use godot::obj::{Base, Gd};
use godot::prelude::*;
use wasmtime::Store;

use crate::marshal::{self, StoreData};

/// A live WASM guest execution state.
#[derive(GodotClass)]
#[class(base = RefCounted, init)]
pub struct WasmInstance {
    base: Base<RefCounted>,

    /// `false` if the module trapped, crashed, or was terminated.
    #[var]
    pub is_alive: bool,
    /// Current execution fuel ticks remaining in this step.
    #[var]
    pub fuel_remaining: i64,
    /// Current size of guest linear memory, in bytes.
    #[var(no_set)]
    pub memory_size_bytes: i64,
    /// Peak memory reached during the lifecycle, in bytes.
    #[var(no_set)]
    pub peak_memory_bytes: i64,

    pub(crate) store: Option<Store<StoreData>>,
    pub(crate) guest: Option<wasmtime::Instance>,
    pub(crate) enable_fuel: bool,
}

/// What went wrong during guest execution.
#[derive(Debug)]
pub enum GuestFault {
    Trap(String),
    OutOfFuel,
    OutOfMemory,
    GuestExit(i32),
}

/// Signal events produced by a guest fault, in emission order.
pub(crate) fn fault_events(fault: &GuestFault) -> Vec<(&'static str, Vec<Variant>)> {
    let bt = PackedStringArray::new().to_variant();
    let trapped = |reason: String| {
        vec![
            GString::from(reason.as_str()).to_variant(),
            bt.clone(),
        ]
    };
    match fault {
        GuestFault::OutOfFuel => vec![
            ("out_of_fuel", vec![]),
            ("trapped", trapped("out of fuel".into())),
        ],
        GuestFault::OutOfMemory => vec![
            ("out_of_memory", vec![]),
            ("trapped", trapped("out of memory".into())),
        ],
        GuestFault::GuestExit(code) => vec![(
            "trapped",
            trapped(format!("guest exited with code {code}")),
        )],
        GuestFault::Trap(reason) => vec![("trapped", trapped(reason.clone()))],
    }
}

/// Emit fault signals directly on an object (used when the `WasmInstance`
/// Rust object isn't otherwise borrowed, e.g. right after instantiation).
pub(crate) fn emit_fault_on(obj: &mut Gd<godot::classes::Object>, fault: GuestFault) {
    for (name, args) in fault_events(&fault) {
        obj.emit_signal(name, &args);
    }
}

impl WasmInstance {
    /// Sync the GDScript-visible properties from the wasmtime store.
    fn sync_props(&mut self) {
        if let Some(store) = self.store.as_mut() {
            if self.enable_fuel {
                if let Ok(f) = store.get_fuel() {
                    self.fuel_remaining = f as i64;
                }
            } else {
                self.fuel_remaining = i64::MAX;
            }
            if let Some(mem) = store.data().memory.clone() {
                let size = mem.data(&*store).len() as i64;
                self.memory_size_bytes = size;
                if size > self.peak_memory_bytes {
                    self.peak_memory_bytes = size;
                }
            }
        }
    }

    fn emit_signal(&mut self, name: &str, args: &[Variant]) {
        self.base_mut().emit_signal(name, args);
    }

    /// Emit the appropriate signal(s) for a fault and mark the instance dead.
    fn fault(&mut self, fault: GuestFault) {
        self.is_alive = false;
        self.sync_props();
        for (name, args) in fault_events(&fault) {
            self.emit_signal(name, &args);
        }
    }

    /// Classify a wasmtime error into a guest fault.
    pub(crate) fn classify(err: &wasmtime::Error, store: &Store<StoreData>) -> GuestFault {
        if let Some(exit) = err.downcast_ref::<wasmtime_wasi::I32Exit>() {
            return GuestFault::GuestExit(exit.0);
        }
        if store.data().enable_fuel {
            if let Ok(0) = store.get_fuel() {
                return GuestFault::OutOfFuel;
            }
        }
        let msg = format!("{err:?}").to_lowercase();
        if msg.contains("fuel") {
            return GuestFault::OutOfFuel;
        }
        if msg.contains("out of bounds memory")
            || msg.contains("memory out of bounds")
            || msg.contains("out of memory")
            || msg.contains("oom")
        {
            return GuestFault::OutOfMemory;
        }
        GuestFault::Trap(format!("{err}"))
    }

    /// Invoke an exported guest function. Returns the fault on error.
    fn invoke_guest(&mut self, name: &str, args: &[Variant]) -> Result<Variant, GuestFault> {
        if !self.is_alive {
            return Err(GuestFault::Trap("instance is not alive".into()));
        }
        let (store, guest) = match (self.store.as_mut(), self.guest) {
            (Some(s), Some(g)) => (s, g),
            _ => return Err(GuestFault::Trap("instance not initialized".into())),
        };

        // Apply the GDScript-side fuel budget before running.
        if self.enable_fuel {
            let _ = store.set_fuel(self.fuel_remaining.max(0) as u64);
        }

        let outcome = (|| -> Result<Variant, GuestFault> {
            let func = match guest.get_func(&mut *store, name) {
                Some(f) => f,
                None => {
                    return Err(GuestFault::Trap(format!("guest has no export '{name}'")));
                }
            };
            let ty = func.ty(&*store);
            let results: Vec<_> = ty.results().collect();

            let vals = match marshal::gd_args_to_wasm_vals(&mut *store, &ty, args) {
                Ok(v) => v,
                Err(e) => return Err(GuestFault::Trap(format!("arg marshal: {e}"))),
            };
            let mut out = vec![wasmtime::Val::I32(0); results.len()];

            if let Err(e) = func.call(&mut *store, &vals, &mut out) {
                return Err(Self::classify(&e, store));
            }

            marshal::wasm_results_to_variant(&mut *store, &results, &out)
                .map_err(|e| GuestFault::Trap(format!("result marshal: {e}")))
        })();

        self.sync_props();
        outcome
    }

    /// Called by `WasmRuntime` when a registered host signal fires.
    /// Forwards `(path, args...)` to the guest export `on_host_signal`.
    pub(crate) fn dispatch_host_signal(&mut self, path: &str, args: &VarArray) {
        if !self.is_alive {
            return;
        }
        if !self.has_func_internal("on_host_signal") {
            return;
        }
        let mut call_args: Vec<Variant> = Vec::with_capacity(args.len() + 1);
        call_args.push(GString::from(path).to_variant());
        for a in args.iter_shared() {
            call_args.push(a);
        }
        let arr: VarArray = call_args.into_iter().collect();
        let arg_vec: Vec<Variant> = arr.iter_shared().collect();
        if let Err(fault) = self.invoke_guest("on_host_signal", &arg_vec) {
            self.fault(fault);
        }
    }

    fn has_func_internal(&mut self, name: &str) -> bool {
        match (self.store.as_mut(), self.guest) {
            (Some(s), Some(g)) => g.get_func(&mut *s, name).is_some(),
            _ => false,
        }
    }
}

#[godot_api]
impl WasmInstance {
    /// Checks if the guest exports a given function.
    #[func]
    pub fn has_func(&mut self, function_name: GString) -> bool {
        let name = function_name.to_string();
        self.has_func_internal(&name)
    }

    /// Calls an exported guest function with arbitrary arguments.
    /// Pass `[]` for no arguments. Returns `null` if the guest does not
    /// export `function_name` (the instance stays alive).
    #[func]
    pub fn call_guest(&mut self, function_name: GString, args: VarArray) -> Variant {
        let name = function_name.to_string();
        if !self.has_func_internal(&name) {
            return Variant::nil();
        }
        let arg_vec: Vec<Variant> = args.iter_shared().collect();
        match self.invoke_guest(&name, &arg_vec) {
            Ok(v) => v,
            Err(fault) => {
                self.fault(fault);
                Variant::nil()
            }
        }
    }

    /// Safely calls a guest function; on trap returns `default_fallback`.
    #[func]
    pub fn safe_call_guest(
        &mut self,
        function_name: GString,
        args: VarArray,
        default_fallback: Variant,
    ) -> Variant {
        let name = function_name.to_string();
        if !self.has_func_internal(&name) {
            return default_fallback;
        }
        let arg_vec: Vec<Variant> = args.iter_shared().collect();
        match self.invoke_guest(&name, &arg_vec) {
            Ok(v) => v,
            Err(fault) => {
                self.fault(fault);
                default_fallback
            }
        }
    }

    /// Allocates bytes inside guest memory via the guest's exported
    /// `malloc`/`allocate`. Returns the guest pointer, or -1 on failure.
    #[func]
    pub fn guest_alloc(&mut self, size: i64) -> i64 {
        let Some(store) = self.store.as_mut() else {
            return -1;
        };
        match marshal::guest_alloc(&mut *store, size.max(0) as u32) {
            Ok(p) => {
                self.sync_props();
                p
            }
            Err(_) => -1,
        }
    }

    /// Frees memory in the guest via its exported `free`/`deallocate`.
    #[func]
    pub fn guest_free(&mut self, pointer: i64) {
        if let Some(store) = self.store.as_mut() {
            let _ = marshal::guest_free(&mut *store, pointer);
        }
    }

    /// Reads a UTF-8 string from guest linear memory.
    #[func]
    pub fn read_string(&mut self, pointer: i64, length: i64) -> GString {
        let Some(store) = self.store.as_mut() else {
            return GString::new();
        };
        marshal::mem_read_string(&mut *store, pointer as u32, length as u32)
            .unwrap_or_default()
    }

    /// Writes a string into guest linear memory. Returns bytes written.
    #[func]
    pub fn write_string(&mut self, pointer: i64, value: GString) -> i64 {
        let Some(store) = self.store.as_mut() else {
            return -1;
        };
        match marshal::mem_write_string(&mut *store, pointer as u32, &value.to_string()) {
            Ok(n) => n as i64,
            Err(_) => -1,
        }
    }

    /// Reads a raw byte slice from guest linear memory.
    #[func]
    pub fn read_bytes(&mut self, pointer: i64, length: i64) -> PackedByteArray {
        let Some(store) = self.store.as_mut() else {
            return PackedByteArray::new();
        };
        marshal::mem_read_bytes(&mut *store, pointer as u32, length as u32)
            .unwrap_or_default()
    }

    /// Writes raw bytes into guest linear memory.
    #[func]
    pub fn write_bytes(&mut self, pointer: i64, bytes: PackedByteArray) {
        if let Some(store) = self.store.as_mut() {
            let _ = marshal::mem_write_bytes(&mut *store, pointer as u32, &bytes);
        }
    }

    /// Internal: forward a host signal into the guest's `on_host_signal`.
    #[func]
    pub fn notify_host_signal(&mut self, path: GString, args: VarArray) {
        let p = path.to_string();
        WasmInstance::dispatch_host_signal(self, &p, &args);
    }
}
