//! `WasmRuntime` - engine singleton managing the host API symbol trie and
//! SDK exporters. Registered as a Godot engine singleton via
//! `#[class(singleton)]`, so GDScript calls it as `WasmRuntime.*`.

use godot::builtin::{Callable, GString, PackedByteArray, Signal, StringName, Variant, VarArray, VarDictionary};
use godot::classes::file_access::ModeFlags;
use godot::classes::{FileAccess, Object};
use godot::obj::{Base, Gd};
use godot::prelude::*;

use crate::api::{self, Api, NodeKind};
use crate::export;
use crate::module::{compile_module_bytes, WasmModule};

/// Global autoload / engine singleton.
#[derive(GodotClass)]
#[class(singleton, init, base = Object)]
pub struct WasmRuntime {
    base: Base<Object>,
    /// The registered host API symbol trie.
    pub(crate) api: Api,
    /// Live `WasmInstance`s that receive host-signal notifications.
    instances: Vec<Gd<crate::instance::WasmInstance>>,
}

#[godot_api]
impl IObject for WasmRuntime {}

#[godot_api]
impl WasmRuntime {
    // ------------------------------------------------------------------
    // API registration
    // ------------------------------------------------------------------

    /// Registers a method/callable at an arbitrary dot-delimited path.
    /// Param names/types and return type are reflected from the Callable.
    #[func]
    pub fn register_func(
        &mut self,
        path: GString,
        callable: Callable,
        #[opt(default = "")] docstring: GString,
    ) {
        let (params, ret) = api::reflect_callable(&callable);
        api::register_node(
            &mut self.api,
            &path.to_string(),
            NodeKind::Func {
                callable,
                params,
                ret,
            },
            &docstring.to_string(),
        );
    }

    /// Registers a property variable as a synthetic getter/setter pair.
    #[func]
    pub fn register_var(
        &mut self,
        path: GString,
        target: Gd<Object>,
        property_name: StringName,
        #[opt(default = false)] is_readonly: bool,
        #[opt(default = "")] docstring: GString,
    ) {
        api::register_node(
            &mut self.api,
            &path.to_string(),
            NodeKind::Var {
                target,
                property: property_name,
                readonly: is_readonly,
            },
            &docstring.to_string(),
        );
    }

    /// Registers an enum (a GDScript Dictionary) as zero-cost i32 values.
    #[func]
    pub fn register_enum(
        &mut self,
        path: GString,
        enum_dict: VarDictionary,
        #[opt(default = "")] docstring: GString,
    ) {
        let values = api::enum_dict_to_vec(&enum_dict);
        api::register_node(
            &mut self.api,
            &path.to_string(),
            NodeKind::Enum { values },
            &docstring.to_string(),
        );
    }

    /// Registers an immutable compile-time constant.
    #[func]
    pub fn register_const(
        &mut self,
        path: GString,
        value: Variant,
        #[opt(default = "")] docstring: GString,
    ) {
        api::register_node(
            &mut self.api,
            &path.to_string(),
            NodeKind::Const { value },
            &docstring.to_string(),
        );
    }

    /// Binds a Godot Signal; guest modules receive it via `on_host_signal`.
    #[func]
    pub fn register_signal(
        &mut self,
        path: GString,
        emitter: Gd<Object>,
        signal_name: StringName,
        #[opt(default = "")] docstring: GString,
    ) {
        let path_string = path.to_string();
        let closure_path = path_string.clone();
        let mut runtime_gd = WasmRuntime::singleton();

        // Route the emitter's signal into a Rust closure that notifies all
        // live WasmInstances. `Callable::from_fn` closures are single-threaded
        // and may capture `Gd`s freely.
        let callable = Callable::from_fn(
            format!("wasm_signal:{path_string}"),
            move |args: &[&Variant]| {
                let mut varr = VarArray::new();
                for a in args {
                    varr.push(*a);
                }
                runtime_gd.bind_mut().dispatch_host_signal(&closure_path, &varr);
                Variant::nil()
            },
        );

        let sig = Signal::from_object_signal(&emitter, &signal_name);
        let _ = sig.connect(&callable);

        api::register_node(
            &mut self.api,
            &path_string,
            NodeKind::Signal {
                emitter,
                signal: signal_name,
            },
            &docstring.to_string(),
        );
    }

