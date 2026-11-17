//! Checking declarations against provider schemas.
//!
//! The evaluated body of a resource is concrete JSON, so it can be checked
//! completely: unknown arguments, missing required ones, read-only ones,
//! value types, and nested blocks with their counts. A string that is a
//! Terraform template ("${...}") has a type only the engine knows; it passes.

use serde_json::{Map as JMap, Value as J};

pub struct Issue {
    pub code: &'static str,
    pub msg: String,
    pub help: Option<String>,
    pub warning: bool,
}

fn issue(code: &'static str, msg: String) -> Issue {
    Issue { code, msg, help: None, warning: false }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Resource,
    Data,
    Provider,
}

const RESOURCE_META: &[&str] =
    &["count", "for_each", "depends_on", "provider", "lifecycle", "provisioner", "connection", "dynamic"];
const DATA_META: &[&str] = &["count", "for_each", "depends_on", "provider", "lifecycle", "dynamic"];
const PROVIDER_META: &[&str] = &["alias"];
const LIFECYCLE: &[&str] = &[
    "create_before_destroy",
    "prevent_destroy",
    "ignore_changes",
    "replace_triggered_by",
    "precondition",
    "postcondition",
];

// ---------------------------------------------------------------- helpers

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The closest candidate, if it is close enough to be a likely typo.
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let limit = (word.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .map(|c| (levenshtein(word, c), c))
        .filter(|(d, _)| *d <= limit)
        .min()
        .map(|(_, c)| c.to_string())
}

/// A Terraform JSON string with an interpolation or directive in it: its
/// value is computed by the engine.
pub fn is_template(s: &str) -> bool {
    let b = s.as_bytes();
    (1..b.len()).any(|i| (b[i - 1] == b'$' || b[i - 1] == b'%') && b[i] == b'{' && (i < 2 || b[i - 2] != b[i - 1]))
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{}.{}", path, key)
    }
}

fn type_name(ty: &J) -> String {
    match ty {
        J::String(s) => s.clone(),
        J::Array(a) => match (a.first().and_then(|v| v.as_str()), a.get(1)) {
            (Some("object"), Some(J::Object(attrs))) => {
                let mut ks: Vec<&String> = attrs.keys().collect();
                ks.sort();
                let shown: Vec<&str> = ks.iter().take(4).map(|k| k.as_str()).collect();
                let more = if ks.len() > 4 { ", ..." } else { "" };
                format!("object({{{}{}}})", shown.join(", "), more)
            }
            (Some("tuple"), Some(J::Array(ts))) => {
                format!("tuple([{}])", ts.iter().map(type_name).collect::<Vec<_>>().join(", "))
            }
            (Some(k), Some(t)) => format!("{}({})", k, type_name(t)),
            _ => "?".into(),
        },
        _ => "?".into(),
    }
}

fn value_name(v: &J) -> String {
    match v {
        J::Null => "nil".into(),
        J::Bool(b) => format!("a boolean ({})", b),
        J::Number(n) => format!("a number ({})", n),
        J::String(s) if s.chars().count() <= 30 => format!("a string ({:?})", s),
        J::String(_) => "a string".into(),
        J::Array(_) => "a vector".into(),
        J::Object(_) => "a map".into(),
    }
}

fn article(t: &str) -> String {
    let t = if t == "bool" { "boolean".to_string() } else { t.to_string() };
    let an = matches!(t.chars().next(), Some('a' | 'e' | 'i' | 'o' | 'u'));
    format!("{} {}", if an { "an" } else { "a" }, t)
}

fn mismatch(path: &str, ty: &J, v: &J) -> Issue {
    let mut i = issue("K0704", format!("`{}` must be {}, got {}", path, article(&type_name(ty)), value_name(v)));
    if let (J::String(t), J::String(s)) = (ty, v) {
        if (t == "number" && s.parse::<f64>().is_ok()) || (t == "bool" && (s == "true" || s == "false")) {
            i.help = Some(format!("write {} without quotes", s));
        }
    }
    i
}

// ---------------------------------------------------------------- values

