//! Symbol trie and API registry for the WASM UGC runtime.
//!
//! `WasmRuntime` owns an [`Api`] tree built from `register_*` calls. Each node
//! of the trie corresponds to one dot-delimited path segment (e.g.
//! `playersapi.local.health` -> `playersapi` -> `local` -> `health`).

use std::collections::HashMap;

use godot::builtin::{Callable, StringName, Variant};
use godot::classes::Object;
use godot::obj::Gd;
use godot::prelude::*;

/// High-level type tag used for reflection, marshaling and SDK export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeTag {
    Unknown,
    Nil,
    Bool,
    Int,
    Float,
    String,
    Array,
    Dictionary,
    Object,
    Vector2,
    Vector3,
    Vector4,
    Color,
    Other,
}

impl TypeTag {
    /// Maps a Godot `Variant::Type` ordinal (as found in `get_method_list`) to a tag.
    pub fn from_godot_ord(ord: i64) -> Self {
        match ord {
            0 => TypeTag::Nil,
            1 => TypeTag::Bool,
            2 => TypeTag::Int,
            3 => TypeTag::Float,
            4 => TypeTag::String,
            27 => TypeTag::Dictionary,
            28 => TypeTag::Array,
            24 => TypeTag::Object,
            21 => TypeTag::Other, // STRING_NAME
            22 => TypeTag::Other, // NODE_PATH
            5 | 6 => TypeTag::Vector2,
            9 | 10 => TypeTag::Vector3,
            12 | 13 => TypeTag::Vector4,
            20 => TypeTag::Color,
            _ => TypeTag::Other,
        }
    }

    /// Best-effort tag for a live `Variant` value.
    pub fn from_variant(v: &Variant) -> Self {
        TypeTag::from_godot_ord(i64::from(v.get_type().ord()))
    }

    /// GDScript-facing type name (used in JSON export).
    pub fn gdscript_name(self) -> &'static str {
        match self {
            TypeTag::Nil => "Nil",
            TypeTag::Bool => "bool",
            TypeTag::Int => "int",
            TypeTag::Float => "float",
            TypeTag::String => "String",
            TypeTag::Array => "Array",
            TypeTag::Dictionary => "Dictionary",
            TypeTag::Object => "Object",
            TypeTag::Vector2 => "Vector2",
            TypeTag::Vector3 => "Vector3",
            TypeTag::Vector4 => "Vector4",
            TypeTag::Color => "Color",
            TypeTag::Other | TypeTag::Unknown => "Variant",
        }
    }

    /// WIT type. Non-scalar values cross the boundary JSON-encoded as strings.
    pub fn wit_type(self) -> &'static str {
        match self {
            TypeTag::Bool => "bool",
            TypeTag::Int => "s64",
            TypeTag::Float => "f64",
            TypeTag::String => "string",
            // Array/Dictionary/Object/etc. are marshaled as JSON strings.
            _ => "string",
        }
    }

    /// TypeScript type for .d.ts exports.
    pub fn ts_type(self) -> &'static str {
        match self {
            TypeTag::Nil => "null",
            TypeTag::Bool => "boolean",
            TypeTag::Int | TypeTag::Float => "number",
            TypeTag::String => "string",
            TypeTag::Array => "any[]",
            TypeTag::Dictionary => "Record<string, any>",
            TypeTag::Object => "object",
            TypeTag::Vector2 | TypeTag::Vector3 | TypeTag::Vector4 | TypeTag::Color => "any",
            _ => "any",
        }
    }
}

/// Reflected parameter info for a registered callable.
#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub name: String,
    pub ty: TypeTag,
}

/// What a trie node actually is.
#[derive(Clone)]
#[allow(dead_code)] // emitter/signal retained for introspection & future use
pub enum NodeKind {
    /// Callable function/method.
    Func {
        callable: Callable,
        params: Vec<ParamInfo>,
        ret: TypeTag,
    },
    /// Property variable (synthetic getter/setter pair).
    Var {
        target: Gd<Object>,
        property: StringName,
        readonly: bool,
    },
    /// Immutable constant.
    Const { value: Variant },
    /// Enum dictionary (name -> i64 value), zero-cost i32 on the wire.
    Enum { values: Vec<(String, i64)> },
    /// Explicit struct/record schema.
    Struct { fields: Vec<(String, TypeTag)> },
    /// Bound Godot signal.
    Signal { emitter: Gd<Object>, signal: StringName },
}

