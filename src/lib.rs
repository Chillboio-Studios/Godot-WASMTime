//! Godot-WASMTime: a Godot GDExtension implementing the WASM UGC Runtime.
//!
//! Classes:
//! - `WasmRuntime`  - engine singleton (host API registry + SDK exporters)
//! - `WasmModule`   - compiled WASM bytecode (RefCounted)
//! - `WasmInstance` - live sandboxed guest execution state (RefCounted)

mod api;
mod export;
mod instance;
mod marshal;
mod module;
mod runtime;

use godot::init::{gdextension, ExtensionLibrary};

pub use instance::WasmInstance;
pub use module::WasmModule;
pub use runtime::WasmRuntime;

struct WasmTimeExtension;

#[gdextension]
unsafe impl ExtensionLibrary for WasmTimeExtension {}