fn check_value(ty: &J, v: &J, path: &str, out: &mut Vec<Issue>) {
    if v.is_null() {
        return;
    }
    if let J::String(s) = v {
        if is_template(s) {
            return;
        }
    }
    match ty {
        J::String(t) => {
            let ok = match t.as_str() {
                // Terraform converts numbers and booleans to strings.
                "string" => !v.is_array() && !v.is_object(),
                "number" => v.is_number(),
                "bool" => v.is_boolean(),
                _ => true, // "dynamic" and anything newer
            };
            if !ok {
                out.push(mismatch(path, ty, v));
            }
        }
        J::Array(spec) => {
            let kind = spec.first().and_then(|k| k.as_str()).unwrap_or("");
            let arg = spec.get(1).unwrap_or(&J::Null);
            match (kind, v) {
                ("list" | "set", J::Array(items)) => {
                    for (i, item) in items.iter().enumerate() {
                        check_value(arg, item, &format!("{}[{}]", path, i), out);
                    }
                }
                ("map", J::Object(m)) => {
                    for (k, item) in m {
                        check_value(arg, item, &format!("{}[{:?}]", path, k), out);
                    }
                }
                ("object", J::Object(m)) => {
                    let attrs = arg.as_object().cloned().unwrap_or_default();
                    let optional: Vec<&str> = spec
                        .get(2)
                        .and_then(|o| o.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    for (k, item) in m {
                        match attrs.get(k) {
                            Some(t) => check_value(t, item, &join(path, k), out),
                            None => out.push(unknown(&join(path, k), k, attrs.keys().map(|s| s.as_str()))),
                        }
                    }
                    for (k, _) in attrs.iter() {
                        if !m.contains_key(k) && !optional.contains(&k.as_str()) {
                            out.push(issue("K0702", format!("`{}` needs the attribute `{}`", path, k)));
                        }
                    }
                }
                ("tuple", J::Array(items)) => {
                    let ts = arg.as_array().cloned().unwrap_or_default();
                    if ts.len() != items.len() {
                        out.push(mismatch(path, ty, v));
                    } else {
                        for (i, (t, item)) in ts.iter().zip(items).enumerate() {
                            check_value(t, item, &format!("{}[{}]", path, i), out);
                        }
                    }
                }
                ("list" | "set" | "map" | "object" | "tuple", _) => out.push(mismatch(path, ty, v)),
                _ => {}
            }
        }
        _ => {}
    }
}

fn unknown<'a>(path: &str, key: &str, candidates: impl IntoIterator<Item = &'a str>) -> Issue {
    let mut i = issue("K0701", format!("unknown argument `{}`", path));
    if let Some(s) = suggest(key, candidates) {
        i.help = Some(format!("did you mean `{}`?", s));
    }
    i
}

// ---------------------------------------------------------------- blocks

fn check_attr(attr: &J, v: &J, path: &str, out: &mut Vec<Issue>) {
    let flag = |k: &str| attr[k] == J::Bool(true);
    if flag("computed") && !flag("optional") && !flag("required") {
        let mut i = issue("K0703", format!("`{}` is read-only; the provider computes it", path));
        i.help = Some("read it from the resource instead, e.g. (:attr resource)".into());
        out.push(i);
        return;
    }
    if flag("deprecated") {
        let mut i = issue("K0750", format!("`{}` is deprecated by the provider", path));
        i.warning = true;
        out.push(i);
    }
    if let Some(nt) = attr.get("nested_type") {
        check_nested(nt, v, path, out);
    } else if let Some(ty) = attr.get("type") {
        check_value(ty, v, path, out);
    }
}

fn check_nested(nt: &J, v: &J, path: &str, out: &mut Vec<Issue>) {
    if let J::String(s) = v {
        if is_template(s) {
            return;
        }
    }
    let mode = nt["nesting_mode"].as_str().unwrap_or("single");
    match (mode, v) {
        ("single", J::Object(_)) => check_block(nt, v, path, &[], out),
        ("list" | "set", J::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                check_block(nt, item, &format!("{}[{}]", path, i), &[], out);
            }
        }
        ("map", J::Object(m)) => {
            for (k, item) in m {
                check_block(nt, item, &format!("{}[{:?}]", path, k), &[], out);
            }
        }
        _ => out.push(issue(
            "K0704",
            format!(
                "`{}` must be {}, got {}",
                path,
                if mode == "single" { "a map" } else if mode == "map" { "a map of maps" } else { "a vector of maps" },
                value_name(v)
            ),
        )),
    }
}

