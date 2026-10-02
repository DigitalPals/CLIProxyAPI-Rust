//! JSON Schema down-levelling for Google's private Cloud Code backend
//! (Antigravity), which accepts only a small OpenAPI-like subset.
//!
//! Everything the backend rejects is either rewritten (`$ref` inlined, unions
//! collapsed to their strongest branch, `const` to `enum`) or folded into the
//! description as a hint so the model still sees the constraint.

use serde_json::{Map, Value, json};

const PLACEHOLDER_REASON: &str = "Brief explanation of why you are calling this tool";

/// Moved into the description, then removed.
const CONSTRAINTS: &[&str] = &[
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "format",
    "default",
    "examples",
    "minimum",
    "maximum",
    "multipleOf",
];

/// Removed outright.
const UNSUPPORTED: &[&str] = &[
    "$schema",
    "$defs",
    "definitions",
    "$id",
    "id",
    "$anchor",
    "$vocabulary",
    "$dynamicRef",
    "$dynamicAnchor",
    "$comment",
    "propertyNames",
    "patternProperties",
    "if",
    "then",
    "else",
    "enumDescriptions",
    "enumTitles",
    "prefill",
    "deprecated",
    "encrypted",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
    "readOnly",
    "writeOnly",
    "dependentRequired",
    "dependentSchemas",
    "minProperties",
    "maxProperties",
    "contentEncoding",
    "contentMediaType",
];

#[derive(Clone, Copy)]
struct Opts {
    /// Claude models on Antigravity run in VALIDATED mode, which needs non-empty objects.
    placeholder: bool,
    /// Tool enums become description hints (the backend only takes string enums).
    drop_enums: bool,
    /// Response schemas keep `additionalProperties: false`.
    keep_closed: bool,
    remove_title: bool,
}

/// Cleans a function-declaration parameter schema.
pub fn clean_tool(schema: &Value, validated: bool) -> Value {
    clean_root(schema, Opts { placeholder: validated, drop_enums: true, keep_closed: false, remove_title: !validated })
}

/// Cleans a structured-output (response) schema.
pub fn clean_response(schema: &Value) -> Value {
    clean_root(schema, Opts { placeholder: false, drop_enums: false, keep_closed: true, remove_title: false })
}

/// Only resolves local `$ref`s (and drops the definitions); everything else is kept.
pub fn inline_only(schema: &Value) -> Value {
    let defs = collect_defs(schema);
    let mut out = inline_refs(schema, &defs, &mut Vec::new());
    if let Some(o) = out.as_object_mut() {
        o.remove("$defs");
        o.remove("definitions");
    }
    out
}

fn collect_defs(schema: &Value) -> Map<String, Value> {
    let mut defs = Map::new();
    for k in ["$defs", "definitions"] {
        if let Some(Value::Object(d)) = schema.get(k) {
            for (name, v) in d {
                defs.insert(format!("#/{k}/{name}"), v.clone());
            }
        }
    }
    defs
}

fn clean_root(schema: &Value, opts: Opts) -> Value {
    let defs = collect_defs(schema);
    let inlined = inline_refs(schema, &defs, &mut Vec::new());
    let mut out = clean(&inlined, opts);
    if !out.is_object() {
        out = json!({ "type": "object", "properties": {} });
    }
    if out.get("type").is_none() {
        out["type"] = "object".into();
    }
    if opts.placeholder {
        add_placeholders(&mut out, true);
    }
    out
}

