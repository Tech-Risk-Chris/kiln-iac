//! Gradual shape inference (M3 step 3).
//!
//! Each top-level form is macro-expanded and its shape inferred before it is
//! evaluated. Shapes are gradual: `?` is consistent with every type, and where
//! inference cannot be precise it yields `?` instead of failing. A definite
//! clash (a string where a number is required, a call with the wrong number
//! of arguments) is recorded. In this step nothing is reported to users; the
//! clashes are collected so that the checker can be shown to accept all
//! existing code before it is allowed to reject any.

use crate::eval::Interp;
use crate::value::*;
use serde_json::Value as J;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

pub type Fields = BTreeMap<String, (Ty, bool)>; // name -> (type, optional)

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tail {
    Closed,
    Open(u32),
}

#[derive(Clone, Debug)]
pub enum Ty {
    Var(u32),
    Dyn,
    Nil,
    Bool,
    Num,
    Str,
    Kw,
    Vec(Box<Ty>),
    Map(Box<Ty>),
    Rec(Fields, Tail),
    Fn(Vec<Ty>, Option<Box<Ty>>, Box<Ty>),
    /// A resource or data source handle of a Terraform type.
    Res(String, bool),
    Module,
}

pub struct Scheme {
    vars: Vec<u32>,
    rows: Vec<u32>,
    ty: Ty,
}

#[derive(Clone, Default)]
struct Subst {
    tys: Vec<Option<Ty>>,
    rows: Vec<Option<(Fields, Tail)>>,
}

pub struct Clash {
    pub msg: String,
    pub pos: Option<Pos>,
}

pub struct Infer {
    s: Subst,
    globals: HashMap<String, Scheme>,
    locals: Vec<(String, Ty)>,
    /// Schema blocks of the resource types seen so far: (type, data) -> block.
    blocks: HashMap<(String, bool), Option<Rc<J>>>,
    /// Constructor symbols resolved to Terraform types, e.g. `g/compute_network`.
    ctors: HashMap<String, String>,
    pos: Vec<Pos>,
    pub clashes: Vec<Clash>,
    /// Inferred types of top-level definitions, in order.
    pub defs: Vec<(String, Ty)>,
}

// ---------------------------------------------------------------- builtins

const NUM_OPS: &[&str] = &["+", "-", "*", "/", "mod", "inc", "dec", "min", "max"];
const BOOL_OPS: &[&str] = &["=", "not=", "<", ">", "<=", ">=", "not", "empty?", "nil?", "some?", "contains?"];
const STR_OPS: &[&str] = &["str", "name", "slug", "lower", "upper", "join"];

fn is_builtin(name: &str) -> bool {
    crate::builtins::NAMES.contains(&name)
}

impl Infer {
    pub fn new() -> Infer {
        Infer {
            s: Subst::default(),
            globals: HashMap::new(),
            locals: vec![],
            blocks: HashMap::new(),
            ctors: HashMap::new(),
            pos: vec![],
            clashes: vec![],
            defs: vec![],
        }
    }

    // ------------------------------------------------------------ substitution

    fn fresh(&mut self) -> Ty {
        self.s.tys.push(None);
        Ty::Var(self.s.tys.len() as u32 - 1)
    }

    fn fresh_row(&mut self) -> Tail {
        self.s.rows.push(None);
        Tail::Open(self.s.rows.len() as u32 - 1)
    }

    fn resolve(&self, t: &Ty) -> Ty {
        match t {
            Ty::Var(v) => match &self.s.tys[*v as usize] {
                Some(b) => self.resolve(b),
                None => t.clone(),
            },
            Ty::Rec(f, tail) => {
                let (f, tail) = self.row(f.clone(), *tail);
                Ty::Rec(f, tail)
            }
            _ => t.clone(),
        }
    }