    /// Explicitly defines a struct/record schema for complex types.
    /// `fields` maps field names to `TYPE_*` int constants or type-name strings.
    #[func]
    pub fn register_struct(
        &mut self,
        path: GString,
        fields: VarDictionary,
        #[opt(default = "")] docstring: GString,
    ) {
        let fields_vec = api::struct_dict_to_fields(&fields);
        api::register_node(
            &mut self.api,
            &path.to_string(),
            NodeKind::Struct {
                fields: fields_vec,
            },
            &docstring.to_string(),
        );
    }

    /// Unregisters an entire namespace branch or single endpoint.
    #[func]
    pub fn unregister(&mut self, path: GString) {
        api::unregister_node(&mut self.api, &path.to_string());
    }

    // ------------------------------------------------------------------
    // Compilation & instantiation
    // ------------------------------------------------------------------

    /// Compile raw WASM bytecode (`.wasm`) or text (`.wat`) into a WasmModule.
    #[func]
    pub fn compile_bytes(&self, bytes: PackedByteArray) -> Option<Gd<WasmModule>> {
        let module = match compile_module_bytes(bytes.as_slice()) {
            Ok(m) => m,
            Err(e) => {
                godot_error!("WasmRuntime: compile failed: {e}");
                return None;
            }
        };
        let mut gd = Gd::<WasmModule>::default();
        gd.bind_mut().set_module(module);
        Some(gd)
    }

    /// Load and compile a `.wasm`/`.wat` file from the Godot VFS
    /// (e.g. `user://mods/mod.wasm`).
    #[func]
    pub fn load_module(&self, path: GString) -> Option<Gd<WasmModule>> {
        let p = path.to_string();
        if !FileAccess::file_exists(p.as_str()) {
            godot_error!("WasmRuntime.load_module: file not found: {p}");
            return None;
        }
        let bytes = FileAccess::get_file_as_bytes(p.as_str());
        self.compile_bytes(bytes)
    }

    // ------------------------------------------------------------------
    // Schema & SDK exporting
    // ------------------------------------------------------------------

    /// Export a WebAssembly Interface Type (WIT) specification.
    #[func]
    pub fn export_wit(&self, package_name: GString) -> GString {
        GString::from(export::export_wit(&self.api, &package_name.to_string()).as_str())
    }

    /// Export WIT to a file in the Godot VFS.
    #[func]
    pub fn export_wit_to_file(&self, file_path: GString, package_name: GString) -> i64 {
        let content = export::export_wit(&self.api, &package_name.to_string());
        write_file(&file_path.to_string(), content.as_bytes())
    }

    /// Export the full API symbol tree as structured JSON.
    #[func]
    pub fn export_json(&self) -> GString {
        GString::from(export::export_json(&self.api).as_str())
    }

    /// Export JSON docs to a file. `pretty` controls indentation.
    #[func]
    pub fn export_json_to_file(&self, file_path: GString, #[opt(default = true)] pretty: bool) -> i64 {
        let content = export::export_json_pretty(&self.api, pretty);
        write_file(&file_path.to_string(), content.as_bytes())
    }

    /// Export TypeScript declaration (.d.ts) stubs.
    #[func]
    pub fn export_typescript_declarations(&self) -> GString {
        GString::from(export::export_typescript(&self.api).as_str())
    }

    /// Export TypeScript declarations to a file.
    #[func]
    pub fn export_typescript_to_file(&self, file_path: GString) -> i64 {
        let content = export::export_typescript(&self.api);
        write_file(&file_path.to_string(), content.as_bytes())
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Notify all live instances of a host signal.
    pub(crate) fn dispatch_host_signal(&mut self, path: &str, args: &godot::builtin::VarArray) {
        self.instances.retain(|inst| inst.is_instance_valid());
        for mut inst in self.instances.clone() {
            inst.bind_mut().dispatch_host_signal(path, args);
        }
    }

    /// Track a new instance for signal dispatch.
    pub(crate) fn track_instance(&mut self, inst: &Gd<crate::instance::WasmInstance>) {
        self.instances.push(inst.clone());
    }
}

fn write_file(path: &str, bytes: &[u8]) -> i64 {
    let Some(mut f) = FileAccess::open(path, ModeFlags::WRITE) else {
        godot_error!("WasmRuntime: cannot open '{path}' for writing");
        return godot::global::Error::FAILED.ord() as i64;
    };
    let buf = PackedByteArray::from(bytes);
    if f.store_buffer(&buf) {
        godot::global::Error::OK.ord() as i64
    } else {
        godot_error!("WasmRuntime: write failed for '{path}'");
        godot::global::Error::FAILED.ord() as i64
    }
}
