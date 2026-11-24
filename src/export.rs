//! SDK exporters: WIT, JSON, and TypeScript declarations generated from the
//! registered API symbol trie.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::api::{Api, NodeKind, TypeTag};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `playersapi.this.that.thing.is.aaaaa` -> `playersapi-this-that-thing-is-aaaaa`
fn kebab(path: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for c in path.chars() {
        if c == '_' || c == '-' || c == '.' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            prev_lower_or_digit = false;
            continue;
        }
        if c.is_uppercase() {
            // Word boundary only after a lowercase letter or digit, so
            // `RED` -> `red` but `getCount` -> `get-count`.
            if prev_lower_or_digit && !out.ends_with('-') {
                out.push('-');
            }
            out.extend(c.to_lowercase());
            prev_lower_or_digit = false;
        } else {
            out.push(c.to_ascii_lowercase());
            prev_lower_or_digit = c.is_lowercase() || c.is_ascii_digit();
        }
    }
    out.trim_matches('-').to_string()
}

fn collect<'a>(api: &'a Api) -> Vec<(String, &'a crate::api::SymbolNode)> {
    let mut out = Vec::new();
    crate::api::collect_leaves(&api.root, "", &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// JSON export
// ---------------------------------------------------------------------------

pub fn export_json(api: &Api) -> String {
    export_json_pretty(api, true)
}

pub fn export_json_pretty(api: &Api, pretty: bool) -> String {
    // namespaces -> category -> dotted-subpath -> info
    #[derive(Default)]
    struct Ns {
        enums: BTreeMap<String, String>,
        constants: BTreeMap<String, String>,
        properties: BTreeMap<String, String>,
        methods: BTreeMap<String, String>,
        structs: BTreeMap<String, String>,
        signals: BTreeMap<String, String>,
    }

    let mut namespaces: BTreeMap<String, Ns> = BTreeMap::new();

    for (path, node) in collect(api) {
        let mut segs = path.splitn(2, '.');
        let ns_name = segs.next().unwrap_or("").to_string();
        let rest = segs.next().unwrap_or("").to_string();
        let ns = namespaces.entry(ns_name.clone()).or_default();
        let doc = if node.doc.is_empty() {
            String::new()
        } else {
            format!(",\"doc\":{}", json_str(&node.doc))
        };

        match &node.kind {
            Some(NodeKind::Func { params, ret, .. }) => {
                let params_json = params
                    .iter()
                    .map(|p| {
                        format!(
                            "{{\"name\":{},\"type\":{}}}",
                            json_str(&p.name),
                            json_str(p.ty.gdscript_name())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let entry = format!(
                    "{{\"params\":[{}],\"returns\":{},\"type\":\"method\"{}}}",
                    params_json,
                    json_str(ret.gdscript_name()),
                    doc
                );
                ns.methods.insert(rest.clone(), entry);
            }
            Some(NodeKind::Var { readonly, .. }) => {
                // Type is unknown statically; report as Variant unless we can
                // infer nothing better.
                let entry = format!(
                    "{{\"type\":\"Variant\",\"readonly\":{},\"access\":\"property\"{}}}",
                    readonly, doc
                );
                ns.properties.insert(rest.clone(), entry);
            }
            Some(NodeKind::Const { value }) => {
                let ty = TypeTag::from_variant(value);
                let val_json = variant_to_json(value);
                let entry = format!(
                    "{{\"type\":{},\"value\":{},\"access\":\"constant\"{}}}",
                    json_str(ty.gdscript_name()),
                    val_json,
                    doc
                );
                ns.constants.insert(rest.clone(), entry);
            }
            Some(NodeKind::Enum { values }) => {
                let inner = values
                    .iter()
                    .map(|(k, v)| format!("{}:{}", json_str(k), v))
                    .collect::<Vec<_>>()
                    .join(",");
                let entry = format!("{{{}}}{}", inner, if doc.is_empty() { String::new() } else { format!(",\"doc\":{}", json_str(&node.doc)) });
                ns.enums.insert(rest.clone(), entry);
            }
            Some(NodeKind::Struct { fields }) => {
                let inner = fields
                    .iter()
                    .map(|(k, t)| format!("{}:{}", json_str(k), json_str(t.gdscript_name())))
                    .collect::<Vec<_>>()
                    .join(",");
                let entry = format!("{{\"fields\":{{{}}},\"type\":\"struct\"{}}}", inner, doc);
                ns.structs.insert(rest.clone(), entry);
            }
            Some(NodeKind::Signal { .. }) => {
                let entry = format!("{{\"type\":\"signal\"{}}}", doc);
                ns.signals.insert(rest.clone(), entry);
            }
            None => {}
        }
    }

    // Serialize manually for full control.
    let mut out = String::from("{\n  \"namespaces\": {");
    let mut first_ns = true;
    for (ns_name, ns) in &namespaces {
        if !first_ns {
            out.push(',');
        }
        first_ns = false;
        let _ = writeln!(out, "\n    {}: {{", json_str(ns_name));
        let sections: [(&str, &BTreeMap<String, String>); 6] = [
            ("enums", &ns.enums),
            ("constants", &ns.constants),
            ("properties", &ns.properties),
            ("methods", &ns.methods),
            ("structs", &ns.structs),
            ("signals", &ns.signals),
        ];
        let mut first_sec = true;
        for (sec_name, map) in sections {
            if map.is_empty() {
                continue;
            }
            if !first_sec {
                out.push(',');
            }
            first_sec = false;
            let _ = writeln!(out, "      {}: {{", json_str(sec_name));
            let mut first = true;
            for (k, v) in map {
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = writeln!(out, "        {}: {}", json_str(k), v);
            }
            let _ = writeln!(out, "      }}");
        }
        let _ = writeln!(out, "    }}");
    }
    out.push_str("\n  }\n}");
    if !pretty {
        out = minify_json(&out);
    }
    out
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn variant_to_json(v: &godot::builtin::Variant) -> String {
    use godot::builtin::VariantType;
    match v.get_type() {
        VariantType::NIL => "null".into(),
        VariantType::BOOL => {
            if v.to::<bool>() {
                "true".into()
            } else {
                "false".into()
            }
        }
        VariantType::INT => v.to::<i64>().to_string(),
        VariantType::FLOAT => v.to::<f64>().to_string(),
        VariantType::STRING | VariantType::STRING_NAME => json_str(&v.to::<godot::builtin::GString>().to_string()),
        _ => json_str(&format!("{:?}", v)),
    }
}

fn minify_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut esc = false;
    for c in s.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => {
                    in_str = true;
                    out.push(c);
                }
                c if c.is_whitespace() => {}
                c => out.push(c),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WIT export
// ---------------------------------------------------------------------------

pub fn export_wit(api: &Api, package_name: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "package {package_name};");
    out.push('\n');
    let _ = writeln!(out, "interface host {{");

    for (path, node) in collect(api) {
        // WIT identifiers are kebab-case; use path relative to nothing (full).
        let name = kebab(&path);
        let doc_comment = if node.doc.is_empty() {
            String::new()
        } else {
            format!("    // {}\n", node.doc.replace('\n', " "))
        };
        match &node.kind {
            Some(NodeKind::Func { params, ret, .. }) => {
                let ps = params
                    .iter()
                    .map(|p| format!("{}: {}", kebab(&p.name), p.ty.wit_type()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write!(
                    out,
                    "{doc_comment}    {name}: func({ps}) -> {};\n",
                    ret.wit_type()
                );
            }
            Some(NodeKind::Var { readonly, .. }) => {
                // Property type isn't statically known; accessors are
                // documented as JSON-string payloads. Readonly vars get no
                // setter.
                let _ = write!(
                    out,
                    "{doc_comment}    {name}: func() -> string;\n"
                );
                if !readonly {
                    let _ = write!(out, "{doc_comment}    {name}-set: func(val: string);\n");
                }
            }
            Some(NodeKind::Const { value }) => {
                let ty = TypeTag::from_variant(value);
                let _ = write!(
                    out,
                    "{doc_comment}    {name}: func() -> {};\n",
                    ty.wit_type()
                );
            }
            Some(NodeKind::Enum { values }) => {
                let ename = kebab(&path);
                let _ = writeln!(out, "{doc_comment}    enum {ename} {{");
                for (k, _) in values {
                    let _ = writeln!(out, "        {},", kebab(k));
                }
                let _ = writeln!(out, "    }}\n");
            }
            Some(NodeKind::Struct { fields }) => {
                let sname = kebab(&path);
                let _ = writeln!(out, "{doc_comment}    record {sname} {{");
                for (k, t) in fields {
                    let _ = writeln!(out, "        {}: {},", kebab(k), t.wit_type());
                }
                let _ = writeln!(out, "    }}\n");
            }
            Some(NodeKind::Signal { .. }) => {
                let _ = write!(
                    out,
                    "{doc_comment}    {name}: func() -> string;\n"
                );
            }
            None => {}
        }
    }

    let _ = writeln!(out, "}}");
    out.push('\n');
    let _ = writeln!(out, "world mod-environment {{");
    let _ = writeln!(out, "    import host;");
    let _ = writeln!(out, "    export mod-init: func();");
    let _ = writeln!(out, "    export mod-update: func(delta: f64);");
    let _ = writeln!(out, "}}");
    out
}

// ---------------------------------------------------------------------------
// TypeScript export
// ---------------------------------------------------------------------------

pub fn export_typescript(api: &Api) -> String {
    let mut out = String::new();
    out.push_str("// Auto-generated by Godot-WASMTime WasmRuntime.export_typescript_declarations()\n");
    out.push_str("// Guest-side host API declarations.\n\n");

    // Build nested namespace tree mirroring the trie.
    fn emit_node(
        out: &mut String,
        node: &crate::api::SymbolNode,
        indent: usize,
        path: &str,
    ) {
        let pad = "  ".repeat(indent);
        // Children that are pure namespaces get nested `declare namespace`.
        let mut names: Vec<&String> = node.children.keys().collect();
        names.sort();
        for name in names {
            let child = &node.children[name];
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let is_pure_ns = child.kind.is_none() && !child.children.is_empty();
            if is_pure_ns {
                let _ = writeln!(out, "{pad}export declare namespace {name} {{");
                emit_node(out, child, indent + 1, &child_path);
                let _ = writeln!(out, "{pad}}}\n");
            } else {
                emit_leaf(out, child, indent, &child_path, name);
            }
        }
    }

    fn emit_leaf(
        out: &mut String,
        node: &crate::api::SymbolNode,
        indent: usize,
        path: &str,
        name: &str,
    ) {
        let pad = "  ".repeat(indent);
        let doc = if node.doc.is_empty() {
            String::new()
        } else {
            format!("{pad}/** {} */\n", node.doc.replace("*/", "* /"))
        };
        match &node.kind {
            Some(NodeKind::Func { params, ret, .. }) => {
                let ps = params
                    .iter()
                    .map(|p| format!("{}: {}", p.name, p.ty.ts_type()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    out,
                    "{doc}{pad}export function {name}({ps}): {};",
                    ret.ts_type()
                );
            }
            Some(NodeKind::Var { readonly, .. }) => {
                let _ = writeln!(
                    out,
                    "{doc}{pad}export let {name}: any; // property{}",
                    if *readonly { " (readonly)" } else { "" }
                );
            }
            Some(NodeKind::Const { value }) => {
                let ty = TypeTag::from_variant(value);
                let _ = writeln!(out, "{doc}{pad}export const {name}: {};", ty.ts_type());
            }
            Some(NodeKind::Enum { values }) => {
                let _ = writeln!(out, "{doc}{pad}export const {name} = {{");
                for (k, v) in values {
                    let _ = writeln!(out, "{pad}  {}: {},", k, v);
                }
                let _ = writeln!(out, "{pad}}} as const;");
            }
            Some(NodeKind::Struct { fields }) => {
                let _ = writeln!(out, "{doc}{pad}export interface {name} {{");
                for (k, t) in fields {
                    let _ = writeln!(out, "{pad}  {}: {};", k, t.ts_type());
                }
                let _ = writeln!(out, "{pad}}}");
            }
            Some(NodeKind::Signal { .. }) => {
                let _ = writeln!(out, "{doc}{pad}export const {name}: string; // signal");
            }
            None => {
                // Namespace node with a kind? Recurse.
                emit_node(out, node, indent, path);
            }
        }
    }

    emit_node(&mut out, &api.root, 0, "");
    out
}
