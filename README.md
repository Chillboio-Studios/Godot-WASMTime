# Godot-WASMTime

**Godot WASM UGC Runtime** — a Rust [GDExtension](https://docs.godotengine.org/en/stable/tutorials/scripting/gdextension/) that embeds [Wasmtime](https://wasmtime.dev/) inside Godot 4, letting you expose arbitrary host APIs to sandboxed WebAssembly guest modules and export SDK specifications for modding toolchains.

Register dot-delimited host paths (`playersapi.this.that.thing.is.aaaaa()`), run community WASM mods with fuel metering and memory limits, and generate **WIT**, **JSON**, and **TypeScript** interface definitions so modders can build with Rust, C, Zig, TinyGo, or AssemblyScript.

## Features

- **Three GDScript classes**: `WasmRuntime` (engine singleton), `WasmModule` (compiled bytecode, shareable), `WasmInstance` (isolated sandboxed execution)
- **Unlimited API nesting** — host functions, properties, enums, constants, signals, and structs at arbitrary dot-delimited paths
- **Sandboxed execution** — per-instance linear memory caps (`max_memory_mb`), CPU fuel metering, opt-in WASI p1, and trap isolation (`safe_call_guest` never crashes the host)
- **String & binary marshaling** — `(ptr, len)` guest-memory string ABI, guest `malloc`/`free` integration, direct `read_bytes`/`write_bytes` helpers for custom serialization layers
- **SDK exporters** — `export_wit*`, `export_json*`, `export_typescript*` (+ `_to_file` variants) with automatic Callable reflection
- **Host signals** — Godot signals bound to guest paths; guests subscribe via the `on_host_signal` export
- **Prebuilt binaries** for Linux, Windows, macOS (x86_64 + ARM64) and Android via GitHub Actions

## Requirements

- Godot **4.4+** (developed against 4.8-beta4)
- One of the prebuilt binaries below, or a Rust toolchain (`stable`) to build from source

## Installation

### Prebuilt binaries (recommended)

Download `godot-wasmtime-gdextension.zip` from the [Releases](../../releases) page (or the `godot-wasmtime-gdextension` workflow artifact). It contains every platform's library plus a `godot-wasmtime.gdextension` manifest.

Copy **all** files into your project, e.g. `res://addons/wasm-runtime/`:

```
addons/wasm-runtime/
├── godot-wasmtime.gdextension
├── libgodot_wasmtime.linux.x86_64.so
├── libgodot_wasmtime.linux.aarch64.so
├── libgodot_wasmtime.windows.x86_64.dll
├── libgodot_wasmtime.windows.arm64.dll
├── libgodot_wasmtime.macos.x86_64.dylib
├── libgodot_wasmtime.macos.arm64.dylib
├── libgodot_wasmtime.android.arm64.so
└── libgodot_wasmtime.android.x86_64.so
```

Godot picks the right library automatically. You can delete binaries for platforms you don't ship to.

> **Note:** 32-bit platforms (i686 Windows/Linux, armv7) are not supported — Wasmtime requires 64-bit hosts.

### Building from source

```sh
cargo build --release   # -> target/release/libgodot_wasmtime.{so,dll,dylib}
```

Copy the `.gdextension` file and the built library into your project and make sure the `[libraries]` paths resolve relative to the `.gdextension` file (see `godot-wasmtime.gdextension` for the dev layout, which points into `target/`).

Cross-compilation for all release platforms is handled by [`.github/workflows/release.yml`](.github/workflows/release.yml) — push a `v*` tag or run the workflow manually.

## Quick start

```gdscript
# No autoload needed — WasmRuntime is an engine singleton.

# 1. Register host APIs (any depth of dot-paths)
WasmRuntime.register_enum("playersapi.Team", { RED = 0, BLUE = 1 })
WasmRuntime.register_const("playersapi.MAX_PLAYERS", 64)
WasmRuntime.register_var("playersapi.local.health", self, "health")
WasmRuntime.register_func("playersapi.this.that.thing.is.aaaaa", deep_action)
WasmRuntime.register_signal("playersapi.on_damaged", self, "player_damaged")

# 2. Compile + instantiate a guest
var module := WasmRuntime.compile_bytes(FileAccess.get_file_as_bytes("user://mods/mod.wasm"))
var inst := module.instantiate({
    "max_memory_mb": 32,
    "enable_fuel": true,
    "initial_fuel": 200_000,
})
inst.trapped.connect(func(r, bt): printerr("mod crashed: ", r))

# 3. Call guest exports
inst.call_guest("mod_init")
inst.fuel_remaining = 200_000            # per-tick budget
inst.safe_call_guest("mod_update", [delta])
```

## API overview

### `WasmRuntime` (singleton)

| Method | Description |
| :--- | :--- |
| `register_func(path, callable, doc := "")` | Register a method at a dot-path |
| `register_var(path, target, property, readonly := false, doc := "")` | Property as synthetic getter/setter |
| `register_enum(path, dict, doc := "")` | Enum dictionary → zero-cost i32 |
| `register_const(path, value, doc := "")` | Immutable compile-time constant |
| `register_signal(path, emitter, signal_name, doc := "")` | Bind a Godot signal to the runtime |
| `register_struct(path, fields, doc := "")` | Custom record schema (`{"x": TYPE_FLOAT, ...}`) |
| `unregister(path)` | Remove a namespace branch or endpoint |
| `compile_bytes(bytes) -> WasmModule` | Compile `.wasm` or `.wat` bytes |
| `load_module(path) -> WasmModule` | Load + compile from `user://`/`res://` |
| `export_wit(package)` / `export_wit_to_file(path, package)` | WIT spec for wit-bindgen toolchains |
| `export_json()` / `export_json_to_file(path, pretty)` | Full symbol tree + signatures |
| `export_typescript_declarations()` / `export_typescript_to_file(path)` | `.d.ts` stubs |

### `WasmModule` (`RefCounted`)

- `instantiate(config := {}) -> WasmInstance` — config keys: `max_memory_mb` (64), `enable_fuel` (true), `initial_fuel` (1000000), `allow_wasi` (false)

### `WasmInstance` (`RefCounted`)

- **Properties**: `is_alive`, `fuel_remaining` (rw), `memory_size_bytes`, `peak_memory_bytes`
- **Signals**: `trapped(reason, backtrace)`, `out_of_fuel()`, `out_of_memory()`, `guest_panicked(message)`
- **Calls**: `has_func(name)`, `call_guest(name, args)`, `safe_call_guest(name, args, fallback)`
- **Memory**: `guest_alloc(size)`, `guest_free(ptr)`, `read_string(ptr, len)`, `write_string(ptr, s)`, `read_bytes(ptr, len)`, `write_bytes(ptr, bytes)`

## Guest-side conventions

- **Imports**: the linker exposes guest imports under the registered namespaces (e.g. `(import "playersapi" "get-count" ...)`). Kebab-case (WIT-style) names are matched automatically.
- **Strings**: passed as `(i32 ptr, i32 len)` pairs or a packed `i64` (`ptr << 32 | len`). Guests returning strings **must export `malloc`/`free`** (or `allocate`/`deallocate`).
- **Complex types**: non-scalar Variants are JSON-encoded.
- **Host signals**: forwarded to guest export `on_host_signal(path, ...)`.

## API notes / deviations

- godot-rust only supports defaults on immutable `#[opt]` params, so pass `[]` to `call_guest`, `{}` to `instantiate`, and `null` as the `safe_call_guest` fallback.
- Callable reflection (param names/types in exports and marshaling hints) requires named methods — lambdas produce no metadata.
- WIT output uses kebab-case identifiers; enums become WIT enums, readonly vars omit setters.

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

Licensed under the **Creative Commons Attribution-NonCommercial-ShareAlike 4.0 International** license — see [LICENSE](LICENSE) or <https://creativecommons.org/licenses/by-nc-sa/4.0/>.

You are free to share and adapt this work for **non-commercial** purposes, with attribution and under the same license. See the license for details.
