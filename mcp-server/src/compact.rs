//! Compact `tools/list` output, opt-in via `JDWP_COMPACT_TOOLS=1`.
//!
//! The full tool set advertises ~290 KB of descriptions and draft-2020-12 schemas (`$defs`/`$ref`,
//! `anyOf`, `["string","null"]` unions, non-standard `format`s such as `uint64`). Some MCP hosts forward
//! that verbatim to model providers whose function-calling APIs reject it — the Windsurf `JetBrains` plugin
//! fails every chat with "Invalid argument: an internal error occurred". This rewrites the advertised
//! metadata only: tool names, argument names and what the handlers accept are unchanged, so every call a
//! compact schema permits is one the full schema permits too.

use crate::protocol::Tool;
use serde_json::{Map, Value};

pub const ENV_VAR: &str = "JDWP_COMPACT_TOOLS";

const TOOL_DESC_BUDGET: usize = 350;
const TOOL_DESC_MAX: usize = 450;
const PROP_DESC_BUDGET: usize = 140;
const PROP_DESC_MAX: usize = 220;
const MAX_REF_DEPTH: u8 = 8;

/// Whether `JDWP_COMPACT_TOOLS` asks for compact tool metadata.
#[must_use]
pub fn enabled() -> bool {
    std::env::var(ENV_VAR)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Shorten every tool's description and flatten its input schema to plain JSON Schema.
#[must_use]
pub fn compact_tools(tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|t| Tool {
            description: summarize(&t.description, TOOL_DESC_BUDGET, TOOL_DESC_MAX),
            input_schema: compact_schema(t.input_schema),
            name: t.name,
        })
        .collect()
}

fn compact_schema(mut schema: Value) -> Value {
    let defs = match schema.as_object_mut().and_then(|o| o.remove("$defs")) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut out = simplify(schema, &defs, 0);
    if let Some(o) = out.as_object_mut() {
        o.remove("description");
    }
    out
}

fn simplify(node: Value, defs: &Map<String, Value>, depth: u8) -> Value {
    match node {
        Value::Array(items) => Value::Array(items.into_iter().map(|v| simplify(v, defs, depth)).collect()),
        Value::Object(obj) => simplify_object(obj, defs, depth),
        other => other,
    }
}

fn merge_under(obj: &mut Map<String, Value>, from: Map<String, Value>) {
    for (k, v) in from {
        obj.entry(k).or_insert(v);
    }
}

fn simplify_object(mut obj: Map<String, Value>, defs: &Map<String, Value>, depth: u8) -> Value {
    if depth < MAX_REF_DEPTH {
        let target = obj
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| defs.get(name))
            .and_then(Value::as_object)
            .cloned();
        if let Some(target) = target {
            obj.remove("$ref");
            merge_under(&mut obj, target);
            return simplify_object(obj, defs, depth + 1);
        }
    }

    if let Some(Value::Array(variants)) = obj.remove("anyOf") {
        let mut kept: Vec<Value> = variants
            .into_iter()
            .map(|v| simplify(v, defs, depth + 1))
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        // `string | string[]` unions (ClassPatterns, TraceExprs): the handlers accept both, and an array
        // is the shape that can say everything the string can.
        let pick =
            kept.iter().position(|v| v.get("type").and_then(Value::as_str) == Some("array")).unwrap_or(0);
        if pick < kept.len() {
            if let Value::Object(chosen) = kept.swap_remove(pick) {
                merge_under(&mut obj, chosen);
            }
        }
    }

    let mut out = Map::new();
    for (key, value) in obj {
        match key.as_str() {
            "$schema" | "$defs" | "$ref" | "title" | "format" | "minimum" | "maximum" => {}
            "default" if value.is_null() => {}
            "type" => {
                out.insert(key, simplify_type(value));
            }
            "description" => {
                if let Value::String(s) = value {
                    out.insert(key, Value::String(summarize(&s, PROP_DESC_BUDGET, PROP_DESC_MAX)));
                }
            }
            "properties" => {
                let value = match value {
                    Value::Object(props) => {
                        Value::Object(props.into_iter().map(|(k, v)| (k, simplify(v, defs, depth))).collect())
                    }
                    other => other,
                };
                out.insert(key, value);
            }
            _ => {
                out.insert(key, simplify(value, defs, depth));
            }
        }
    }
    Value::Object(out)
}

fn simplify_type(t: Value) -> Value {
    match t {
        Value::Array(types) => {
            let mut non_null: Vec<Value> = types.into_iter().filter(|x| x.as_str() != Some("null")).collect();
            match non_null.len() {
                0 => Value::String("null".to_string()),
                1 => non_null.pop().unwrap_or(Value::Null),
                _ => Value::Array(non_null),
            }
        }
        other => other,
    }
}