fn inline_refs(v: &Value, defs: &Map<String, Value>, stack: &mut Vec<String>) -> Value {
    match v {
        Value::Object(m) => {
            if let Some(r) = m.get("$ref").and_then(Value::as_str) {
                if stack.iter().any(|s| s == r) {
                    let name = r.rsplit('/').next().unwrap_or(r);
                    return json!({ "type": "object", "description": format!("(recursive reference to {name})") });
                }
                if let Some(target) = defs.get(r) {
                    stack.push(r.to_string());
                    let mut resolved = inline_refs(target, defs, stack);
                    stack.pop();
                    // Sibling keywords (usually a description) win over the target's.
                    if let Value::Object(rm) = &mut resolved {
                        for (k, sv) in m {
                            if k != "$ref" {
                                rm.insert(k.clone(), inline_refs(sv, defs, stack));
                            }
                        }
                    }
                    return resolved;
                }
            }
            Value::Object(m.iter().map(|(k, v)| (k.clone(), inline_refs(v, defs, stack))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| inline_refs(x, defs, stack)).collect()),
        other => other.clone(),
    }
}

fn hint(m: &mut Map<String, Value>, text: String) {
    let desc = m.get("description").and_then(Value::as_str).unwrap_or_default();
    let merged = if desc.is_empty() { text } else { format!("{desc} ({text})") };
    m.insert("description".into(), merged.into());
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn type_of(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

/// Strongest branch of a union: object > array > other > null.
fn best_branch(items: &[Value]) -> (usize, Vec<String>) {
    let mut best = (0usize, -1i32);
    let mut types = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let t = type_of(it).unwrap_or_default();
        let (score, name) = if t == "object" || it.get("properties").is_some() {
            (3, "object")
        } else if t == "array" || it.get("items").is_some() {
            (2, "array")
        } else if !t.is_empty() && t != "null" {
            (1, t)
        } else if t == "null" {
            (0, "null")
        } else {
            (0, "")
        };
        if !name.is_empty() && !types.iter().any(|x| x == name) {
            types.push(name.to_string());
        }
        if score > best.1 {
            best = (i, score);
        }
    }
    (best.0, types)
}

fn clean(v: &Value, opts: Opts) -> Value {
    let Value::Object(src) = v else {
        // `true` / `{}`-like schemas: accept anything.
        return if v == &Value::Bool(false) { json!({ "type": "string" }) } else { json!({}) };
    };
    let mut m = src.clone();

    // allOf: merge every branch into this node.
    if let Some(Value::Array(all)) = m.remove("allOf") {
        for branch in all {
            let Value::Object(b) = clean(&branch, opts) else { continue };
            for (k, bv) in b {
                match (k.as_str(), m.get_mut(&k)) {
                    ("properties", Some(Value::Object(p))) => {
                        if let Value::Object(bp) = bv {
                            for (pk, pv) in bp {
                                p.entry(pk).or_insert(pv);
                            }
                        }
                    }
                    ("required", Some(Value::Array(r))) => {
                        for x in bv.as_array().into_iter().flatten() {
                            if !r.contains(x) {
                                r.push(x.clone());
                            }
                        }
                    }
                    (_, None) => {
                        m.insert(k, bv);
                    }
                    _ => {}
                }
            }
        }
    }

    // anyOf / oneOf: keep the strongest branch.
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(items)) = m.remove(key) else { continue };
        if items.is_empty() {
            continue;
        }
        let has_null = items.iter().any(|i| type_of(i) == Some("null"));
        if let Some(Value::Object(props)) = m.get_mut("properties") {
            for it in &items {
                if let Some(Value::Object(bp)) = it.get("properties") {
                    for (pk, pv) in bp {
                        props.entry(pk.clone()).or_insert_with(|| pv.clone());
                    }
                }
            }
        } else {
            let (idx, types) = best_branch(&items);
            if let Value::Object(b) = clean(&items[idx], opts) {
                for (k, bv) in b {
                    if k == "description" && m.contains_key("description") {
                        continue;
                    }
                    m.insert(k, bv);
                }
            }
            if types.len() > 1 {
                hint(&mut m, format!("Accepts: {}", types.join(" | ")));
            }
        }
        if has_null && type_of(&Value::Object(m.clone())) != Some("null") {
            m.insert("nullable".into(), true.into());
        }
    }

    // ["string", "null"] -> "string" + nullable.
    if let Some(Value::Array(ts)) = m.get("type").cloned() {
        let non_null: Vec<&str> = ts.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect();
        if ts.iter().any(|t| t == "null") {
            m.insert("nullable".into(), true.into());
        }
        if non_null.len() > 1 {
            hint(&mut m, format!("Accepts: {}", non_null.join(" | ")));
        }
        m.insert("type".into(), non_null.first().copied().unwrap_or("string").into());
    }

    if let Some(c) = m.remove("const") {
        m.entry("enum").or_insert_with(|| json!([c]));
    }
    if let Some(Value::Array(vals)) = m.remove("enum") {
        let vals: Vec<String> = vals.iter().map(scalar).collect();
        let boolean = type_of(&Value::Object(m.clone())) == Some("boolean");
        if opts.drop_enums || boolean {
            hint(&mut m, format!("Allowed: {}", vals.join(", ")));
            m.entry("type").or_insert_with(|| "string".into());
        } else {
            m.insert("enum".into(), vals.into_iter().map(Value::String).collect::<Vec<_>>().into());
            m.insert("type".into(), "string".into());
        }
    }

    for k in CONSTRAINTS {
        if let Some(c) = m.remove(*k) {
            hint(&mut m, format!("{k}: {}", scalar(&c)));
        }
    }
    if let Some(n) = m.remove("not") {
        hint(&mut m, format!("must not match: {n}"));
    }
    if let Some(ap) = m.remove("additionalProperties")
        && opts.keep_closed
        && ap == Value::Bool(false)
    {
        m.insert("additionalProperties".into(), false.into());
    }
    if let Some(r) = m.remove("$ref") {
        hint(&mut m, format!("see {}", scalar(&r)));
        m.entry("type").or_insert_with(|| "object".into());
    }
    for k in UNSUPPORTED {
        m.remove(*k);
    }
    if opts.remove_title {
        m.remove("title");
    }
    m.retain(|k, _| !k.starts_with("x-"));

    if let Some(Value::Object(props)) = m.remove("properties") {
        let cleaned: Map<String, Value> = props.iter().map(|(k, pv)| (k.clone(), clean(pv, opts))).collect();
        m.insert("properties".into(), Value::Object(cleaned));
        m.entry("type").or_insert_with(|| "object".into());
    }
    match m.remove("items") {
        Some(Value::Array(list)) => {
            let first = list.first().cloned().unwrap_or_else(|| json!({ "type": "string" }));
            m.insert("items".into(), clean(&first, opts));
        }
        Some(Value::Bool(_)) => {
            m.insert("items".into(), json!({ "type": "string" }));
        }
        Some(items) => {
            m.insert("items".into(), clean(&items, opts));
        }
        None => {}
    }
    match type_of(&Value::Object(m.clone())) {
        Some("array") => {
            m.entry("items").or_insert_with(|| json!({ "type": "string" }));
        }
        None if m.contains_key("items") => {
            m.insert("type".into(), "array".into());
        }
        Some(_) => {
            m.remove("items");
        }
        None => {}
    }

    // `required` may only name existing properties.
    let names: Vec<String> =
        m.get("properties").and_then(Value::as_object).map(|p| p.keys().cloned().collect()).unwrap_or_default();
    if let Some(Value::Array(req)) = m.remove("required") {
        let req: Vec<Value> =
            req.into_iter().filter(|r| r.as_str().is_some_and(|r| names.iter().any(|n| n == r))).collect();
        if !req.is_empty() {
            m.insert("required".into(), req.into());
        }
    }
    Value::Object(m)
}