/// One node of the symbol trie.
#[derive(Default)]
pub struct SymbolNode {
    pub doc: String,
    pub kind: Option<NodeKind>,
    pub children: HashMap<String, SymbolNode>,
}

impl SymbolNode {
    fn child_mut(&mut self, seg: &str) -> &mut SymbolNode {
        self.children.entry(seg.to_string()).or_default()
    }

    fn child(&self, seg: &str) -> Option<&SymbolNode> {
        self.children.get(seg)
    }
}

/// The whole registered API tree.
#[derive(Default)]
pub struct Api {
    pub root: SymbolNode,
}

/// A resolved, owned (cloned) host target ready to be invoked.
/// Cloning out of the trie before invocation avoids holding a borrow on the
/// runtime while arbitrary GDScript callables run (which may re-enter
/// `WasmRuntime` and need `&mut`).
#[derive(Clone)]
#[allow(dead_code)] // `ret`/`param_types` retained for reflection consumers
pub enum ResolvedTarget {
    Func {
        callable: Callable,
        params: Vec<ParamInfo>,
        ret: TypeTag,
    },
    VarGet {
        target: Gd<Object>,
        property: StringName,
    },
    VarSet {
        target: Gd<Object>,
        property: StringName,
        readonly: bool,
    },
    Const {
        value: Variant,
    },
    EnumValue {
        value: i64,
    },
}

impl ResolvedTarget {
    /// Parameter type hints for marshaling (used for string detection).
    #[allow(dead_code)]
    pub fn param_types(&self) -> &[ParamInfo] {
        match self {
            ResolvedTarget::Func { params, .. } => params,
            _ => &[],
        }
    }
}

/// Walk `segs` from `node`, returning the terminal node.
fn walk<'a>(mut node: &'a SymbolNode, segs: &[&str]) -> Option<&'a SymbolNode> {
    for s in segs {
        node = node.child(s)?;
    }
    Some(node)
}

/// Resolve a dot-delimited path to an invocable target.
///
/// Resolution order:
/// 1. Exact path match on Func/Var/Const.
/// 2. `path` where the last segment is a key of a parent `Enum` node
///    (e.g. `playersapi.Team.RED`).
/// 3. `path.set` suffix -> `VarSet` for a `Var` node.
pub fn resolve(api: &Api, path: &str) -> Option<ResolvedTarget> {
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }

    // 1. Exact match.
    if let Some(node) = walk(&api.root, &segs) {
        match &node.kind {
            Some(NodeKind::Func {
                callable,
                params,
                ret,
            }) => {
                return Some(ResolvedTarget::Func {
                    callable: callable.clone(),
                    params: params.clone(),
                    ret: *ret,
                })
            }
            Some(NodeKind::Var {
                target,
                property,
                ..
            }) => {
                return Some(ResolvedTarget::VarGet {
                    target: target.clone(),
                    property: property.clone(),
                })
            }
            Some(NodeKind::Const { value }) => {
                return Some(ResolvedTarget::Const {
                    value: value.clone(),
                })
            }
            _ => {}
        }
    }

    // 2. Enum value: parent node is an Enum, last segment is a key.
    if segs.len() >= 2 {
        if let Some(parent) = walk(&api.root, &segs[..segs.len() - 1]) {
            if let Some(NodeKind::Enum { values }) = &parent.kind {
                let key = segs[segs.len() - 1];
                if let Some((_, v)) = values.iter().find(|(k, _)| k == key) {
                    return Some(ResolvedTarget::EnumValue { value: *v });
                }
                // Case-insensitive fallback.
                if let Some((_, v)) = values
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                {
                    return Some(ResolvedTarget::EnumValue { value: *v });
                }
            }
        }
    }

    // 3. `.set` suffix on a Var node.
    if let Some(parent_path) = path.strip_suffix(".set") {
        let psegs: Vec<&str> = parent_path.split('.').filter(|s| !s.is_empty()).collect();
        if let Some(node) = walk(&api.root, &psegs) {
            if let Some(NodeKind::Var {
                target,
                property,
                readonly,
            }) = &node.kind
            {
                return Some(ResolvedTarget::VarSet {
                    target: target.clone(),
                    property: property.clone(),
                    readonly: *readonly,
                });
            }
        }
    }

    None
}

