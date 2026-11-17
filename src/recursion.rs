//! Structural recursion.
//!
//! A function may call itself only on a strictly smaller part of one of its
//! parameters, and it must be the same parameter in every recursive call.
//! The check here is syntactic, on the macro-expanded body, and picks that
//! parameter. At run time the evaluator also checks that the parameter's
//! structural size really shrinks on each call, which closes the remaining
//! hole (`(rest [])` is `[]`) and makes termination a guarantee.

use crate::value::*;
use std::collections::HashMap;

/// How a local name relates to a parameter: equal to it, or a strict part.
#[derive(Clone, Copy, PartialEq)]
struct Rel {
    param: usize,
    strict: bool,
}

#[derive(Clone, Default)]
struct Scope {
    rels: HashMap<String, Rel>,
    /// The function's own name is rebound here.
    shadowed: bool,
}

struct Walk<'a> {
    name: &'a str,
    arity: usize,
    /// For each recursive call: the parameters it makes smaller.
    calls: Vec<(Vec<usize>, Option<Pos>)>,
    error: Option<KilnError>,
}

const PART_OF: &[&str] = &["rest", "next", "first", "second", "last", "nth", "get", "get-in", "vals"];
const OVER_ELEMENTS: &[&str] = &["map", "mapcat", "filter"];

fn names_in(pat: &Value, out: &mut Vec<String>) {
    match pat {
        Value::Symbol(s) if &**s != "&" => out.push(s.to_string()),
        Value::Vector(items) => items.iter().for_each(|p| names_in(p, out)),
        Value::Map(entries) => {
            for (k, v) in entries.iter() {
                match k {
                    Value::Keyword(kw) if &**kw == "keys" => {
                        if let Value::Vector(syms) = v {
                            syms.iter().for_each(|p| names_in(p, out));
                        }
                    }
                    Value::Keyword(kw) if &**kw == "as" => names_in(v, out),
                    Value::Keyword(kw) if &**kw == "or" => {}
                    p => names_in(p, out),
                }
            }
        }
        _ => {}
    }
}

