# Godot-WASMTime

A Rust GDExtension (godot-rust + wasmtime) implementing the **Godot WASM UGC
Runtime**: register arbitrary dot-delimited host APIs from GDScript, execute
sandboxed guest WASM modules, and export SDK specs (WIT / JSON / TypeScript)
for modding toolchains.

## Classes

| Class | Base | Purpose |
| :--- | :--- | :--- |
| `WasmRuntime` | `Object` (engine singleton) | Host API registry + SDK exporters |
| `WasmModule` | `RefCounted` | Compiled WASM bytecode, shareable across instances |
| `WasmInstance` | `RefCounted` | Live sandboxed guest execution state |

`WasmRuntime` is registered as an engine singleton (`#[class(singleton)]`), so
GDScript calls it directly as `WasmRuntime.*` — no autoload needed.

## Building

```sh
cargo build            # debug  -> target/debug/libgodot_wasmtime.so
cargo build --release  # release -> target/release/libgodot_wasmtime.so
```

## Installing into a Godot project

The `.gdextension` file references the Cargo `target/` directory relative to
its own location, so the simplest setup is to keep the whole crate inside the
project's `res://` tree (e.g. `res://OpenSourceModules/Godot-WASMTime/`) or
copy/symlink both the `.gdextension` file and the built `.so` so the relative
paths resolve. For a flat layout, copy:

```sh
cp godot-wasmtime.gdextension "$GAME/addons/wasm-runtime/"
cp target/debug/libgodot_wasmtime.so "$GAME/addons/wasm-runtime/"
# then edit the .gdextension [libraries] paths to "libgodot_wasmtime.so"
```

Requires Godot 4.4+ (developed against 4.8-beta4 mono).

## API notes / deviations from the spec doc

- **Optional arguments.** godot-rust only supports `#[opt(default = ...)]` for
  *immutable* types (scalars, strings, vectors). `Array`, `Dictionary` and
  `Variant` parameters cannot have Rust-side defaults, so:
  - `WasmInstance.call_guest(function_name, args)` — pass `[]` explicitly.
  - `WasmInstance.safe_call_guest(function_name, args, default_fallback)` —
    pass `null` for the fallback.
  - `WasmModule.instantiate(config)` — pass `{}` for defaults.
- Guest string passing uses the `(i32 ptr, i32 len)` / packed-`i64` convention
  documented in `marshal.rs`; complex Variant values are JSON-encoded.
  **Guests that return strings to the host must export `malloc`/`free`** (or
  `allocate`/`deallocate`) so results can be written into guest memory.
- Host imports are resolved by dot-path with kebab-case (WIT-style) fallback,
  e.g. import `playersapi.get-count` or `playersapi.get_count` both reach
  `register_func("playersapi.get_count", ...)`.
- Registered host signals forward to the guest export `on_host_signal(path, ...)`.
- Callable reflection (param names/types for WIT/JSON/TS exports and string
  marshaling hints) works for named methods; **lambdas and Callables without
  a method name get no parameter metadata** — prefer named methods when
  signature fidelity matters.

## Layout

```
src/
  api.rs       symbol trie, registration, callable reflection, path resolution
  marshal.rs   Variant <-> wasm Val conversion, guest memory/string helpers
  module.rs    WasmModule + shared Engine/Linker/host-import closures
  instance.rs  WasmInstance + invocation, fuel/limits, signal emission
  runtime.rs   WasmRuntime singleton (register_*, compile/load, exporters)
  export.rs    WIT / JSON / TypeScript declaration generators
```

## License

MIT OR Apache-2.0