/// Resolve a kebab-case flattened path (WIT-style name, e.g.
/// `playersapi-local-health` or `playersapi-local-health-set`) back to a
/// dot-path. Segments are matched greedily against child names with
/// `_`/`-`/`.` treated as equivalent and case-insensitive comparison.
///
/// Returns `(dot_path, is_setter)`.
pub fn kebab_to_dot_path(root: &SymbolNode, kebab: &str) -> Option<(String, bool)> {
    let mut is_set = false;
    let mut name = kebab;
    if let Some(stripped) = name.strip_suffix("-set") {
        is_set = true;
        name = stripped;
    }
    let parts: Vec<&str> = name.split('-').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    // Recursive greedy match: at each level, try joining 1..=n remaining
    // dash-separated parts and compare against child keys (normalized).
    fn norm(s: &str) -> String {
        s.chars()
            .filter(|c| *c != '_' && *c != '-' && *c != '.')
            .flat_map(|c| c.to_lowercase())
            .collect()
    }

    fn rec<'a>(
        node: &'a SymbolNode,
        parts: &[&str],
        idx: usize,
        out: &mut Vec<String>,
    ) -> bool {
        if idx == parts.len() {
            return true;
        }
        // Try longest join first so `get-count` matches `get_count` before `get`.
        for take in (1..=(parts.len() - idx)).rev() {
            let candidate = parts[idx..idx + take].join("-");
            let cand_norm = norm(&candidate);
            if let Some((key, child)) = node
                .children
                .iter()
                .find(|(k, _)| norm(k) == cand_norm)
            {
                out.push(key.clone());
                if rec(child, parts, idx + take, out) {
                    return true;
                }
                out.pop();
            }
        }
        false
    }

    let mut segs = Vec::new();
    if !rec(root, &parts, 0, &mut segs) {
        return None;
    }
    Some((segs.join("."), is_set))
}

/// Insert (or replace) a node kind at `path`, creating intermediate namespaces.
pub fn register_node(api: &mut Api, path: &str, kind: NodeKind, doc: &str) {
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return;
    }
    let mut node = &mut api.root;
    for s in &segs {
        node = node.child_mut(s);
    }
    node.kind = Some(kind);
    if !doc.is_empty() {
        node.doc = doc.to_string();
    }
}

/// Remove the node at `path` (and prune empty ancestors).
pub fn unregister_node(api: &mut Api, path: &str) {
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return;
    }
    remove_at(&mut api.root, &segs);
}