/// VALIDATED mode rejects empty objects and objects without required fields.
fn add_placeholders(v: &mut Value, root: bool) {
    let Value::Object(m) = v else { return };
    if let Some(Value::Object(props)) = m.get_mut("properties") {
        for pv in props.values_mut() {
            add_placeholders(pv, false);
        }
    }
    if let Some(items) = m.get_mut("items") {
        add_placeholders(items, false);
    }
    if m.get("type").and_then(Value::as_str) != Some("object") {
        return;
    }
    let empty = m.get("properties").and_then(Value::as_object).is_none_or(|p| p.is_empty());
    if empty {
        m.insert("properties".into(), json!({ "reason": { "type": "string", "description": PLACEHOLDER_REASON } }));
        m.insert("required".into(), json!(["reason"]));
        return;
    }
    if !root && m.get("required").and_then(Value::as_array).is_none_or(|r| r.is_empty()) {
        if let Some(Value::Object(props)) = m.get_mut("properties") {
            props.entry("_").or_insert_with(|| json!({ "type": "boolean" }));
        }
        m.insert("required".into(), json!(["_"]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_unions_and_constraints_are_flattened() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "$defs": { "Mode": { "type": "string", "enum": ["fast", "slow"] } },
            "properties": {
                "mode": { "$ref": "#/$defs/Mode", "description": "speed" },
                "path": { "type": ["string", "null"], "minLength": 1 },
                "count": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
                "tags": { "type": "array" },
                "x-internal": { "type": "string" }
            },
            "required": ["mode", "missing"]
        });
        let out = clean_tool(&schema, false);
        assert!(out.get("$schema").is_none() && out.get("$defs").is_none());
        assert!(out.get("additionalProperties").is_none());
        let p = &out["properties"];
        assert_eq!(p["mode"]["type"], "string");
        assert!(p["mode"]["description"].as_str().unwrap().contains("Allowed: fast, slow"));
        assert_eq!(p["path"]["type"], "string");
        assert_eq!(p["path"]["nullable"], true);
        assert!(p["path"]["description"].as_str().unwrap().contains("minLength: 1"));
        assert_eq!(p["count"]["type"], "integer");
        assert_eq!(p["count"]["nullable"], true);
        assert_eq!(p["tags"]["items"]["type"], "string");
        assert_eq!(out["required"], json!(["mode"]));
    }

    #[test]
    fn validated_mode_adds_placeholders() {
        let out = clean_tool(&json!({ "type": "object", "properties": {} }), true);
        assert_eq!(out["required"], json!(["reason"]));
        let nested = clean_tool(
            &json!({ "type": "object", "properties": { "o": { "type": "object", "properties": { "a": { "type": "string" } } } }, "required": ["o"] }),
            true,
        );
        assert_eq!(nested["properties"]["o"]["required"], json!(["_"]));
    }

    #[test]
    fn recursive_refs_terminate() {
        let schema = json!({
            "type": "object",
            "definitions": { "Node": { "type": "object", "properties": { "next": { "$ref": "#/definitions/Node" } } } },
            "properties": { "root": { "$ref": "#/definitions/Node" } }
        });
        let out = clean_response(&schema);
        assert_eq!(out["properties"]["root"]["properties"]["next"]["type"], "object");
    }
}
