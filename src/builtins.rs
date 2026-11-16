//! Built-in functions. Operations that Terraform can also perform (string
//! concatenation, arithmetic, comparison, lookup) lift over deferred values:
//! given one, they return a Terraform expression instead of a result.

use crate::eval::{get_value, seq_of, Interp};
use crate::value::*;
use std::rc::Rc;

pub const NAMES: &[&str] = &[
    "str", "+", "-", "*", "/", "mod", "inc", "dec", "min", "max", "=", "not=", "<", ">", "<=",
    ">=", "not", "merge", "merge-deep", "assoc", "dissoc", "get", "get-in", "select-keys",
    "update", "zipmap", "keys", "vals", "contains?", "conj", "concat", "count", "first", "second",
    "last", "rest", "nth", "vec", "list", "range", "empty?", "nil?", "some?", "flatten", "map",
    "mapcat", "filter", "reduce", "apply", "identity", "name", "keyword", "slug", "join", "lower",
    "upper", "fail",
];

fn need(name: &str, a: &[Value], n: usize) -> Res<()> {
    if a.len() != n {
        return fail(
            "K0102",
            format!("`{}` takes {} argument{}, got {}", name, n, if n == 1 { "" } else { "s" }, a.len()),
        );
    }
    Ok(())
}

fn need_min(name: &str, a: &[Value], n: usize) -> Res<()> {
    if a.len() < n {
        return fail("K0102", format!("`{}` takes at least {} arguments, got {}", name, n, a.len()));
    }
    Ok(())
}

fn deferred(e: TfExpr) -> Value {
    Value::Deferred(Rc::new(e))
}

fn to_text(v: &Value) -> Res<String> {
    Ok(match v {
        Value::Nil => String::new(),
        Value::Str(s) | Value::Symbol(s) => s.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Keyword(k) => format!(":{}", k),
        Value::Handle(h) => {
            return Err(KilnError::new(
                "K0203",
                format!("resource handle `{}` used where a value is expected", h.address()),
            )
            .help("read an attribute of it, for example (:id x) or (:name x)"))
        }
        other => pr_str(other),
    })
}

fn str_concat(args: &[Value]) -> Res<Value> {
    let mut parts: Vec<TplPart> = vec![];
    let mut any_deferred = false;
    let push_lit = |parts: &mut Vec<TplPart>, s: &str| {
        if s.is_empty() {
            return;
        }
        if let Some(TplPart::Lit(l)) = parts.last_mut() {
            l.push_str(s);
        } else {
            parts.push(TplPart::Lit(s.to_string()));
        }
    };
    for a in args {
        match a {
            Value::Deferred(e) => {
                any_deferred = true;
                match &**e {
                    TfExpr::Template(ps) => {
                        for p in ps {
                            match p {
                                TplPart::Lit(s) => push_lit(&mut parts, s),
                                TplPart::Expr(e) => parts.push(TplPart::Expr(e.clone())),
                            }
                        }
                    }
                    _ => parts.push(TplPart::Expr(e.clone())),
                }
            }
            other => push_lit(&mut parts, &to_text(other)?),
        }
    }
    if any_deferred {
        return Ok(deferred(TfExpr::Template(parts)));
    }
    let mut s = String::new();
    for p in parts {
        if let TplPart::Lit(l) = p {
            s.push_str(&l);
        }
    }
    Ok(Value::str(s))
}

#[derive(Clone, Copy)]
enum Num {
    I(i64),
    F(f64),
}

fn num(name: &str, v: &Value) -> Res<Num> {
    match v {
        Value::Int(i) => Ok(Num::I(*i)),
        Value::Float(f) => Ok(Num::F(*f)),
        other => fail("K0103", format!("`{}` needs numbers, got a {}", name, other.type_name())),
    }
}

fn num_val(n: Num) -> Value {
    match n {
        Num::I(i) => Value::Int(i),
        Num::F(f) => Value::Float(f),
    }
}

fn f(n: Num) -> f64 {
    match n {
        Num::I(i) => i as f64,
        Num::F(f) => f,
    }
}