fn check_block_type(bt: &J, v: &J, path: &str, out: &mut Vec<Issue>) {
    let mode = bt["nesting_mode"].as_str().unwrap_or("list");
    let block = &bt["block"];
    let not_block = |out: &mut Vec<Issue>, what: &str| {
        let mut i = issue("K0705", format!("`{}` is a nested block and must be {}, got {}", path, what, value_name(v)));
        if matches!(v, J::String(s) if is_template(s)) {
            i.help = Some("Terraform cannot compute a block from an expression; build the map in Kiln".into());
        }
        out.push(i);
    };
    match mode {
        "single" | "group" => match v {
            J::Object(_) => check_block(block, v, path, &[], out),
            _ => not_block(out, "a map"),
        },
        "map" => match v {
            J::Object(m) => {
                for (k, item) in m {
                    let p = format!("{}[{:?}]", path, k);
                    if item.is_object() {
                        check_block(block, item, &p, &[], out);
                    } else {
                        out.push(issue("K0705", format!("`{}` must be a map, got {}", p, value_name(item))));
                    }
                }
            }
            _ => not_block(out, "a map of maps"),
        },
        _ => {
            let items: Vec<(String, &J)> = match v {
                J::Object(_) => vec![(path.to_string(), v)],
                J::Array(a) => a.iter().enumerate().map(|(i, x)| (format!("{}[{}]", path, i), x)).collect(),
                _ => return not_block(out, "a map or a vector of maps"),
            };
            let n = items.len() as u64;
            let min = bt["min_items"].as_u64().unwrap_or(0);
            let max = bt["max_items"].as_u64().unwrap_or(0);
            if n < min {
                out.push(issue("K0706", format!("`{}` needs at least {} block{}, got {}", path, min, if min == 1 { "" } else { "s" }, n)));
            }
            if max > 0 && n > max {
                out.push(issue("K0706", format!("`{}` allows at most {} block{}, got {}", path, max, if max == 1 { "" } else { "s" }, n)));
            }
            for (p, item) in items {
                if item.is_object() {
                    check_block(block, item, &p, &[], out);
                } else {
                    out.push(issue("K0705", format!("`{}` must be a map, got {}", p, value_name(item))));
                }
            }
        }
    }
}

fn check_block(block: &J, body: &J, path: &str, meta: &[&str], out: &mut Vec<Issue>) {
    let obj = match body.as_object() {
        Some(o) => o,
        None => return out.push(issue("K0705", format!("`{}` must be a map, got {}", path, value_name(body)))),
    };
    let empty = JMap::new();
    let attrs = block["attributes"].as_object().unwrap_or(&empty);
    let blocks = block["block_types"].as_object().unwrap_or(&empty);
    for (k, v) in obj {
        let p = join(path, k);
        if meta.contains(&k.as_str()) {
            check_meta(k, v, &p, out);
        } else if let Some(a) = attrs.get(k) {
            check_attr(a, v, &p, out);
        } else if let Some(bt) = blocks.get(k) {
            check_block_type(bt, v, &p, out);
        } else {
            let names = attrs.keys().chain(blocks.keys()).map(|s| s.as_str()).chain(meta.iter().copied());
            out.push(unknown(&p, k, names));
        }
    }
    for (k, a) in attrs {
        if a["required"] == J::Bool(true) && !obj.contains_key(k) {
            out.push(issue("K0702", format!("missing required argument `{}`", join(path, k))));
        }
    }
    for (k, bt) in blocks {
        let min = bt["min_items"].as_u64().unwrap_or(0);
        if min > 0 && !obj.contains_key(k) {
            out.push(issue(
                "K0702",
                format!("missing required block `{}` (at least {})", join(path, k), min),
            ));
        }
    }
}

fn check_meta(key: &str, v: &J, path: &str, out: &mut Vec<Issue>) {
    match key {
        "lifecycle" => match v.as_object() {
            Some(m) => {
                for k in m.keys() {
                    if !LIFECYCLE.contains(&k.as_str()) {
                        out.push(unknown(&join(path, k), k, LIFECYCLE.iter().copied()));
                    }
                }
            }
            None => out.push(issue("K0705", format!("`{}` must be a map, got {}", path, value_name(v)))),
        },
        "depends_on" => {
            if !v.as_array().map_or(false, |a| a.iter().all(|x| x.is_string())) {
                out.push(issue("K0704", format!("`{}` must be a vector of resources, got {}", path, value_name(v))));
            }
        }
        "count" => {
            if !(v.is_u64() || matches!(v, J::String(s) if is_template(s))) {
                out.push(issue("K0704", format!("`{}` must be a whole number, got {}", path, value_name(v))));
            }
        }
        "for_each" => {
            let ok = v.is_object() || v.is_array() || matches!(v, J::String(s) if is_template(s));
            if !ok {
                out.push(issue("K0704", format!("`{}` must be a map or a set, got {}", path, value_name(v))));
            }
        }
        _ => {}
    }
}

/// Check one declaration's body against its schema block.
pub fn check_body(kind: Kind, block: &J, body: &J) -> Vec<Issue> {
    let meta = match kind {
        Kind::Resource => RESOURCE_META,
        Kind::Data => DATA_META,
        Kind::Provider => PROVIDER_META,
    };
    let mut out = vec![];
    check_block(block, body, "", meta, &mut out);
    if let Some(o) = body.as_object() {
        if o.contains_key("count") && o.contains_key("for_each") {
            out.push(issue("K0704", "`count` and `for_each` cannot be used together".into()));
        }
    }
    out
}

/// The names readable from a declaration: its attributes and nested blocks.
pub fn readable(block: &J) -> Vec<&str> {
    let mut out: Vec<&str> = vec![];
    for key in ["attributes", "block_types"] {
        if let Some(m) = block[key].as_object() {
            out.extend(m.keys().map(|s| s.as_str()));
        }
    }
    out
}