impl<'a> Walk<'a> {
    fn rel(&self, e: &Value, sc: &Scope) -> Option<Rel> {
        match e {
            Value::Symbol(s) => sc.rels.get(&**s).copied(),
            Value::List(items, _) if items.len() >= 2 => {
                let part = match &items[0] {
                    Value::Symbol(f) => PART_OF.contains(&&**f),
                    Value::Keyword(_) => true, // (:children node)
                    _ => false,
                };
                if part {
                    self.rel(&items[1], sc).map(|r| Rel { param: r.param, strict: true })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Bind a pattern to a value with relation `r`: a plain name keeps the
    /// relation, a destructured part is strictly smaller.
    fn bind(&self, pat: &Value, r: Option<Rel>, sc: &mut Scope) {
        let mut names = vec![];
        names_in(pat, &mut names);
        let destructured = !matches!(pat, Value::Symbol(_));
        for n in names {
            if n == self.name {
                sc.shadowed = true;
            }
            match r {
                Some(r) => {
                    sc.rels.insert(n, Rel { param: r.param, strict: r.strict || destructured });
                }
                None => {
                    sc.rels.remove(&n);
                }
            }
        }
    }

    fn call(&mut self, args: &[Value], sc: &Scope, pos: Option<Pos>) {
        let smaller = (0..self.arity)
            .filter(|i| matches!(args.get(*i).and_then(|a| self.rel(a, sc)), Some(Rel { param, strict: true }) if param == *i))
            .collect();
        self.calls.push((smaller, pos));
    }

    fn walk(&mut self, form: &Value, sc: &Scope) {
        if self.error.is_some() {
            return;
        }
        match form {
            Value::Symbol(s) if &**s == self.name && !sc.shadowed => {
                self.error = Some(KilnError::new(
                    "K0403",
                    format!("`{}` may only call itself directly, or be passed to map, mapcat or filter", self.name),
                ));
            }
            Value::Vector(items) => items.iter().for_each(|i| self.walk(i, sc)),
            Value::Map(entries) => entries.iter().for_each(|(k, v)| {
                self.walk(k, sc);
                self.walk(v, sc)
            }),
            Value::List(items, pos) if !items.is_empty() => {
                let head = items[0].sym_name().unwrap_or("");
                match head {
                    "quote" => {}
                    _ if head == self.name && !sc.shadowed => {
                        self.call(&items[1..], sc, *pos);
                        items[1..].iter().for_each(|a| self.walk(a, sc));
                    }
                    "let" => {
                        let mut inner = sc.clone();
                        if let Some(Value::Vector(binds)) = items.get(1) {
                            for pair in binds.chunks(2) {
                                if pair.len() == 2 {
                                    self.walk(&pair[1], &inner);
                                    let r = self.rel(&pair[1], &inner);
                                    self.bind(&pair[0], r, &mut inner);
                                }
                            }
                        }
                        items[2..].iter().for_each(|b| self.walk(b, &inner));
                    }
                    "for" => {
                        let mut inner = sc.clone();
                        if let Some(Value::Vector(binds)) = items.get(1) {
                            for pair in binds.chunks(2) {
                                if pair.len() != 2 {
                                    continue;
                                }
                                match &pair[0] {
                                    Value::Keyword(k) if &**k == "when" => self.walk(&pair[1], &inner),
                                    Value::Keyword(k) if &**k == "let" => {
                                        if let Value::Vector(lb) = &pair[1] {
                                            for p in lb.chunks(2) {
                                                if p.len() == 2 {
                                                    self.walk(&p[1], &inner);
                                                    let r = self.rel(&p[1], &inner);
                                                    self.bind(&p[0], r, &mut inner);
                                                }
                                            }
                                        }
                                    }
                                    pat => {
                                        self.walk(&pair[1], &inner);
                                        // an element of a part of p is a strict part of p
                                        let r = self.rel(&pair[1], &inner).map(|r| Rel { param: r.param, strict: true });
                                        self.bind(pat, r, &mut inner);
                                    }
                                }
                            }
                        }
                        items[2..].iter().for_each(|b| self.walk(b, &inner));
                    }
                    "fn" => {
                        let mut inner = sc.clone();
                        if let Some(p) = items.get(1) {
                            self.bind(p, None, &mut inner);
                        }
                        items[2..].iter().for_each(|b| self.walk(b, &inner));
                    }
                    _ if OVER_ELEMENTS.contains(&head) && items.len() == 3 => {
                        let coll = &items[2];
                        let elem = self.rel(coll, sc).map(|r| Rel { param: r.param, strict: true });
                        match &items[1] {
                            // (map f xs): f is applied to each element
                            Value::Symbol(f) if &**f == self.name && !sc.shadowed => {
                                let smaller = match elem {
                                    Some(r) if r.param == 0 => vec![0],
                                    _ => vec![],
                                };
                                self.calls.push((smaller, *pos));
                            }
                            // (map (fn [x] ...) xs): x is an element
                            Value::List(f, _) if f.first().and_then(|h| h.sym_name()) == Some("fn") && f.len() >= 2 => {
                                let mut inner = sc.clone();
                                if let Value::Vector(ps) = &f[1] {
                                    if let Some(p0) = ps.first() {
                                        self.bind(p0, elem, &mut inner);
                                    }
                                    ps.iter().skip(1).for_each(|p| self.bind(p, None, &mut inner));
                                }
                                f[2..].iter().for_each(|b| self.walk(b, &inner));
                            }
                            other => self.walk(other, sc),
                        }
                        self.walk(coll, sc);
                    }
                    _ => items.iter().for_each(|i| self.walk(i, sc)),
                }
            }
            _ => {}
        }
    }
}

/// Analyse a function body (already macro-expanded). Returns the parameter
/// that must shrink, or None if the function does not call itself.
pub fn decreasing_param(name: &str, params: &Value, body: &[Value]) -> Res<Option<usize>> {
    let pats = match params {
        Value::Vector(p) => p.clone(),
        _ => return Ok(None),
    };
    let arity = pats.iter().position(|p| p.sym_name() == Some("&")).unwrap_or(pats.len());
    let mut w = Walk { name, arity, calls: vec![], error: None };
    let mut sc = Scope::default();
    for (i, p) in pats.iter().take(arity).enumerate() {
        w.bind(p, Some(Rel { param: i, strict: false }), &mut sc);
    }
    if let Some(rest) = pats.get(arity + 1) {
        w.bind(rest, None, &mut sc);
    }
    for f in body {
        w.walk(f, &sc);
    }
    if let Some(e) = w.error {
        return Err(e);
    }
    if w.calls.is_empty() {
        return Ok(None);
    }
    let mut common: Vec<usize> = (0..arity).collect();
    for (smaller, _) in &w.calls {
        common.retain(|i| smaller.contains(i));
    }
    match common.first() {
        Some(i) => Ok(Some(*i)),
        None => {
            let (smaller, pos) = w.calls.iter().find(|(s, _)| s.is_empty()).unwrap_or(&w.calls[0]);
            let msg = if smaller.is_empty() {
                format!("this call to `{}` does not pass a smaller part of any parameter", name)
            } else {
                format!("the recursive calls to `{}` do not all make the same parameter smaller", name)
            };
            Err(KilnError::new("K0401", msg)
                .help(
                    "Kiln allows only structural recursion, so every build terminates: recur on (rest xs), \
                     a destructured part, or an element of the same parameter each time",
                )
                .at(*pos))
        }
    }
}