fn arith2(name: &'static str, a: &Value, b: &Value) -> Res<Value> {
    if a.is_deferred() || b.is_deferred() {
        let op = if name == "mod" { "%" } else { name };
        return Ok(deferred(TfExpr::Bin(op, a.clone(), b.clone())));
    }
    let (x, y) = (num(name, a)?, num(name, b)?);
    let overflow = || KilnError::new("K0105", "integer overflow");
    Ok(num_val(match (name, x, y) {
        ("+", Num::I(a), Num::I(b)) => Num::I(a.checked_add(b).ok_or_else(overflow)?),
        ("-", Num::I(a), Num::I(b)) => Num::I(a.checked_sub(b).ok_or_else(overflow)?),
        ("*", Num::I(a), Num::I(b)) => Num::I(a.checked_mul(b).ok_or_else(overflow)?),
        ("/", _, Num::I(0)) | ("mod", _, Num::I(0)) => return fail("K0105", "division by zero"),
        ("/", Num::I(a), Num::I(b)) if a % b == 0 => Num::I(a / b),
        ("mod", Num::I(a), Num::I(b)) => Num::I(a.rem_euclid(b)),
        ("+", a, b) => Num::F(f(a) + f(b)),
        ("-", a, b) => Num::F(f(a) - f(b)),
        ("*", a, b) => Num::F(f(a) * f(b)),
        ("/", a, b) => Num::F(f(a) / f(b)),
        ("mod", a, b) => Num::F(f(a).rem_euclid(f(b))),
        _ => unreachable!(),
    }))
}

fn arith(name: &'static str, a: &[Value]) -> Res<Value> {
    match (name, a.len()) {
        ("+", 0) => return Ok(Value::Int(0)),
        ("*", 0) => return Ok(Value::Int(1)),
        ("-", 1) => return arith2("-", &Value::Int(0), &a[0]),
        (_, 0) => return fail("K0102", format!("`{}` needs at least one argument", name)),
        _ => {}
    }
    let mut acc = a[0].clone();
    if a.len() == 1 && !acc.is_deferred() {
        num(name, &acc)?;
    }
    for x in &a[1..] {
        acc = arith2(name, &acc, x)?;
    }
    Ok(acc)
}

fn compare(name: &'static str, a: &[Value]) -> Res<Value> {
    need(name, a, 2)?;
    if a[0].is_deferred() || a[1].is_deferred() {
        return Ok(deferred(TfExpr::Bin(name, a[0].clone(), a[1].clone())));
    }
    let ord = match (&a[0], &a[1]) {
        (Value::Str(x), Value::Str(y)) => x.partial_cmp(y),
        (x, y) => f(num(name, x)?).partial_cmp(&f(num(name, y)?)),
    };
    let ord = ord.ok_or_else(|| KilnError::new("K0103", "values cannot be ordered"))?;
    Ok(Value::Bool(match name {
        "<" => ord.is_lt(),
        ">" => ord.is_gt(),
        "<=" => ord.is_le(),
        _ => ord.is_ge(),
    }))
}

fn as_map(name: &str, v: &Value) -> Res<Vec<(Value, Value)>> {
    match v {
        Value::Nil => Ok(vec![]),
        Value::Map(e) => Ok(e.to_vec()),
        other => fail("K0103", format!("`{}` needs a map, got a {}", name, other.type_name())),
    }
}

fn merge_deep(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Map(x), Value::Map(y)) => {
            let mut out = x.to_vec();
            for (k, v) in y.iter() {
                let merged = match map_get(&out, k) {
                    Some(old) => merge_deep(&old, v),
                    None => v.clone(),
                };
                map_assoc(&mut out, k.clone(), merged);
            }
            Value::map(out)
        }
        _ => b.clone(),
    }
}