fn remove_at(node: &mut SymbolNode, segs: &[&str]) -> bool {
    if segs.is_empty() {
        return false;
    }
    if segs.len() == 1 {
        return node.children.remove(segs[0]).is_some();
    }
    if let Some(child) = node.children.get_mut(segs[0]) {
        if remove_at(child, &segs[1..]) {
            // Prune child if it became an empty namespace.
            if child.children.is_empty() && child.kind.is_none() {
                node.children.remove(segs[0]);
            }
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Callable reflection
// ---------------------------------------------------------------------------

fn variant_type_ord(v: &Variant) -> i64 {
    i64::from(v.get_type().ord())
}

fn dict_get<'a>(dict: &'a VarDictionary, key: &str) -> Variant {
    dict.get_or_nil(&key.to_variant())
}

/// Best-effort reflection of a `Callable`'s parameter names/types and return
/// type via `Object::get_method_list`. Works for GDScript and native methods;
/// returns empty params when reflection is unavailable (e.g. lambdas).
pub fn reflect_callable(callable: &Callable) -> (Vec<ParamInfo>, TypeTag) {
    let empty = (Vec::new(), TypeTag::Unknown);
    let Some(mut obj) = callable.object() else {
        return empty;
    };
    let Some(method) = callable.method_name() else {
        return empty;
    };
    let method_str = method.to_string();

    // `get_method_list` is a Godot Object method; call it defensively.
    // It returns a typed `Array[Dictionary]`.
    let list_var = obj.call("get_method_list", &[]);
    let Ok(list) = list_var.try_to::<godot::builtin::Array<VarDictionary>>() else {
        return empty;
    };

    for dict in list.iter_shared() {
        let name_v = dict_get(&dict, "name");
        let name = name_v.to::<GString>().to_string();
        if name != method_str {
            continue;
        }

        let mut params = Vec::new();
        if let Ok(args) = dict_get(&dict, "args").try_to::<godot::builtin::Array<VarDictionary>>() {
            for adict in args.iter_shared() {
                let pname = dict_get(&adict, "name")
                    .to::<GString>()
                    .to_string();
                let ty_ord = dict_get(&adict, "type").to::<i64>();
                params.push(ParamInfo {
                    name: pname,
                    ty: TypeTag::from_godot_ord(ty_ord),
                });
            }
        }

        let ret = dict_get(&dict, "return");
        let ret_ty = if ret.get_type() == VariantType::DICTIONARY {
            let rdict = ret.to::<VarDictionary>();
            TypeTag::from_godot_ord(dict_get(&rdict, "type").to::<i64>())
        } else {
            TypeTag::Unknown
        };
        // `return` may itself be absent on older formats; fall back to nil.
        let _ = variant_type_ord(&ret);
        return (params, ret_ty);
    }

    empty
}

/// Convert a GDScript `Dictionary` (enum) into ordered (name, value) pairs.
pub fn enum_dict_to_vec(dict: &VarDictionary) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for (k, v) in dict.iter_shared() {
        let key = k.to::<GString>().to_string();
        let val = match v.get_type() {
            VariantType::INT => v.to::<i64>(),
            VariantType::FLOAT => v.to::<f64>() as i64,
            VariantType::BOOL => {
                if v.to::<bool>() {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        };
        out.push((key, val));
    }
    out
}

/// Convert a GDScript `Dictionary` of field-name -> type into struct fields.
/// Accepts either `{ "x": TYPE_FLOAT, ... }` constants or
/// `{ "x": "float", ... }` strings.
pub fn struct_dict_to_fields(dict: &VarDictionary) -> Vec<(String, TypeTag)> {
    let mut out = Vec::new();
    for (k, v) in dict.iter_shared() {
        let key = k.to::<GString>().to_string();
        let ty = match v.get_type() {
            VariantType::INT => TypeTag::from_godot_ord(v.to::<i64>()),
            VariantType::STRING => type_tag_from_name(&v.to::<GString>().to_string()),
            _ => TypeTag::Unknown,
        };
        out.push((key, ty));
    }
    out
}

fn type_tag_from_name(name: &str) -> TypeTag {
    match name.to_ascii_lowercase().as_str() {
        "bool" | "boolean" => TypeTag::Bool,
        "int" | "integer" | "i32" | "i64" | "s32" | "s64" => TypeTag::Int,
        "float" | "double" | "f32" | "f64" | "number" | "real" => TypeTag::Float,
        "string" | "str" => TypeTag::String,
        "array" | "list" | "vec" | "vector" => TypeTag::Array,
        "dict" | "dictionary" | "map" | "record" | "object" => TypeTag::Dictionary,
        "vector2" | "vec2" => TypeTag::Vector2,
        "vector3" | "vec3" => TypeTag::Vector3,
        "vector4" | "vec4" => TypeTag::Vector4,
        "color" | "colour" => TypeTag::Color,
        "nil" | "null" | "void" | "none" => TypeTag::Nil,
        _ => TypeTag::Unknown,
    }
}

/// Collect all leaf entries of the trie for SDK export.
pub fn collect_leaves<'a>(
    node: &'a SymbolNode,
    prefix: &str,
    out: &mut Vec<(String, &'a SymbolNode)>,
) {
    for (name, child) in &node.children {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if child.kind.is_some() {
            out.push((path.clone(), child));
        }
        collect_leaves(child, &path, out);
    }
}