/// Byte offsets just past each sentence-ending punctuation mark, skipping `e.g.` / `i.e.` / `etc.` / `vs.`.
fn sentence_ends(text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let at_break = chars.peek().is_none_or(|&(_, next)| next.is_whitespace());
        if !at_break {
            continue;
        }
        let before = text.get(..i).unwrap_or_default().to_ascii_lowercase();
        if c == '.' && ["e.g", "i.e", "etc", "vs"].iter().any(|abbr| before.ends_with(abbr)) {
            continue;
        }
        ends.push(i + c.len_utf8());
    }
    ends
}

/// Whole leading sentences up to `budget` bytes; if even the first runs past `max`, a word-boundary cut
/// with an ellipsis.
fn summarize(text: &str, budget: usize, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() <= budget {
        return text;
    }
    let ends = sentence_ends(&text);
    let mut end = None;
    for &e in &ends {
        if e <= budget || end.is_none() {
            end = Some(e);
        }
        if e > budget {
            break;
        }
    }
    if let Some(e) = end.filter(|&e| e <= max) {
        return text.get(..e).unwrap_or(&text).to_string();
    }
    let cut = text.char_indices().map(|(i, _)| i).take_while(|&i| i <= max).last().unwrap_or(0);
    let head = text.get(..cut).unwrap_or_default();
    let head = head.rfind(' ').and_then(|sp| head.get(..sp)).unwrap_or(head);
    format!("{}…", head.trim_end_matches([',', ';', ':', ' ', '—']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn walk(v: &Value, f: &mut impl FnMut(&Map<String, Value>)) {
        match v {
            Value::Object(o) => {
                f(o);
                o.values().for_each(|c| walk(c, f));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
            _ => {}
        }
    }

    #[test]
    fn every_tool_survives_with_the_same_names_and_arguments() {
        let full = crate::tools::get_tools();
        let compact = compact_tools(full.clone());
        assert_eq!(full.len(), compact.len());
        for (f, c) in full.iter().zip(&compact) {
            assert_eq!(f.name, c.name);
            let keys = |t: &Tool| {
                let mut k: Vec<String> =
                    t.input_schema["properties"].as_object().unwrap().keys().cloned().collect();
                k.sort();
                k
            };
            assert_eq!(keys(f), keys(c), "{} lost or gained an argument", f.name);
            assert_eq!(f.input_schema.get("required"), c.input_schema.get("required"), "{}", f.name);
            assert!(c.description.len() <= TOOL_DESC_MAX + 4, "{} description too long", c.name);
            assert!(!c.description.is_empty(), "{} lost its description", c.name);
        }
    }

    #[test]
    fn compact_schemas_use_only_plain_json_schema() {
        for tool in compact_tools(crate::tools::get_tools()) {
            walk(&tool.input_schema, &mut |o| {
                for banned in ["$ref", "$defs", "$schema", "anyOf", "oneOf", "allOf", "format", "title"] {
                    assert!(!o.contains_key(banned), "{} still has {banned}", tool.name);
                }
                if let Some(t) = o.get("type") {
                    assert!(t.is_string(), "{} has a union type {t}", tool.name);
                }
            });
        }
    }

    #[test]
    fn the_whole_list_shrinks_by_an_order_of_magnitude() {
        let full = serde_json::to_string(&crate::tools::get_tools()).unwrap().len();
        let compact = serde_json::to_string(&compact_tools(crate::tools::get_tools())).unwrap().len();
        assert!(compact * 5 < full, "compact {compact} B vs full {full} B");
    }

    #[test]
    fn nullable_ref_to_string_or_array_becomes_an_array_of_strings() {
        let schema = json!({
            "type": "object",
            "$defs": {"P": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]}},
            "properties": {"p": {"anyOf": [{"$ref": "#/$defs/P"}, {"type": "null"}], "default": null, "description": "Pat."}}
        });
        let out = compact_schema(schema);
        assert_eq!(
            out["properties"]["p"],
            json!({"type": "array", "items": {"type": "string"}, "description": "Pat."})
        );
    }

    #[test]
    fn summarize_keeps_whole_sentences_and_skips_abbreviations() {
        let text = "Set a value, e.g. a local. Second sentence here. Third.";
        assert_eq!(summarize(text, 40, 80), "Set a value, e.g. a local.");
        assert_eq!(summarize(text, 50, 80), "Set a value, e.g. a local. Second sentence here.");
        let long = "word ".repeat(100);
        let s = summarize(&long, 20, 30);
        assert!(s.ends_with('…') && s.len() <= 34, "{s}");
    }

    #[test]
    fn nullable_types_collapse() {
        assert_eq!(simplify_type(json!(["string", "null"])), json!("string"));
        assert_eq!(simplify_type(json!("integer")), json!("integer"));
    }
}