fn text_arg(name: &str, v: &Value) -> Res<String> {
    match v {
        Value::Str(s) | Value::Keyword(s) | Value::Symbol(s) => Ok(s.to_string()),
        Value::Deferred(_) => Err(KilnError::new(
            "K0316",
            format!("`{}` needs a compile-time string, got a value known only after apply", name),
        )
        .help(format!("use the Terraform function instead, e.g. (tf/{} x)", name))),
        other => fail("K0103", format!("`{}` needs a string, got a {}", name, other.type_name())),
    }
}

pub fn slug(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            o.push(c);
        } else if !o.ends_with('_') {
            o.push('_');
        }
    }
    let o = o.trim_matches('_').to_string();
    match o.chars().next() {
        Some(c) if c.is_ascii_digit() => format!("_{}", o),
        None => "_".into(),
        _ => o,
    }
}

fn flatten(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Vector(items) | Value::List(items, _) => items.iter().for_each(|i| flatten(i, out)),
        Value::Nil => {}
        other => out.push(other.clone()),
    }
}

pub fn call(it: &mut Interp, name: &'static str, a: Vec<Value>) -> Res<Value> {
    match name {
        "str" => str_concat(&a),
        "+" | "-" | "*" | "/" => arith(name, &a),
        "mod" => {
            need(name, &a, 2)?;
            arith2("mod", &a[0], &a[1])
        }
        "inc" => {
            need(name, &a, 1)?;
            arith2("+", &a[0], &Value::Int(1))
        }
        "dec" => {
            need(name, &a, 1)?;
            arith2("-", &a[0], &Value::Int(1))
        }
        "min" | "max" => {
            need_min(name, &a, 1)?;
            if a.iter().any(|v| v.is_deferred()) {
                return Ok(deferred(TfExpr::Call(name.to_string(), a)));
            }
            let mut best = num(name, &a[0])?;
            for x in &a[1..] {
                let n = num(name, x)?;
                if (name == "min") == (f(n) < f(best)) && f(n) != f(best) {
                    best = n;
                }
            }
            Ok(num_val(best))
        }
        "=" | "not=" => {
            need(name, &a, 2)?;
            if a[0].is_deferred() || a[1].is_deferred() {
                let op = if name == "=" { "==" } else { "!=" };
                return Ok(deferred(TfExpr::Bin(op, a[0].clone(), a[1].clone())));
            }
            Ok(Value::Bool((a[0] == a[1]) == (name == "=")))
        }
        "<" | ">" | "<=" | ">=" => compare(name, &a),
        "not" => {
            need(name, &a, 1)?;
            if a[0].is_deferred() {
                return Ok(deferred(TfExpr::Not(a[0].clone())));
            }
            Ok(Value::Bool(!a[0].truthy()))
        }
        "merge" => {
            let mut out = vec![];
            for m in &a {
                for (k, v) in as_map(name, m)? {
                    map_assoc(&mut out, k, v);
                }
            }
            Ok(Value::map(out))
        }
        "merge-deep" => {
            let mut acc = Value::map(vec![]);
            for m in &a {
                as_map(name, m)?;
                if !matches!(m, Value::Nil) {
                    acc = merge_deep(&acc, m);
                }
            }
            Ok(acc)
        }
        "assoc" => {
            if a.len() < 3 || a.len() % 2 == 0 {
                return fail("K0102", "(assoc map key value ...)");
            }
            let mut out = as_map(name, &a[0])?;
            for pair in a[1..].chunks(2) {
                map_assoc(&mut out, pair[0].clone(), pair[1].clone());
            }
            Ok(Value::map(out))
        }
        "dissoc" => {
            need_min(name, &a, 1)?;
            let mut out = as_map(name, &a[0])?;
            out.retain(|(k, _)| !a[1..].contains(k));
            Ok(Value::map(out))
        }
        "get" => match a.len() {
            2 => get_value(&a[0], &a[1]),
            3 => {
                let v = get_value(&a[0], &a[1])?;
                Ok(if matches!(v, Value::Nil) { a[2].clone() } else { v })
            }
            _ => fail("K0102", "(get collection key default?)"),
        },
        "get-in" => {
            need(name, &a, 2)?;
            let mut cur = a[0].clone();
            for k in seq_of(&a[1])? {
                cur = get_value(&cur, &k)?;
            }
            Ok(cur)
        }
        "select-keys" => {
            need(name, &a, 2)?;
            let m = as_map(name, &a[0])?;
            let ks = seq_of(&a[1])?;
            Ok(Value::map(m.into_iter().filter(|(k, _)| ks.contains(k)).collect()))
        }
        "update" => {
            need_min(name, &a, 3)?;
            let mut out = as_map(name, &a[0])?;
            let old = map_get(&out, &a[1]).unwrap_or(Value::Nil);
            let mut args = vec![old];
            args.extend(a[3..].iter().cloned());
            let new = it.apply(&a[2], args)?;
            map_assoc(&mut out, a[1].clone(), new);
            Ok(Value::map(out))
        }
        "zipmap" => {
            need(name, &a, 2)?;
            let mut out = vec![];
            for (k, v) in seq_of(&a[0])?.into_iter().zip(seq_of(&a[1])?) {
                map_assoc(&mut out, k, v);
            }
            Ok(Value::map(out))
        }
        "keys" => {
            need(name, &a, 1)?;
            Ok(Value::vector(as_map(name, &a[0])?.into_iter().map(|(k, _)| k).collect()))
        }
        "vals" => {
            need(name, &a, 1)?;
            Ok(Value::vector(as_map(name, &a[0])?.into_iter().map(|(_, v)| v).collect()))
        }
        "contains?" => {
            need(name, &a, 2)?;
            Ok(Value::Bool(as_map(name, &a[0])?.iter().any(|(k, _)| *k == a[1])))
        }
        "conj" => {
            need_min(name, &a, 1)?;
            let mut out = seq_of(&a[0])?;
            if let Value::Map(_) = &a[0] {
                return fail("K0103", "conj works on vectors; use assoc or merge for maps");
            }
            out.extend(a[1..].iter().cloned());
            Ok(Value::vector(out))
        }
        "concat" => {
            let mut out = vec![];
            for c in &a {
                out.extend(seq_of(c)?);
            }
            Ok(Value::vector(out))
        }
        "count" => {
            need(name, &a, 1)?;
            Ok(Value::Int(match &a[0] {
                Value::Str(s) => s.chars().count() as i64,
                other => seq_of(other)?.len() as i64,
            }))
        }
        "first" | "second" | "last" => {
            need(name, &a, 1)?;
            let s = seq_of(&a[0])?;
            let v = match name {
                "first" => s.first(),
                "second" => s.get(1),
                _ => s.last(),
            };
            Ok(v.cloned().unwrap_or(Value::Nil))
        }
        "rest" => {
            need(name, &a, 1)?;
            Ok(Value::vector(seq_of(&a[0])?.into_iter().skip(1).collect()))
        }
        "nth" => {
            need(name, &a, 2)?;
            let s = seq_of(&a[0])?;
            match &a[1] {
                Value::Int(i) if *i >= 0 && (*i as usize) < s.len() => Ok(s[*i as usize].clone()),
                Value::Int(i) => fail("K0105", format!("index {} out of range for {} elements", i, s.len())),
                other => fail("K0103", format!("`nth` needs an integer index, got a {}", other.type_name())),
            }
        }
        "vec" => {
            need(name, &a, 1)?;
            Ok(Value::vector(seq_of(&a[0])?))
        }
        "list" => Ok(Value::list(a)),
        "range" => {
            let (lo, hi) = match a.as_slice() {
                [Value::Int(n)] => (0, *n),
                [Value::Int(lo), Value::Int(hi)] => (*lo, *hi),
                _ => return fail("K0102", "(range end) or (range start end), with integers"),
            };
            if hi - lo > 100_000 {
                return fail("K0105", "range is limited to 100000 elements");
            }
            Ok(Value::vector((lo..hi).map(Value::Int).collect()))
        }
        "empty?" => {
            need(name, &a, 1)?;
            Ok(Value::Bool(match &a[0] {
                Value::Str(s) => s.is_empty(),
                other => seq_of(other)?.is_empty(),
            }))
        }
        "nil?" => {
            need(name, &a, 1)?;
            Ok(Value::Bool(matches!(a[0], Value::Nil)))
        }
        "some?" => {
            need(name, &a, 1)?;
            Ok(Value::Bool(!matches!(a[0], Value::Nil)))
        }
        "flatten" => {
            need(name, &a, 1)?;
            let mut out = vec![];
            flatten(&a[0], &mut out);
            Ok(Value::vector(out))
        }
        "map" | "mapcat" => {
            need_min(name, &a, 2)?;
            let colls = a[1..].iter().map(seq_of).collect::<Res<Vec<_>>>()?;
            let n = colls.iter().map(|c| c.len()).min().unwrap_or(0);
            let mut out = vec![];
            for i in 0..n {
                let args = colls.iter().map(|c| c[i].clone()).collect();
                let r = it.apply(&a[0], args)?;
                if name == "mapcat" {
                    out.extend(seq_of(&r)?);
                } else {
                    out.push(r);
                }
            }
            Ok(Value::vector(out))
        }
        "filter" => {
            need(name, &a, 2)?;
            let mut out = vec![];
            for x in seq_of(&a[1])? {
                let keep = it.apply(&a[0], vec![x.clone()])?;
                if keep.is_deferred() {
                    return Err(KilnError::new(
                        "K0312",
                        "filter predicate returned a value known only after apply",
                    )
                    .help("filter on compile-time data"));
                }
                if keep.truthy() {
                    out.push(x);
                }
            }
            Ok(Value::vector(out))
        }
        "reduce" => {
            let (mut acc, items) = match a.len() {
                2 => {
                    let mut s = seq_of(&a[1])?;
                    if s.is_empty() {
                        return it.apply(&a[0], vec![]);
                    }
                    let first = s.remove(0);
                    (first, s)
                }
                3 => (a[1].clone(), seq_of(&a[2])?),
                _ => return fail("K0102", "(reduce f init? collection)"),
            };
            for x in items {
                acc = it.apply(&a[0], vec![acc, x])?;
            }
            Ok(acc)
        }
        "apply" => {
            need_min(name, &a, 2)?;
            let mut args: Vec<Value> = a[1..a.len() - 1].to_vec();
            args.extend(seq_of(&a[a.len() - 1])?);
            it.apply(&a[0], args)
        }
        "identity" => {
            need(name, &a, 1)?;
            Ok(a[0].clone())
        }
        "name" => {
            need(name, &a, 1)?;
            Ok(Value::str(text_arg(name, &a[0])?))
        }
        "keyword" => {
            need(name, &a, 1)?;
            Ok(Value::kw(&text_arg(name, &a[0])?))
        }
        "slug" => {
            need(name, &a, 1)?;
            Ok(Value::str(slug(&text_arg(name, &a[0])?)))
        }
        "lower" => {
            need(name, &a, 1)?;
            Ok(Value::str(text_arg(name, &a[0])?.to_lowercase()))
        }
        "upper" => {
            need(name, &a, 1)?;
            Ok(Value::str(text_arg(name, &a[0])?.to_uppercase()))
        }
        "join" => {
            need(name, &a, 2)?;
            let sep = text_arg(name, &a[0])?;
            let items = seq_of(&a[1])?;
            if items.iter().any(|v| v.is_deferred()) {
                return Ok(deferred(TfExpr::Call(
                    "join".into(),
                    vec![Value::str(sep), Value::vector(items)],
                )));
            }
            let parts = items.iter().map(to_text_pub).collect::<Res<Vec<_>>>()?;
            Ok(Value::str(parts.join(&sep)))
        }
        "fail" => {
            let msg = match str_concat(&a)? {
                Value::Str(s) => s.to_string(),
                _ => "failure message depends on a value known only after apply".into(),
            };
            fail("K0900", msg)
        }
        _ => fail("K0100", format!("unknown builtin `{}`", name)),
    }
}

fn to_text_pub(v: &Value) -> Res<String> {
    to_text(v)
}