    /// A record's fields with its tail followed as far as it is bound.
    fn row(&self, mut f: Fields, mut tail: Tail) -> (Fields, Tail) {
        let mut guard = 0;
        while let Tail::Open(r) = tail {
            match &self.s.rows[r as usize] {
                Some((more, next)) if guard < 10_000 => {
                    for (k, v) in more {
                        f.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                    tail = *next;
                    guard += 1;
                }
                _ => break,
            }
        }
        (f, tail)
    }

    fn occurs(&self, v: u32, t: &Ty) -> bool {
        match self.resolve(t) {
            Ty::Var(w) => v == w,
            Ty::Vec(e) | Ty::Map(e) => self.occurs(v, &e),
            Ty::Rec(f, _) => f.values().any(|(t, _)| self.occurs(v, t)),
            Ty::Fn(ps, r, ret) => {
                ps.iter().any(|p| self.occurs(v, p)) || r.map_or(false, |r| self.occurs(v, &r)) || self.occurs(v, &ret)
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ unification

    fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), String> {
        let (a, b) = (self.resolve(a), self.resolve(b));
        match (&a, &b) {
            (Ty::Dyn, _) | (_, Ty::Dyn) | (Ty::Nil, _) | (_, Ty::Nil) => Ok(()),
            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(x), t) | (t, Ty::Var(x)) => {
                if !self.occurs(*x, t) {
                    self.s.tys[*x as usize] = Some(t.clone());
                }
                Ok(())
            }
            (Ty::Bool, Ty::Bool) | (Ty::Num, Ty::Num) | (Ty::Str, Ty::Str) | (Ty::Kw, Ty::Kw) => Ok(()),
            // Keywords become strings in Terraform.
            (Ty::Str, Ty::Kw) | (Ty::Kw, Ty::Str) => Ok(()),
            (Ty::Module, Ty::Module) => Ok(()),
            (Ty::Vec(x), Ty::Vec(y)) | (Ty::Map(x), Ty::Map(y)) => self.unify(x, y),
            (Ty::Rec(f, _), Ty::Map(v)) | (Ty::Map(v), Ty::Rec(f, _)) => {
                for (t, _) in f.values() {
                    self.unify(t, v)?;
                }
                Ok(())
            }
            (Ty::Rec(f1, t1), Ty::Rec(f2, t2)) => self.unify_rows(f1.clone(), *t1, f2.clone(), *t2),
            (Ty::Res(t, d), Ty::Rec(f, _)) | (Ty::Rec(f, _), Ty::Res(t, d)) => {
                let (t, d, f) = (t.clone(), *d, f.clone());
                for (k, (ft, _)) in f {
                    if let Some(at) = self.res_attr(&t, d, &k) {
                        self.unify(&ft, &at)?;
                    }
                }
                Ok(())
            }
            (Ty::Res(x, dx), Ty::Res(y, dy)) if x == y && dx == dy => Ok(()),
            (Ty::Module, Ty::Rec(..)) | (Ty::Rec(..), Ty::Module) => Ok(()),
            (Ty::Fn(p1, r1, ret1), Ty::Fn(p2, r2, ret2)) => {
                let fixed = p1.len().min(p2.len());
                if (p1.len() != p2.len()) && (r1.is_none() && p1.len() < p2.len() || r2.is_none() && p2.len() < p1.len()) {
                    return Err(format!("functions of {} and {} arguments", p1.len(), p2.len()));
                }
                for i in 0..fixed {
                    self.unify(&p1[i], &p2[i])?;
                }
                self.unify(ret1, ret2)
            }
            _ => Err(format!("{} where {} is expected", self.show(&a), self.show(&b))),
        }
    }

    fn unify_rows(&mut self, f1: Fields, t1: Tail, f2: Fields, t2: Tail) -> Result<(), String> {
        for (k, (a, _)) in &f1 {
            if let Some((b, _)) = f2.get(k) {
                self.unify(a, b)?;
            }
        }
        let only1: Fields = f1.iter().filter(|(k, _)| !f2.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let only2: Fields = f2.iter().filter(|(k, _)| !f1.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        if t1 == t2 && t1 != Tail::Closed && (!only1.is_empty() || !only2.is_empty()) {
            return Ok(()); // a cycle; give up quietly
        }
        let need = |only: &Fields, other: Tail| -> Result<(), String> {
            match (other, only.iter().find(|(_, (_, opt))| !opt)) {
                (Tail::Closed, Some((k, _))) => Err(format!("a record without the field :{}", k)),
                _ => Ok(()),
            }
        };
        need(&only1, t2)?;
        need(&only2, t1)?;
        match (t1, t2) {
            (Tail::Open(r1), Tail::Open(r2)) => {
                let rest = self.fresh_row();
                self.s.rows[r1 as usize] = Some((only2, rest));
                self.s.rows[r2 as usize] = Some((only1, rest));
            }
            (Tail::Open(r1), Tail::Closed) => self.s.rows[r1 as usize] = Some((only2, Tail::Closed)),
            (Tail::Closed, Tail::Open(r2)) => self.s.rows[r2 as usize] = Some((only1, Tail::Closed)),
            (Tail::Closed, Tail::Closed) => {}
        }
        Ok(())
    }

    /// Unify, recording a clash on failure. The program goes on with `?`.
    fn expect(&mut self, found: &Ty, expected: &Ty, what: &str) {
        let snapshot = self.s.clone();
        if let Err(e) = self.unify(found, expected) {
            self.s = snapshot;
            self.clash(format!("{}: {}", what, e));
        }
    }

    fn clash(&mut self, msg: String) {
        let pos = self.pos.last().copied();
        self.clashes.push(Clash { msg, pos });
    }

    /// The least common shape of two types: unification if it succeeds,
    /// otherwise records with the differing fields made optional, otherwise `?`.
    fn join(&mut self, a: &Ty, b: &Ty) -> Ty {
        let snapshot = self.s.clone();
        if self.unify(a, b).is_ok() {
            return match self.resolve(a) {
                Ty::Nil => self.resolve(b),
                t => t,
            };
        }
        self.s = snapshot;
        match (self.resolve(a), self.resolve(b)) {
            (Ty::Rec(f1, _), Ty::Rec(f2, _)) => {
                let mut out = Fields::new();
                for (k, (t, opt)) in &f1 {
                    match f2.get(k) {
                        Some((u, opt2)) => {
                            let j = self.join(t, u);
                            out.insert(k.clone(), (j, *opt || *opt2));
                        }
                        None => {
                            out.insert(k.clone(), (t.clone(), true));
                        }
                    }
                }
                for (k, (t, _)) in &f2 {
                    if !f1.contains_key(k) {
                        out.insert(k.clone(), (t.clone(), true));
                    }
                }
                Ty::Rec(out, Tail::Closed)
            }
            (Ty::Vec(x), Ty::Vec(y)) => Ty::Vec(Box::new(self.join(&x, &y))),
            (Ty::Map(x), Ty::Map(y)) => Ty::Map(Box::new(self.join(&x, &y))),
            _ => Ty::Dyn,
        }
    }

    // ------------------------------------------------------------ schemas

    fn shape_of_type(t: &J) -> Ty {
        match t {
            J::String(s) => match s.as_str() {
                "string" => Ty::Str,
                "number" => Ty::Num,
                "bool" => Ty::Bool,
                _ => Ty::Dyn,
            },
            J::Array(a) => match (a.first().and_then(|k| k.as_str()), a.get(1)) {
                (Some("list" | "set"), Some(e)) => Ty::Vec(Box::new(Self::shape_of_type(e))),
                (Some("map"), Some(e)) => Ty::Map(Box::new(Self::shape_of_type(e))),
                (Some("object"), Some(J::Object(attrs))) => Ty::Rec(
                    attrs.iter().map(|(k, v)| (k.clone(), (Self::shape_of_type(v), false))).collect(),
                    Tail::Closed,
                ),
                _ => Ty::Vec(Box::new(Ty::Dyn)),
            },
            _ => Ty::Dyn,
        }
    }

    fn shape_of_block(b: &J) -> Ty {
        let mut f = Fields::new();
        if let Some(attrs) = b["attributes"].as_object() {
            for (k, a) in attrs {
                f.insert(k.clone(), (Self::shape_of_attr(a), false));
            }
        }
        if let Some(bts) = b["block_types"].as_object() {
            for (k, bt) in bts {
                f.insert(k.clone(), (Self::shape_of_block_type(bt), false));
            }
        }
        Ty::Rec(f, Tail::Closed)
    }

    fn shape_of_attr(a: &J) -> Ty {
        match a.get("nested_type") {
            Some(nt) => {
                let inner = Self::shape_of_block(nt);
                match nt["nesting_mode"].as_str().unwrap_or("single") {
                    "single" => inner,
                    "map" => Ty::Map(Box::new(inner)),
                    _ => Ty::Vec(Box::new(inner)),
                }
            }
            None => Self::shape_of_type(&a["type"]),
        }
    }

    fn shape_of_block_type(bt: &J) -> Ty {
        let inner = Self::shape_of_block(&bt["block"]);
        match bt["nesting_mode"].as_str().unwrap_or("list") {
            "single" | "group" => inner,
            "map" => Ty::Map(Box::new(inner)),
            _ => Ty::Vec(Box::new(inner)),
        }
    }

    fn res_attr(&self, ty: &str, data: bool, k: &str) -> Option<Ty> {
        let block = self.blocks.get(&(ty.to_string(), data))?.as_ref()?;
        if let Some(a) = block["attributes"].get(k) {
            return Some(Self::shape_of_attr(a));
        }
        if let Some(bt) = block["block_types"].get(k) {
            return Some(Self::shape_of_block_type(bt));
        }
        None
    }

    // ------------------------------------------------------------ access

    fn field(&mut self, t: &Ty, k: &str) -> Ty {
        match self.resolve(t) {
            Ty::Rec(f, tail) => match f.get(k) {
                Some((ft, _)) => ft.clone(),
                None => match tail {
                    Tail::Open(r) => {
                        let v = self.fresh();
                        let rest = self.fresh_row();
                        let mut more = Fields::new();
                        more.insert(k.to_string(), (v.clone(), true));
                        self.s.rows[r as usize] = Some((more, rest));
                        v
                    }
                    // (:missing m) is nil, which Kiln code relies on.
                    Tail::Closed => Ty::Nil,
                },
            },
            Ty::Map(v) => *v,
            Ty::Res(ty, d) => self.res_attr(&ty, d, k).unwrap_or(Ty::Dyn),
            Ty::Var(x) => {
                let v = self.fresh();
                let rest = self.fresh_row();
                let mut f = Fields::new();
                f.insert(k.to_string(), (v.clone(), true));
                self.s.tys[x as usize] = Some(Ty::Rec(f, rest));
                v
            }
            Ty::Vec(_) | Ty::Nil => Ty::Nil,
            Ty::Dyn | Ty::Module => Ty::Dyn,
            other => {
                let shown = self.show(&other);
                self.clash(format!("cannot look up :{} in {}", k, shown));
                Ty::Dyn
            }
        }
    }

    /// The type of the elements of something iterated.
    fn element(&mut self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Vec(e) => *e,
            Ty::Map(_) | Ty::Rec(..) | Ty::Nil | Ty::Var(_) | Ty::Dyn | Ty::Module => Ty::Dyn,
            other => {
                let shown = self.show(&other);
                self.clash(format!("{} is not a collection", shown));
                Ty::Dyn
            }
        }
    }

    // ------------------------------------------------------------ environment

    fn lookup(&mut self, name: &str) -> Option<Ty> {
        if let Some((_, t)) = self.locals.iter().rev().find(|(n, _)| n == name) {
            return Some(t.clone());
        }
        let (vars, rows, ty) = match self.globals.get(name) {
            Some(sc) => (sc.vars.clone(), sc.rows.clone(), sc.ty.clone()),
            None => return None,
        };
        let mut vm = HashMap::new();
        for v in vars {
            let f = self.fresh();
            vm.insert(v, f);
        }
        let mut rm = HashMap::new();
        for r in rows {
            let f = self.fresh_row();
            rm.insert(r, f);
        }
        Some(self.instantiate(&ty, &vm, &rm))
    }

    fn instantiate(&self, t: &Ty, vm: &HashMap<u32, Ty>, rm: &HashMap<u32, Tail>) -> Ty {
        match t {
            Ty::Var(v) => vm.get(v).cloned().unwrap_or_else(|| t.clone()),
            Ty::Vec(e) => Ty::Vec(Box::new(self.instantiate(e, vm, rm))),
            Ty::Map(e) => Ty::Map(Box::new(self.instantiate(e, vm, rm))),
            Ty::Rec(f, tail) => Ty::Rec(
                f.iter().map(|(k, (t, o))| (k.clone(), (self.instantiate(t, vm, rm), *o))).collect(),
                match tail {
                    Tail::Open(r) => rm.get(r).copied().unwrap_or(*tail),
                    Tail::Closed => Tail::Closed,
                },
            ),
            Ty::Fn(ps, r, ret) => Ty::Fn(
                ps.iter().map(|p| self.instantiate(p, vm, rm)).collect(),
                r.as_ref().map(|r| Box::new(self.instantiate(r, vm, rm))),
                Box::new(self.instantiate(ret, vm, rm)),
            ),
            other => other.clone(),
        }
    }

    /// Fully resolve a type, so that it no longer refers to bound variables.
    pub fn zonk(&self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Vec(e) => Ty::Vec(Box::new(self.zonk(&e))),
            Ty::Map(e) => Ty::Map(Box::new(self.zonk(&e))),
            Ty::Rec(f, tail) => Ty::Rec(f.iter().map(|(k, (t, o))| (k.clone(), (self.zonk(t), *o))).collect(), tail),
            Ty::Fn(ps, r, ret) => Ty::Fn(
                ps.iter().map(|p| self.zonk(p)).collect(),
                r.map(|r| Box::new(self.zonk(&r))),
                Box::new(self.zonk(&ret)),
            ),
            t => t,
        }
    }

    fn free(&self, t: &Ty, vars: &mut Vec<u32>, rows: &mut Vec<u32>) {
        match t {
            Ty::Var(v) => {
                if !vars.contains(v) {
                    vars.push(*v)
                }
            }
            Ty::Vec(e) | Ty::Map(e) => self.free(e, vars, rows),
            Ty::Rec(f, tail) => {
                f.values().for_each(|(t, _)| self.free(t, vars, rows));
                if let Tail::Open(r) = tail {
                    if !rows.contains(r) {
                        rows.push(*r)
                    }
                }
            }
            Ty::Fn(ps, r, ret) => {
                ps.iter().for_each(|p| self.free(p, vars, rows));
                if let Some(r) = r {
                    self.free(r, vars, rows);
                }
                self.free(ret, vars, rows);
            }
            _ => {}
        }
    }

    fn define(&mut self, name: &str, t: &Ty) {
        let ty = self.zonk(t);
        let (mut vars, mut rows) = (vec![], vec![]);
        self.free(&ty, &mut vars, &mut rows);
        self.defs.push((name.to_string(), ty.clone()));
        self.globals.insert(name.to_string(), Scheme { vars, rows, ty });
    }

    // ------------------------------------------------------------ patterns

    fn bind(&mut self, pat: &Value, t: Ty) {
        match pat {
            Value::Symbol(s) if &**s != "&" => self.locals.push((s.to_string(), t)),
            Value::Vector(ps) => {
                let elem = match self.resolve(&t) {
                    Ty::Vec(e) => *e,
                    _ => Ty::Dyn,
                };
                let mut i = 0;
                while i < ps.len() {
                    match &ps[i] {
                        Value::Symbol(s) if &**s == "&" => {
                            if let Some(p) = ps.get(i + 1) {
                                self.bind(p, Ty::Vec(Box::new(elem.clone())));
                            }
                            i += 2;
                        }
                        Value::Keyword(k) if &**k == "as" => {
                            if let Some(p) = ps.get(i + 1) {
                                self.bind(p, t.clone());
                            }
                            i += 2;
                        }
                        p => {
                            self.bind(p, elem.clone());
                            i += 1;
                        }
                    }
                }
            }
            Value::Map(entries) => {
                let defaults: Vec<(Value, Value)> = entries
                    .iter()
                    .find(|(k, _)| matches!(k, Value::Keyword(x) if &**x == "or"))
                    .and_then(|(_, v)| if let Value::Map(d) = v { Some(d.to_vec()) } else { None })
                    .unwrap_or_default();
                for (k, v) in entries.iter() {
                    match k {
                        Value::Keyword(kw) if &**kw == "keys" => {
                            if let Value::Vector(syms) = v {
                                for sym in syms.iter() {
                                    if let Value::Symbol(n) = sym {
                                        let mut ft = self.field(&t, n);
                                        if let Some((_, d)) = defaults.iter().find(|(dn, _)| dn.sym_name() == Some(n)) {
                                            let dt = self.infer(d);
                                            ft = self.join(&ft, &dt);
                                        }
                                        self.locals.push((n.to_string(), ft));
                                    }
                                }
                            }
                        }
                        Value::Keyword(kw) if &**kw == "as" => self.bind(v, t.clone()),
                        Value::Keyword(kw) if &**kw == "or" => {}
                        p => {
                            let ft = match v {
                                Value::Keyword(key) => self.field(&t, key),
                                _ => Ty::Dyn,
                            };
                            self.bind(p, ft);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ expressions

    pub fn infer(&mut self, form: &Value) -> Ty {
        match form {
            Value::Nil => Ty::Nil,
            Value::Bool(_) => Ty::Bool,
            Value::Int(_) | Value::Float(_) => Ty::Num,
            Value::Str(_) | Value::Named(_) => Ty::Str,
            Value::Keyword(_) => Ty::Kw,
            Value::Symbol(s) => self.lookup(s).unwrap_or(Ty::Dyn),
            Value::Vector(items) => {
                let mut elem = Ty::Nil;
                for it in items.iter() {
                    let t = self.infer(it);
                    elem = self.join(&elem, &t);
                }
                Ty::Vec(Box::new(match elem {
                    Ty::Nil if !items.is_empty() => Ty::Nil,
                    Ty::Nil => self.fresh(),
                    e => e,
                }))
            }
            Value::Map(entries) => {
                let literal = entries.iter().all(|(k, _)| matches!(k, Value::Keyword(_) | Value::Str(_)));
                if literal {
                    let mut f = Fields::new();
                    for (k, v) in entries.iter() {
                        let name = match k {
                            Value::Keyword(s) | Value::Str(s) => s.to_string(),
                            _ => unreachable!(),
                        };
                        let t = self.infer(v);
                        // nil values are dropped from maps, so such a field is optional
                        let opt = matches!(self.resolve(&t), Ty::Nil);
                        f.insert(name, (t, opt));
                    }
                    Ty::Rec(f, Tail::Closed)
                } else {
                    let mut vt = Ty::Nil;
                    for (k, v) in entries.iter() {
                        self.infer(k);
                        let t = self.infer(v);
                        vt = self.join(&vt, &t);
                    }
                    Ty::Map(Box::new(vt))
                }
            }
            Value::List(items, pos) if !items.is_empty() => {
                if let Some(p) = pos {
                    self.pos.push(*p);
                }
                let t = self.infer_list(items);
                if pos.is_some() {
                    self.pos.pop();
                }
                t
            }
            _ => Ty::Dyn,
        }
    }

    fn body(&mut self, forms: &[Value]) -> Ty {
        let mut last = Ty::Nil;
        for f in forms {
            last = self.infer(f);
        }
        last
    }

    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.locals.len();
        let r = f(self);
        self.locals.truncate(mark);
        r
    }

    fn shadowed(&self, name: &str) -> bool {
        self.locals.iter().any(|(n, _)| n == name) || self.globals.contains_key(name)
    }

    fn infer_list(&mut self, items: &[Value]) -> Ty {
        let head = items[0].sym_name().unwrap_or("");
        let special = !self.shadowed(head);
        if special {
            match head {
                "quote" | "quasiquote" => return Ty::Dyn,
                "if" => {
                    if let Some(c) = items.get(1) {
                        self.infer(c);
                    }
                    let a = items.get(2).map(|f| self.infer(f)).unwrap_or(Ty::Nil);
                    let b = items.get(3).map(|f| self.infer(f)).unwrap_or(Ty::Nil);
                    return self.join(&a, &b);
                }
                "do" => return self.body(&items[1..]),
                "and" | "or" => {
                    let mut t = Ty::Nil;
                    for f in &items[1..] {
                        let u = self.infer(f);
                        t = self.join(&t, &u);
                    }
                    return t;
                }
                "let" => {
                    return self.scoped(|me| {
                        if let Some(Value::Vector(binds)) = items.get(1) {
                            for pair in binds.chunks(2) {
                                if pair.len() == 2 {
                                    let t = me.infer(&pair[1]);
                                    me.bind(&pair[0], t);
                                }
                            }
                        }
                        me.body(&items[2..])
                    })
                }
                "fn" => return self.infer_fn(None, items.get(1), &items[2.min(items.len())..]),
                "for" => {
                    return self.scoped(|me| {
                        if let Some(Value::Vector(binds)) = items.get(1) {
                            for pair in binds.chunks(2) {
                                if pair.len() != 2 {
                                    continue;
                                }
                                match &pair[0] {
                                    Value::Keyword(k) if &**k == "when" => {
                                        me.infer(&pair[1]);
                                    }
                                    Value::Keyword(k) if &**k == "let" => {
                                        if let Value::Vector(lb) = &pair[1] {
                                            for p in lb.chunks(2) {
                                                if p.len() == 2 {
                                                    let t = me.infer(&p[1]);
                                                    me.bind(&p[0], t);
                                                }
                                            }
                                        }
                                    }
                                    pat => {
                                        let ct = me.infer(&pair[1]);
                                        let e = me.element(&ct);
                                        me.bind(pat, e);
                                    }
                                }
                            }
                        }
                        let b = me.body(&items[2..]);
                        Ty::Vec(Box::new(b))
                    })
                }
                "scope" => {
                    items.get(1).map(|n| self.infer(n));
                    return self.body(&items[2.min(items.len())..]);
                }
                "data" => {
                    let ty = items.get(1).and_then(|c| c.sym_name()).and_then(|c| self.ctor_name(c));
                    for a in &items[2.min(items.len())..] {
                        self.infer(a);
                    }
                    return match ty {
                        Some(t) => Ty::Res(t, true),
                        None => Ty::Dyn,
                    };
                }
                "module" => {
                    for a in &items[1..] {
                        self.infer(a);
                    }
                    return Ty::Module;
                }
                "provider" | "output" | "backend" => {
                    for a in &items[2.min(items.len())..] {
                        self.infer(a);
                    }
                    return Ty::Nil;
                }
                "def" | "defn" | "defmacro" | "ns" => return Ty::Nil,
                _ => {}
            }
        }
        // namespaced constructors and Terraform functions
        if let Some(sym) = items[0].sym_name() {
            if sym.contains('/') && !self.shadowed(sym) {
                for a in &items[1..] {
                    self.infer(a);
                }
                if sym.starts_with("tf/") {
                    return Ty::Dyn;
                }
                return match self.ctor_name(sym) {
                    Some(t) => Ty::Res(t, false),
                    None => Ty::Dyn,
                };
            }
            if is_builtin(sym) && !self.shadowed(sym) {
                return self.builtin(sym, &items[1..]);
            }
        }
        if let Value::Keyword(k) = &items[0] {
            let m = items.get(1).map(|f| self.infer(f)).unwrap_or(Ty::Dyn);
            let v = self.field(&m, k);
            return match items.get(2) {
                Some(d) => {
                    let dt = self.infer(d);
                    self.join(&v, &dt)
                }
                None => v,
            };
        }
        let f = self.infer(&items[0]);
        let args: Vec<Ty> = items[1..].iter().map(|a| self.infer(a)).collect();
        self.apply(&f, &args, &items[0])
    }

    fn apply(&mut self, f: &Ty, args: &[Ty], head: &Value) -> Ty {
        let name = match head {
            Value::Symbol(s) => format!("`{}`", s),
            _ => "this function".to_string(),
        };
        match self.resolve(f) {
            Ty::Fn(ps, rest, ret) => {
                if args.len() < ps.len() || (rest.is_none() && args.len() > ps.len()) {
                    self.clash(format!(
                        "{} takes {}{} argument{}, got {}",
                        name,
                        if rest.is_some() { "at least " } else { "" },
                        ps.len(),
                        if ps.len() == 1 { "" } else { "s" },
                        args.len()
                    ));
                    return *ret;
                }
                for (i, (a, p)) in args.iter().zip(ps.iter()).enumerate() {
                    self.expect(a, p, &format!("argument {} of {}", i + 1, name));
                }
                *ret
            }
            Ty::Var(x) => {
                let ret = self.fresh();
                self.s.tys[x as usize] = Some(Ty::Fn(args.to_vec(), None, Box::new(ret.clone())));
                ret
            }
            Ty::Map(v) => *v,
            Ty::Rec(..) | Ty::Dyn | Ty::Nil | Ty::Module => Ty::Dyn,
            other => {
                let shown = self.show(&other);
                self.clash(format!("{} is not a function", shown));
                Ty::Dyn
            }
        }
    }

    fn infer_fn(&mut self, name: Option<&str>, params: Option<&Value>, body: &[Value]) -> Ty {
        let pats = match params {
            Some(Value::Vector(p)) => p.clone(),
            _ => return Ty::Dyn,
        };
        let fixed_n = pats.iter().position(|p| p.sym_name() == Some("&")).unwrap_or(pats.len());
        let ps: Vec<Ty> = (0..fixed_n).map(|_| self.fresh()).collect();
        let rest = if fixed_n < pats.len() { Some(Box::new(Ty::Dyn)) } else { None };
        let ret = self.fresh();
        let me_ty = Ty::Fn(ps.clone(), rest.clone(), Box::new(ret.clone()));
        let body_t = self.scoped(|me| {
            if let Some(n) = name {
                me.locals.push((n.to_string(), me_ty.clone()));
            }
            for (p, t) in pats.iter().take(fixed_n).zip(ps.iter()) {
                me.bind(p, t.clone());
            }
            if let Some(rp) = pats.get(fixed_n + 1) {
                let rt = match rp {
                    Value::Symbol(_) => {
                        let e = me.fresh();
                        Ty::Vec(Box::new(e))
                    }
                    _ => me.fresh(), // keyword arguments
                };
                me.bind(rp, rt);
            }
            me.body(body)
        });
        self.expect(&body_t, &ret, "the result");
        me_ty
    }

    fn builtin(&mut self, name: &str, args: &[Value]) -> Ty {
        let ts: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
        let arg = |i: usize| ts.get(i).cloned().unwrap_or(Ty::Dyn);
        if NUM_OPS.contains(&name) {
            for (i, t) in ts.iter().enumerate() {
                self.expect(t, &Ty::Num, &format!("argument {} of `{}`", i + 1, name));
            }
            return Ty::Num;
        }
        if BOOL_OPS.contains(&name) {
            return Ty::Bool;
        }
        if STR_OPS.contains(&name) {
            return Ty::Str;
        }
        match name {
            "count" => {
                if let Some(t) = ts.first() {
                    if !matches!(self.resolve(t), Ty::Str) {
                        self.element(t);
                    }
                }
                Ty::Num
            }
            "first" | "second" | "last" | "nth" => self.element(&arg(0)),
            "rest" | "vec" => {
                let e = self.element(&arg(0));
                Ty::Vec(Box::new(e))
            }
            "conj" => {
                let mut e = self.element(&arg(0));
                for t in ts.iter().skip(1) {
                    e = self.join(&e, t);
                }
                Ty::Vec(Box::new(e))
            }
            "concat" | "flatten" => {
                let mut e = Ty::Nil;
                for t in &ts {
                    let x = if name == "flatten" { Ty::Dyn } else { self.element(t) };
                    e = self.join(&e, &x);
                }
                Ty::Vec(Box::new(e))
            }
            "list" => {
                let mut e = Ty::Nil;
                for t in &ts {
                    e = self.join(&e, t);
                }
                Ty::Vec(Box::new(e))
            }
            "range" => {
                for (i, t) in ts.iter().enumerate() {
                    self.expect(t, &Ty::Num, &format!("argument {} of `range`", i + 1));
                }
                Ty::Vec(Box::new(Ty::Num))
            }
            "get" => {
                let t = match (self.resolve(&arg(0)), args.get(1)) {
                    (_, Some(Value::Keyword(k))) | (_, Some(Value::Str(k))) => self.field(&arg(0), k),
                    (Ty::Vec(e), _) => *e,
                    (Ty::Map(v), _) => *v,
                    _ => Ty::Dyn,
                };
                match ts.get(2) {
                    Some(d) => self.join(&t, d),
                    None => t,
                }
            }
            "merge" => {
                let mut f = Fields::new();
                for t in &ts {
                    match self.resolve(t) {
                        Ty::Rec(g, Tail::Closed) => f.extend(g),
                        Ty::Nil => {}
                        _ => return Ty::Dyn,
                    }
                }
                Ty::Rec(f, Tail::Closed)
            }
            "assoc" => match self.resolve(&arg(0)) {
                Ty::Rec(mut f, Tail::Closed) => {
                    for pair in args[1.min(args.len())..].chunks(2).zip(ts[1.min(ts.len())..].chunks(2)) {
                        match (pair.0.first(), pair.1.get(1)) {
                            (Some(Value::Keyword(k)), Some(t)) => {
                                f.insert(k.to_string(), (t.clone(), false));
                            }
                            _ => return Ty::Dyn,
                        }
                    }
                    Ty::Rec(f, Tail::Closed)
                }
                _ => Ty::Dyn,
            },
            "map" | "mapcat" | "filter" if !args.is_empty() => {
                let mut elems = vec![];
                for t in ts.iter().skip(1) {
                    elems.push(self.element(t));
                }
                let r = self.apply(&arg(0), &elems, &args[0]);
                match name {
                    "map" => Ty::Vec(Box::new(r)),
                    "mapcat" => {
                        let e = self.element(&r);
                        Ty::Vec(Box::new(e))
                    }
                    _ => {
                        let e = elems.first().cloned().unwrap_or(Ty::Dyn);
                        Ty::Vec(Box::new(e))
                    }
                }
            }
            "reduce" => {
                if ts.len() == 3 {
                    let e = self.element(&ts[2]);
                    let r = self.apply(&ts[0], &[ts[1].clone(), e], &args[0]);
                    self.join(&ts[1], &r)
                } else {
                    Ty::Dyn
                }
            }
            "keys" => Ty::Vec(Box::new(Ty::Str)),
            "identity" | "with-meta" | "secret" => arg(0),
            "keyword" => Ty::Kw,
            _ => Ty::Dyn,
        }
    }

    fn ctor_name(&mut self, sym: &str) -> Option<String> {
        self.ctors.get(sym).cloned()
    }

    /// Resolve every namespaced symbol in a form while the interpreter is at hand.
    fn learn_ctors(&mut self, it: &Interp, f: &Value) {
        match f {
            Value::Symbol(s) if s.contains('/') && !s.starts_with("tf/") => {
                if !self.ctors.contains_key(&**s) {
                    if let Some(ty) = it.ctor_type(s) {
                        for data in [false, true] {
                            self.blocks.entry((ty.clone(), data)).or_insert_with(|| it.peek_schema(data, &ty));
                        }
                        self.ctors.insert(s.to_string(), ty);
                    }
                }
            }
            Value::List(items, _) | Value::Vector(items) => items.iter().for_each(|i| self.learn_ctors(it, i)),
            Value::Map(entries) => entries.iter().for_each(|(k, v)| {
                self.learn_ctors(it, k);
                self.learn_ctors(it, v)
            }),
            _ => {}
        }
    }

    // ------------------------------------------------------------ top level

    /// Infer one top-level form. Macros are expanded first, using the
    /// interpreter as it stands before the form is evaluated.
    pub fn toplevel(&mut self, it: &mut Interp, form: &Value) {
        let expanded = match it.expand_all(form, 0) {
            Ok(e) => e,
            Err(_) => return, // evaluation reports it
        };
        self.learn_ctors(it, &expanded);
        self.form(&expanded);
    }

    fn form(&mut self, f: &Value) {
        match f {
            Value::List(items, pos) if !items.is_empty() => {
                if let Some(p) = pos {
                    self.pos.push(*p);
                }
                self.top_items(items);
                if pos.is_some() {
                    self.pos.pop();
                }
            }
            _ => {
                self.infer(f);
            }
        }
    }

    fn top_items(&mut self, items: &[Value]) {
        match items[0].sym_name().unwrap_or("") {
            "ns" | "defmacro" => {}
            "do" => items[1..].iter().for_each(|f| self.form(f)),
            "def" => {
                if let (Some(Value::Symbol(n)), Some(v)) = (items.get(1), items.get(2)) {
                    let t = self.infer(v);
                    self.define(n, &t);
                }
            }
            "defn" => {
                if let Some(Value::Symbol(n)) = items.get(1) {
                    let mut i = 2;
                    if matches!(items.get(i), Some(Value::Str(_))) && items.len() > 3 {
                        i += 1;
                    }
                    let t = self.infer_fn(Some(n), items.get(i), &items[(i + 1).min(items.len())..]);
                    self.define(n, &t);
                }
            }
            _ => {
                self.infer_list(items);
            }
        }
    }

    // ------------------------------------------------------------ printing

    pub fn show(&self, t: &Ty) -> String {
        let mut names = HashMap::new();
        self.show_in(&self.zonk(t), &mut names)
    }

    fn show_in(&self, t: &Ty, names: &mut HashMap<String, String>) -> String {
        let mut name = |key: String, prefix: &str| -> String {
            let n = names.len();
            names
                .entry(key)
                .or_insert_with(|| {
                    if prefix.is_empty() {
                        ((b'a' + (n % 26) as u8) as char).to_string()
                    } else {
                        format!("{}{}", prefix, n)
                    }
                })
                .clone()
        };
        match t {
            Ty::Var(v) => name(format!("t{}", v), ""),
            Ty::Dyn => "?".into(),
            Ty::Nil => "Nil".into(),
            Ty::Bool => "Bool".into(),
            Ty::Num => "Num".into(),
            Ty::Str => "Str".into(),
            Ty::Kw => "Kw".into(),
            Ty::Module => "Module".into(),
            Ty::Res(ty, d) => format!("{}{}", if *d { "data " } else { "" }, ty),
            Ty::Vec(e) => format!("[{}]", self.show_in(e, names)),
            Ty::Map(e) => format!("(Map {})", self.show_in(e, names)),
            Ty::Rec(f, tail) => {
                let mut parts: Vec<String> = f
                    .iter()
                    .map(|(k, (t, o))| format!(":{}{} {}", k, if *o { "?" } else { "" }, self.show_in(t, names)))
                    .collect();
                if let Tail::Open(r) = tail {
                    let n = names.len();
                    let rn = names.entry(format!("r{}", r)).or_insert_with(|| format!("r{}", n)).clone();
                    parts.push(format!("& {}", rn));
                }
                format!("{{{}}}", parts.join(", "))
            }
            Ty::Fn(ps, rest, ret) => {
                let mut parts: Vec<String> = ps.iter().map(|p| self.show_in(p, names)).collect();
                if rest.is_some() {
                    parts.push("& ?".into());
                }
                format!("(Fn [{}] {})", parts.join(" "), self.show_in(ret, names))
            }
        }
    }
}

impl Default for Infer {
    fn default() -> Self {
        Infer::new()
    }
}
