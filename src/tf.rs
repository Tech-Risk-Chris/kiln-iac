//! Terraform expression rendering and conversion of values to Terraform JSON.

use crate::value::*;
use serde_json::{Map as JMap, Number, Value as J};

/// Escape literal text for an HCL template: `${` and `%{` would otherwise
/// start an interpolation or a directive.
pub fn escape_template(s: &str) -> String {
    s.replace("${", "$${").replace("%{", "%%{")
}

/// A quoted HCL string literal, for use inside an expression.
pub fn hcl_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in escape_template(s).chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn no_handle<T>(h: &Handle) -> Res<T> {
    Err(KilnError::new(
        "K0203",
        format!("resource handle `{}` used where a value is expected", h.address()),
    )
    .help("read an attribute of it, for example (:id x) or (:name x)"))
}

/// Render any value as an HCL expression (the inside of `${...}`).
pub fn render_expr(v: &Value) -> Res<String> {
    Ok(match v {
        Value::Nil => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{}", f),
        Value::Str(s) => hcl_quote(s),
        Value::Keyword(k) => hcl_quote(k),
        Value::Named(n) => hcl_quote(&n.name),
        Value::List(items, _) | Value::Vector(items) => {
            let parts = items.iter().map(render_expr).collect::<Res<Vec<_>>>()?;
            format!("[{}]", parts.join(", "))
        }
        Value::Map(entries) => {
            let mut parts = vec![];
            for (k, v) in entries.iter() {
                parts.push(format!("{} = {}", hcl_quote(&key_string(k)?), render_expr(v)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        Value::Deferred(e) => render_tf(e)?,
        Value::Handle(h) => return no_handle(h),
        other => {
            return fail(
                "K0103",
                format!("a {} cannot appear in generated Terraform", other.type_name()),
            )
        }
    })
}

pub fn render_tf(e: &TfExpr) -> Res<String> {
    Ok(match e {
        TfExpr::Ref(s) | TfExpr::Raw(s) => s.clone(),
        TfExpr::Attr(base, name) => format!("{}.{}", render_tf(base)?, name),
        TfExpr::Index(base, idx) => format!("{}[{}]", render_tf(base)?, render_expr(idx)?),
        TfExpr::Template(parts) => {
            let mut o = String::from("\"");
            for p in parts {
                match p {
                    TplPart::Lit(s) => {
                        let q = hcl_quote(s);
                        o.push_str(&q[1..q.len() - 1]);
                    }
                    TplPart::Expr(e) => {
                        o.push_str("${");
                        o.push_str(&render_tf(e)?);
                        o.push('}');
                    }
                }
            }
            o.push('"');
            o
        }
        TfExpr::Call(name, args) => {
            let parts = args.iter().map(render_expr).collect::<Res<Vec<_>>>()?;
            format!("{}({})", name, parts.join(", "))
        }
        TfExpr::Bin(op, a, b) => format!("({} {} {})", render_expr(a)?, op, render_expr(b)?),
        TfExpr::Not(a) => format!("!({})", render_expr(a)?),
        TfExpr::Cond(c, a, b) => {
            format!("({} ? {} : {})", render_expr(c)?, render_expr(a)?, render_expr(b)?)
        }
    })
}

pub fn key_string(k: &Value) -> Res<String> {
    match k {
        Value::Keyword(s) | Value::Str(s) => Ok(s.to_string()),
        Value::Int(i) => Ok(i.to_string()),
        other => fail("K0103", format!("a {} cannot be a map key in Terraform", other.type_name())),
    }
}

/// Convert an evaluated value to Terraform JSON. Strings in Terraform JSON are
/// templates, so deferred values become `${...}` and literal text is escaped.
/// Nil entries are dropped, so `(when ...)` inside a map omits the argument.
pub fn to_json(v: &Value) -> Res<J> {
    to_json_in(v, false)
}

fn to_json_in(v: &Value, refs: bool) -> Res<J> {
    Ok(match v {
        Value::Nil => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::Number((*i).into()),
        Value::Float(f) => match Number::from_f64(*f) {
            Some(n) => J::Number(n),
            None => return fail("K0103", "non-finite number in generated Terraform"),
        },
        Value::Str(s) => J::String(escape_template(s)),
        Value::Keyword(k) => J::String(escape_template(k)),
        Value::Named(n) => J::String(escape_template(&n.name)),
        Value::List(items, _) | Value::Vector(items) => {
            let mut out = vec![];
            for it in items.iter() {
                if !matches!(it, Value::Nil) {
                    out.push(to_json_in(it, refs)?);
                }
            }
            J::Array(out)
        }
        Value::Map(entries) => {
            let mut m = JMap::new();
            for (k, v) in entries.iter() {
                if matches!(v, Value::Nil) {
                    continue;
                }
                let key = key_string(k)?;
                let as_refs = key == "depends_on";
                m.insert(key, to_json_in(v, as_refs)?);
            }
            J::Object(m)
        }
        Value::Deferred(e) => match &**e {
            TfExpr::Template(parts) => {
                let mut o = String::new();
                for p in parts {
                    match p {
                        TplPart::Lit(s) => o.push_str(&escape_template(s)),
                        TplPart::Expr(e) => {
                            o.push_str("${");
                            o.push_str(&render_tf(e)?);
                            o.push('}');
                        }
                    }
                }
                J::String(o)
            }
            other => J::String(format!("${{{}}}", render_tf(other)?)),
        },
        Value::Handle(h) => {
            if refs {
                J::String(h.address())
            } else {
                return no_handle(h);
            }
        }
        other => {
            return fail(
                "K0103",
                format!("a {} cannot appear in generated Terraform", other.type_name()),
            )
        }
    })
}
