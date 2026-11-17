//! Type inference (M3): gradual shapes, sound stages.
//!
//! Each top-level form is macro-expanded and inferred before it is evaluated.
//!
//! **Shapes** are gradual. `?` is consistent with every type, and where
//! inference cannot be precise it yields `?` instead of failing. Definite
//! shape clashes are collected but not yet reported.
//!
//! **Stages** are sound. Every shape carries a node of a constraint graph,
//! and so does every function's effect. Edges say "flows into". Sources are
//! values known only after apply (resource attributes, Terraform functions)
//! and declarations; sinks are the places that must be known at compile time
//! (conditions that decide whether resources exist, collections that are
//! iterated, names). A source that reaches a sink is an error, reported at
//! the sink with the source it came from, whether or not any build runs
//! that code. Top-level definitions are generalised together with the part
//! of the graph that concerns them, so a helper used once with a static and
//! once with a deferred value is judged at each call.

use crate::eval::Interp;
use crate::value::*;
use serde_json::Value as J;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;

pub type Fields = BTreeMap<String, (Ty, bool)>; // name -> (type, optional)

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tail {
    Closed,
    Open(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Prim {
    Bool,
    Num,
    Str,
    Kw,
}

/// A shape. The `u32` on most variants is its stage node in the graph; on
/// functions it is the effect node.
#[derive(Clone, Debug)]
pub enum Ty {
    Var(u32),
    Nil,
    Dyn(u32),
    P(Prim, u32),
    Vec(Box<Ty>, u32),
    Map(Box<Ty>, u32),
    Rec(Fields, Tail, u32),
    Fn(Vec<Ty>, Option<Box<Ty>>, Box<Ty>, u32),
    /// A resource or data source handle: type, data?, address if known,
    /// instancing (0 single, 1 count-deferred, 2 for-each-deferred).
    Res(String, bool, Option<Rc<str>>, u8),
    Module,
}

// ---------------------------------------------------------------- the graph

#[derive(Clone)]
enum Node {
    Var,
    /// A value known only after apply; `secret` if it is also sensitive.
    Src { desc: String, pos: Option<Pos>, secret: bool },
    /// Declaring a resource.
    Decl,
    /// Something that must be known at compile time, or (`secret`) must not
    /// receive a sensitive value.
    Sink { code: &'static str, msg: String, help: &'static str, pos: Option<Pos>, secret: bool },
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    out: Vec<Vec<u32>>,
}

impl Graph {
    fn add(&mut self, n: Node) -> u32 {
        self.nodes.push(n);
        self.out.push(vec![]);
        self.nodes.len() as u32 - 1
    }
    fn edge(&mut self, a: u32, b: u32) {
        if a != b && !self.out[a as usize].contains(&b) {
            self.out[a as usize].push(b);
        }
    }
    fn is_var(&self, n: u32) -> bool {
        matches!(self.nodes[n as usize], Node::Var)
    }
    /// For every node reachable from the starts: the start it was reached from.
    fn reach(&self, starts: &[u32]) -> HashMap<u32, u32> {
        let mut seen = HashMap::new();
        let mut q = VecDeque::new();
        for &s in starts {
            if seen.insert(s, s).is_none() {
                q.push_back(s);
            }
        }
        while let Some(n) = q.pop_front() {
            let origin = seen[&n];
            for &m in &self.out[n as usize] {
                if !seen.contains_key(&m) {
                    seen.insert(m, origin);
                    q.push_back(m);
                }
            }
        }
        seen
    }
}

pub struct Scheme {
    vars: Vec<u32>,
    rows: Vec<u32>,
    anns: Vec<u32>,
    edges: Vec<(u32, u32)>,
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

/// A stage error: a value known only after apply reaching a place that must
/// be known at compile time.
pub struct StageError {
    pub code: &'static str,
    pub msg: String,
    pub help: String,
    pub pos: Option<Pos>,
}

struct Pending {
    branch: u32,
    cond: u32,
    pos: Option<Pos>,
}

pub struct Infer {
    s: Subst,
    g: Graph,
    decl: u32,
    globals: HashMap<String, Scheme>,
    locals: Vec<(String, Ty)>,
    blocks: HashMap<(String, bool), Option<Rc<J>>>,
    ctors: HashMap<String, String>,
    /// Module sources resolved to their cached interfaces.
    modules: HashMap<String, Option<Rc<crate::schema::ModuleInterface>>>,
    pos: Vec<Pos>,
    effects: Vec<u32>,
    pending: Vec<Pending>,
    scope_depth: usize,
    /// Enclosing count-deferred (1) and for-each-deferred (2) forms.
    inst_frames: Vec<u8>,
    reported: HashSet<u32>,
    /// Type mismatches that count only if the value is known at compile time.
    typed: Vec<(u32, StageError)>,
    /// Sources already blamed: one error per cause, so that iterating a
    /// deferred list does not also report each name derived from it.
    blamed: HashSet<u32>,
    pub clashes: Vec<Clash>,
    pub errors: Vec<StageError>,
    pub defs: Vec<(String, Ty)>,
}

// ---------------------------------------------------------------- builtins

const NUM_OPS: &[&str] = &["+", "-", "*", "/", "mod", "inc", "dec", "min", "max"];
const LIFTED_BOOL: &[&str] = &["=", "not=", "<", ">", "<=", ">=", "not"];
const STATIC_BOOL: &[&str] = &["nil?", "some?"];
const COLLECTION_ARG: &[&str] = &[
    "count", "first", "second", "last", "nth", "rest", "vec", "conj", "empty?", "keys", "vals", "assoc", "dissoc",
    "select-keys", "update", "contains?", "get-in",
];
const COLLECTION_ARGS: &[&str] = &["concat", "merge", "merge-deep", "zipmap", "flatten"];
const TEXT_ARG: &[&str] = &["name", "keyword", "slug", "lower", "upper"];

const RESOURCE_META: &[&str] =
    &["count", "for_each", "depends_on", "provider", "lifecycle", "provisioner", "connection", "dynamic"];
const DATA_META: &[&str] = &["count", "for_each", "depends_on", "provider", "lifecycle", "dynamic"];

const K0312: (&str, &str, &str) = (
    "K0312",
    "resource existence depends on a value known only after apply",
    "branch on a compile-time value, or declare the resource unconditionally with an explicit :count meta-argument",
);
const K0313: (&str, &str, &str) = (
    "K0313",
    "cannot iterate over a collection known only after apply",
    "iterate over compile-time data, or pass a :for_each meta-argument explicitly",
);
const K0314: (&str, &str, &str) = (
    "K0314",
    "a name depends on a value known only after apply",
    "addresses are fixed at compile time; derive the name from compile-time data",
);
const K0315: (&str, &str, &str) = (
    "K0315",
    "and/or cannot short-circuit on a value known only after apply",
    "write the condition as a Terraform expression: (tf/and a b)",
);
const K0316: (&str, &str, &str) = (
    "K0316",
    "this function needs a compile-time string, not a value known only after apply",
    "use the Terraform function instead, e.g. (tf/lower x)",
);

fn is_builtin(name: &str) -> bool {
    crate::builtins::NAMES.contains(&name)
}

impl Default for Infer {
    fn default() -> Self {
        Infer::new()
    }
}

impl Infer {
    pub fn new() -> Infer {
        let mut g = Graph::default();
        let decl = g.add(Node::Decl);
        let top = g.add(Node::Var);
        Infer {
            s: Subst::default(),
            g,
            decl,
            globals: HashMap::new(),
            locals: vec![],
            blocks: HashMap::new(),
            ctors: HashMap::new(),
            modules: HashMap::new(),
            pos: vec![],
            effects: vec![top],
            pending: vec![],
            scope_depth: 0,
            inst_frames: vec![],
            reported: HashSet::new(),
            typed: vec![],
            blamed: HashSet::new(),
            clashes: vec![],
            errors: vec![],
            defs: vec![],
        }
    }

    // ------------------------------------------------------------ nodes

    fn ann(&mut self) -> u32 {
        self.g.add(Node::Var)
    }

    fn src(&mut self, desc: String) -> u32 {
        let pos = self.pos.last().copied();
        self.g.add(Node::Src { desc, pos, secret: false })
    }

    fn secret_src(&mut self, desc: String) -> u32 {
        let pos = self.pos.last().copied();
        self.g.add(Node::Src { desc, pos, secret: true })
    }

    /// Every stage node in a type, all the way down.
    fn all_anns(&self, t: &Ty, out: &mut Vec<u32>) {
        match self.resolve(t) {
            Ty::Dyn(a) | Ty::P(_, a) => out.push(a),
            Ty::Vec(e, a) | Ty::Map(e, a) => {
                out.push(a);
                self.all_anns(&e, out);
            }
            Ty::Rec(f, _, a) => {
                out.push(a);
                for (t, _) in f.values() {
                    self.all_anns(t, out);
                }
            }
            _ => {}
        }
    }

    /// No sensitive value may flow into this type.
    fn secret_sink(&mut self, t: &Ty, code: &'static str, msg: String, help: &'static str) {
        let mut anns = vec![];
        self.all_anns(t, &mut anns);
        if anns.is_empty() && matches!(self.resolve(t), Ty::Var(_)) {
            // an unconstrained parameter: give it a stage node so callers' secrets reach here
            anns.push(self.stage(t));
        }
        if anns.is_empty() {
            return;
        }
        let pos = self.pos.last().copied();
        let s = self.g.add(Node::Sink { code, msg, help, pos, secret: true });
        for a in anns {
            self.g.edge(a, s);
        }
    }

    fn link(&mut self, a: u32, b: u32) {
        self.g.edge(a, b);
        self.g.edge(b, a);
    }

    fn fresh(&mut self) -> Ty {
        self.s.tys.push(None);
        Ty::Var(self.s.tys.len() as u32 - 1)
    }

    fn fresh_row(&mut self) -> Tail {
        self.s.rows.push(None);
        Tail::Open(self.s.rows.len() as u32 - 1)
    }

    fn prim(&mut self, p: Prim) -> Ty {
        let a = self.ann();
        Ty::P(p, a)
    }

    fn dyn_(&mut self) -> Ty {
        let a = self.ann();
        Ty::Dyn(a)
    }

    /// The stage node at the top of a type, if it has one.
    fn top(&self, t: &Ty) -> Option<u32> {
        match self.resolve(t) {
            Ty::Dyn(a) | Ty::P(_, a) | Ty::Vec(_, a) | Ty::Map(_, a) | Ty::Rec(_, _, a) => Some(a),
            _ => None,
        }
    }

    /// The stage node of a type; an unconstrained variable becomes `?`.
    fn stage(&mut self, t: &Ty) -> u32 {
        if let Some(a) = self.top(t) {
            return a;
        }
        match self.resolve(t) {
            Ty::Var(x) => {
                let a = self.ann();
                self.s.tys[x as usize] = Some(Ty::Dyn(a));
                a
            }
            _ => self.ann(), // handles, functions, nil: compile-time
        }
    }

    fn sink(&mut self, t: &Ty, k: (&'static str, &'static str, &'static str)) {
        let a = self.stage(t);
        let pos = self.pos.last().copied();
        let s = self.g.add(Node::Sink { code: k.0, msg: k.1.to_string(), help: k.2, pos, secret: false });
        self.g.edge(a, s);
    }

    fn emit_decl(&mut self) {
        let top = *self.effects.last().unwrap();
        self.g.edge(self.decl, top);
    }

    // ------------------------------------------------------------ substitution

    fn resolve(&self, t: &Ty) -> Ty {
        match t {
            Ty::Var(v) => match &self.s.tys[*v as usize] {
                Some(b) => self.resolve(b),
                None => t.clone(),
            },
            Ty::Rec(f, tail, a) => {
                let (f, tail) = self.row(f.clone(), *tail);
                Ty::Rec(f, tail, *a)
            }
            _ => t.clone(),
        }
    }

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
            Ty::Vec(e, _) | Ty::Map(e, _) => self.occurs(v, &e),
            Ty::Rec(f, _, _) => f.values().any(|(t, _)| self.occurs(v, t)),
            Ty::Fn(ps, r, ret, _) => {
                ps.iter().any(|p| self.occurs(v, p)) || r.map_or(false, |r| self.occurs(v, &r)) || self.occurs(v, &ret)
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ unification

    /// Unify shapes. With `link`, stage nodes at matching positions are
    /// made equal; without, only shapes are compared (used by `join`).
    fn unify(&mut self, a: &Ty, b: &Ty, link: bool) -> Result<(), String> {
        let (a, b) = (self.resolve(a), self.resolve(b));
        let l = |me: &mut Self, x: u32, y: u32| {
            if link {
                me.link(x, y)
            }
        };
        match (&a, &b) {
            (Ty::Nil, _) | (_, Ty::Nil) => Ok(()),
            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Dyn(x), t) | (t, Ty::Dyn(x)) => {
                match t {
                    // an unconstrained variable becomes `?` too, so that stage and
                    // secrecy flow on through it
                    Ty::Var(v) if link => self.s.tys[*v as usize] = Some(Ty::Dyn(*x)),
                    _ => {
                        if let Some(y) = self.top(t) {
                            l(self, *x, y);
                        }
                    }
                }
                Ok(())
            }
            (Ty::Var(x), t) | (t, Ty::Var(x)) => {
                if !self.occurs(*x, t) {
                    self.s.tys[*x as usize] = Some(t.clone());
                }
                Ok(())
            }
            (Ty::P(p, x), Ty::P(q, y)) => {
                let ok = p == q || matches!((p, q), (Prim::Str, Prim::Kw) | (Prim::Kw, Prim::Str));
                if !ok {
                    return Err(format!("{} where {} is expected", self.show(&a), self.show(&b)));
                }
                l(self, *x, *y);
                Ok(())
            }
            (Ty::Module, Ty::Module) => Ok(()),
            (Ty::Vec(e1, x), Ty::Vec(e2, y)) | (Ty::Map(e1, x), Ty::Map(e2, y)) => {
                l(self, *x, *y);
                self.unify(e1, e2, link)
            }
            (Ty::Rec(f, _, x), Ty::Map(v, y)) | (Ty::Map(v, y), Ty::Rec(f, _, x)) => {
                l(self, *x, *y);
                for (t, _) in f.values() {
                    self.unify(t, v, link)?;
                }
                Ok(())
            }
            (Ty::Rec(f1, t1, x), Ty::Rec(f2, t2, y)) => {
                l(self, *x, *y);
                self.unify_rows(f1.clone(), *t1, f2.clone(), *t2, link)
            }
            (Ty::Res(t, d, _, _), Ty::Rec(f, _, _)) | (Ty::Rec(f, _, _), Ty::Res(t, d, _, _)) => {
                let (t, d, f) = (t.clone(), *d, f.clone());
                for (k, (ft, _)) in f {
                    if let Some(at) = self.res_attr(&t, d, &k, None) {
                        self.unify(&ft, &at, link)?;
                    }
                }
                Ok(())
            }
            (Ty::Res(x, dx, _, _), Ty::Res(y, dy, _, _)) if x == y && dx == dy => Ok(()),
            (Ty::Module, Ty::Rec(..)) | (Ty::Rec(..), Ty::Module) => Ok(()),
            (Ty::Fn(p1, r1, ret1, e1), Ty::Fn(p2, r2, ret2, e2)) => {
                if p1.len() != p2.len() && (r1.is_none() && p1.len() < p2.len() || r2.is_none() && p2.len() < p1.len()) {
                    return Err(format!("functions of {} and {} arguments", p1.len(), p2.len()));
                }
                l(self, *e1, *e2);
                for i in 0..p1.len().min(p2.len()) {
                    self.unify(&p1[i], &p2[i], link)?;
                }
                self.unify(ret1, ret2, link)
            }
            _ => Err(format!("{} where {} is expected", self.show(&a), self.show(&b))),
        }
    }

    fn unify_rows(&mut self, f1: Fields, t1: Tail, f2: Fields, t2: Tail, link: bool) -> Result<(), String> {
        for (k, (a, _)) in &f1 {
            if let Some((b, _)) = f2.get(k) {
                self.unify(a, b, link)?;
            }
        }
        let only1: Fields = f1.iter().filter(|(k, _)| !f2.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let only2: Fields = f2.iter().filter(|(k, _)| !f1.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        if t1 == t2 && t1 != Tail::Closed && (!only1.is_empty() || !only2.is_empty()) {
            return Ok(());
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

    /// Unify, recording a shape clash on failure. The program goes on with `?`.
    fn expect(&mut self, found: &Ty, expected: &Ty, what: &str) {
        let snapshot = self.s.clone();
        if let Err(e) = self.unify(found, expected, true) {
            self.s = snapshot;
            self.clash(format!("{}: {}", what, e));
        }
    }

    fn clash(&mut self, msg: String) {
        let pos = self.pos.last().copied();
        self.clashes.push(Clash { msg, pos });
    }

    /// A copy of a shape with fresh stage nodes.
    fn copy_fresh(&mut self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Dyn(_) => self.dyn_(),
            Ty::P(p, _) => self.prim(p),
            Ty::Vec(e, _) => {
                let e = self.copy_fresh(&e);
                Ty::Vec(Box::new(e), self.ann())
            }
            Ty::Map(e, _) => {
                let e = self.copy_fresh(&e);
                Ty::Map(Box::new(e), self.ann())
            }
            Ty::Rec(f, tail, _) => {
                let mut g = Fields::new();
                for (k, (t, o)) in f {
                    let c = self.copy_fresh(&t);
                    g.insert(k, (c, o));
                }
                Ty::Rec(g, tail, self.ann())
            }
            other => other,
        }
    }

    /// Stage flow from one shape into another, position by position.
    fn flow(&mut self, from: &Ty, to: &Ty) {
        let (a, b) = (self.resolve(from), self.resolve(to));
        if let (Some(x), Some(y)) = (self.top(&a), self.top(&b)) {
            self.g.edge(x, y);
        }
        match (&a, &b) {
            (Ty::Vec(x, _), Ty::Vec(y, _)) | (Ty::Map(x, _), Ty::Map(y, _)) => self.flow(x, y),
            (Ty::Rec(f, _, _), Ty::Rec(g, _, _)) => {
                for (k, (t, _)) in f {
                    if let Some((u, _)) = g.get(k) {
                        let (t, u) = (t.clone(), u.clone());
                        self.flow(&t, &u);
                    }
                }
            }
            (Ty::Rec(f, _, _), Ty::Map(v, _)) => {
                let v = (**v).clone();
                for (t, _) in f.values() {
                    let t = t.clone();
                    self.flow(&t, &v);
                }
            }
            _ => {}
        }
    }

    /// The least common shape of two types, with stage flowing from both.
    fn join(&mut self, a: &Ty, b: &Ty) -> Ty {
        match (self.resolve(a), self.resolve(b)) {
            (Ty::Nil, _) => return self.resolve(b),
            (_, Ty::Nil) => return self.resolve(a),
            _ => {}
        }
        let snapshot = self.s.clone();
        if self.unify(a, b, false).is_ok() {
            let r = self.copy_fresh(a);
            self.flow(a, &r);
            self.flow(b, &r);
            return r;
        }
        self.s = snapshot;
        let r = match (self.resolve(a), self.resolve(b)) {
            (Ty::Rec(f1, _, _), Ty::Rec(f2, _, _)) => {
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
                Ty::Rec(out, Tail::Closed, self.ann())
            }
            (Ty::Vec(x, _), Ty::Vec(y, _)) => {
                let j = self.join(&x, &y);
                Ty::Vec(Box::new(j), self.ann())
            }
            (Ty::Map(x, _), Ty::Map(y, _)) => {
                let j = self.join(&x, &y);
                Ty::Map(Box::new(j), self.ann())
            }
            _ => self.dyn_(),
        };
        if let Some(r_top) = self.top(&r) {
            for t in [a, b] {
                if let Some(x) = self.top(t) {
                    self.g.edge(x, r_top);
                }
            }
        }
        r
    }

    // ------------------------------------------------------------ schemas

    fn shape_of_type(t: &J, a: u32) -> Ty {
        match t {
            J::String(x) => match x.as_str() {
                "string" => Ty::P(Prim::Str, a),
                "number" => Ty::P(Prim::Num, a),
                "bool" => Ty::P(Prim::Bool, a),
                _ => Ty::Dyn(a),
            },
            J::Array(x) => match (x.first().and_then(|k| k.as_str()), x.get(1)) {
                (Some("list" | "set"), Some(e)) => Ty::Vec(Box::new(Self::shape_of_type(e, a)), a),
                (Some("map"), Some(e)) => Ty::Map(Box::new(Self::shape_of_type(e, a)), a),
                (Some("object"), Some(J::Object(attrs))) => Ty::Rec(
                    attrs.iter().map(|(k, v)| (k.clone(), (Self::shape_of_type(v, a), false))).collect(),
                    Tail::Closed,
                    a,
                ),
                _ => Ty::Vec(Box::new(Ty::Dyn(a)), a),
            },
            _ => Ty::Dyn(a),
        }
    }

    fn shape_of_block(&mut self, b: &J, a: u32, path: &str) -> Ty {
        let mut f = Fields::new();
        if let Some(attrs) = b["attributes"].as_object() {
            for (k, x) in attrs {
                let t = self.shape_of_attr(x, a, &format!("{}.{}", path, k));
                f.insert(k.clone(), (t, false));
            }
        }
        if let Some(bts) = b["block_types"].as_object() {
            for (k, bt) in bts {
                let t = self.shape_of_block_type(bt, a, &format!("{}.{}", path, k));
                f.insert(k.clone(), (t, false));
            }
        }
        Ty::Rec(f, Tail::Closed, a)
    }

    fn shape_of_attr(&mut self, x: &J, a: u32, path: &str) -> Ty {
        let a = if x["sensitive"] == J::Bool(true) { self.secret_src(format!("`{}`", path)) } else { a };
        match x.get("nested_type") {
            Some(nt) => {
                let inner = self.shape_of_block(nt, a, path);
                match nt["nesting_mode"].as_str().unwrap_or("single") {
                    "single" => inner,
                    "map" => Ty::Map(Box::new(inner), a),
                    _ => Ty::Vec(Box::new(inner), a),
                }
            }
            None => Self::shape_of_type(&x["type"], a),
        }
    }

    fn shape_of_block_type(&mut self, bt: &J, a: u32, path: &str) -> Ty {
        let inner = self.shape_of_block(&bt["block"], a, &format!("{}[]", path));
        match bt["nesting_mode"].as_str().unwrap_or("list") {
            "single" | "group" => inner,
            "map" => Ty::Map(Box::new(inner), a),
            _ => Ty::Vec(Box::new(inner), a),
        }
    }

    /// The shape of an attribute of a resource type, all of it known only
    /// after apply. With `desc`, a source node is made for the read.
    fn res_attr(&mut self, ty: &str, data: bool, k: &str, desc: Option<String>) -> Option<Ty> {
        let block = self.blocks.get(&(ty.to_string(), data))?.clone()?;
        let path = desc.as_deref().map(|d| d.trim_matches('`').to_string()).unwrap_or_else(|| format!("{}.{}", ty, k));
        let a = match desc {
            Some(d) => self.src(d),
            None => self.ann(),
        };
        if let Some(x) = block["attributes"].get(k) {
            return Some(self.shape_of_attr(x, a, &path));
        }
        if let Some(bt) = block["block_types"].get(k) {
            return Some(self.shape_of_block_type(bt, a, &path));
        }
        None
    }

    // ------------------------------------------------------------ access

    fn field(&mut self, t: &Ty, k: &str) -> Ty {
        match self.resolve(t) {
            Ty::Rec(f, tail, _) => match f.get(k) {
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
                    Tail::Closed => Ty::Nil,
                },
            },
            Ty::Map(v, _) => *v,
            Ty::Res(ty, _, _, 2) => {
                let s = self.src(format!("`{}` of the instances of a {}", k, ty));
                Ty::Dyn(s)
            }
            Ty::Res(ty, d, name, _) => {
                let desc = match &name {
                    Some(n) if ty.starts_with("module ") => format!("`module.{}.{}`", n, k),
                    None if ty.starts_with("module ") => format!("the output `{}` of a module", k),
                    Some(n) => format!("`{}{}.{}.{}`", if d { "data." } else { "" }, ty, n, k),
                    None => format!("`{}` of a {}", k, ty),
                };
                match self.res_attr(&ty, d, k, Some(desc.clone())) {
                    Some(t) => t,
                    None => {
                        // a known schema without this attribute: the read is wrong
                        if let Some(Some(block)) = self.blocks.get(&(ty.to_string(), d)).cloned() {
                            let what = match (&name, ty.strip_prefix("module ")) {
                                (Some(n), Some(_)) => format!("module.{}", n),
                                (None, Some(_)) => "module".to_string(),
                                (Some(n), None) => format!("{}{}.{}", if d { "data." } else { "" }, ty, n),
                                (None, None) => format!("{}{}", if d { "data " } else { "" }, ty),
                            };
                            let help = crate::check::suggest(k, crate::check::readable(&block)).map(|s| format!("did you mean `{}`?", s));
                            self.arg_error("K0707", &what, format!("has no attribute `{}`", k), help, None);
                        }
                        let s = self.src(desc);
                        Ty::Dyn(s)
                    }
                }
            }
            Ty::Module => {
                let s = self.src(format!("the module output `{}`", k));
                Ty::Dyn(s)
            }
            Ty::Var(x) => {
                let v = self.fresh();
                let rest = self.fresh_row();
                let mut f = Fields::new();
                f.insert(k.to_string(), (v.clone(), true));
                let a = self.ann();
                self.s.tys[x as usize] = Some(Ty::Rec(f, rest, a));
                v
            }
            Ty::Dyn(a) => {
                let b = self.ann();
                self.g.edge(a, b);
                Ty::Dyn(b)
            }
            Ty::Vec(..) | Ty::Nil => Ty::Nil,
            other => {
                let shown = self.show(&other);
                self.clash(format!("cannot look up :{} in {}", k, shown));
                self.dyn_()
            }
        }
    }

    /// The type of the elements of something iterated. The collection itself
    /// must be known at compile time.
    fn element(&mut self, t: &Ty) -> Ty {
        self.sink(t, K0313);
        match self.resolve(t) {
            Ty::Vec(e, _) => *e,
            Ty::Dyn(a) => {
                let b = self.ann();
                self.g.edge(a, b);
                Ty::Dyn(b)
            }
            Ty::Map(..) | Ty::Rec(..) | Ty::Nil | Ty::Var(_) | Ty::Module => self.dyn_(),
            other => {
                let shown = self.show(&other);
                self.clash(format!("{} is not a collection", shown));
                self.dyn_()
            }
        }
    }

    // ------------------------------------------------------------ environment

    fn lookup(&mut self, name: &str) -> Option<Ty> {
        if let Some((_, t)) = self.locals.iter().rev().find(|(n, _)| n == name) {
            return Some(t.clone());
        }
        let (vars, rows, anns, edges, ty) = match self.globals.get(name) {
            Some(sc) => (sc.vars.clone(), sc.rows.clone(), sc.anns.clone(), sc.edges.clone(), sc.ty.clone()),
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
        let mut am = HashMap::new();
        for a in anns {
            let f = self.ann();
            am.insert(a, f);
        }
        for (x, y) in edges {
            let x2 = *am.get(&x).unwrap_or(&x);
            let y2 = *am.get(&y).unwrap_or(&y);
            self.g.edge(x2, y2);
        }
        Some(self.instantiate(&ty, &vm, &rm, &am))
    }

    fn instantiate(&self, t: &Ty, vm: &HashMap<u32, Ty>, rm: &HashMap<u32, Tail>, am: &HashMap<u32, u32>) -> Ty {
        let a = |x: &u32| *am.get(x).unwrap_or(x);
        match t {
            Ty::Var(v) => vm.get(v).cloned().unwrap_or_else(|| t.clone()),
            Ty::Dyn(x) => Ty::Dyn(a(x)),
            Ty::P(p, x) => Ty::P(*p, a(x)),
            Ty::Vec(e, x) => Ty::Vec(Box::new(self.instantiate(e, vm, rm, am)), a(x)),
            Ty::Map(e, x) => Ty::Map(Box::new(self.instantiate(e, vm, rm, am)), a(x)),
            Ty::Rec(f, tail, x) => Ty::Rec(
                f.iter().map(|(k, (t, o))| (k.clone(), (self.instantiate(t, vm, rm, am), *o))).collect(),
                match tail {
                    Tail::Open(r) => rm.get(r).copied().unwrap_or(*tail),
                    Tail::Closed => Tail::Closed,
                },
                a(x),
            ),
            Ty::Fn(ps, r, ret, e) => Ty::Fn(
                ps.iter().map(|p| self.instantiate(p, vm, rm, am)).collect(),
                r.as_ref().map(|r| Box::new(self.instantiate(r, vm, rm, am))),
                Box::new(self.instantiate(ret, vm, rm, am)),
                a(e),
            ),
            other => other.clone(),
        }
    }

    pub fn zonk(&self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Vec(e, a) => Ty::Vec(Box::new(self.zonk(&e)), a),
            Ty::Map(e, a) => Ty::Map(Box::new(self.zonk(&e)), a),
            Ty::Rec(f, tail, a) => Ty::Rec(f.iter().map(|(k, (t, o))| (k.clone(), (self.zonk(t), *o))).collect(), tail, a),
            Ty::Fn(ps, r, ret, e) => Ty::Fn(
                ps.iter().map(|p| self.zonk(p)).collect(),
                r.map(|r| Box::new(self.zonk(&r))),
                Box::new(self.zonk(&ret)),
                e,
            ),
            t => t,
        }
    }

    fn free(&self, t: &Ty, vars: &mut Vec<u32>, rows: &mut Vec<u32>, anns: &mut Vec<u32>) {
        let ann = |x: u32, anns: &mut Vec<u32>| {
            if self.g.is_var(x) && !anns.contains(&x) {
                anns.push(x)
            }
        };
        match t {
            Ty::Var(v) => {
                if !vars.contains(v) {
                    vars.push(*v)
                }
            }
            Ty::Dyn(a) | Ty::P(_, a) => ann(*a, anns),
            Ty::Vec(e, a) | Ty::Map(e, a) => {
                ann(*a, anns);
                self.free(e, vars, rows, anns)
            }
            Ty::Rec(f, tail, a) => {
                ann(*a, anns);
                f.values().for_each(|(t, _)| self.free(t, vars, rows, anns));
                if let Tail::Open(r) = tail {
                    if !rows.contains(r) {
                        rows.push(*r)
                    }
                }
            }
            Ty::Fn(ps, r, ret, e) => {
                ann(*e, anns);
                ps.iter().for_each(|p| self.free(p, vars, rows, anns));
                if let Some(r) = r {
                    self.free(r, vars, rows, anns);
                }
                self.free(ret, vars, rows, anns);
            }
            _ => {}
        }
    }

    /// The part of the graph a generalised type needs: edges between its
    /// nodes, from its nodes to sinks, and from sources to its nodes, each
    /// summarising a path through nodes private to the definition.
    fn scheme_edges(&self, anns: &[u32]) -> Vec<(u32, u32)> {
        let q: HashSet<u32> = anns.iter().copied().collect();
        let mut edges = vec![];
        let walk = |start: u32, edges: &mut Vec<(u32, u32)>, to_q_only: bool| {
            let mut seen = HashSet::new();
            let mut stack = vec![start];
            while let Some(n) = stack.pop() {
                for &m in &self.g.out[n as usize] {
                    if !seen.insert(m) {
                        continue;
                    }
                    if q.contains(&m) {
                        edges.push((start, m));
                    } else if self.g.is_var(m) {
                        stack.push(m);
                    } else if !to_q_only && matches!(self.g.nodes[m as usize], Node::Sink { .. }) {
                        edges.push((start, m));
                    }
                }
            }
        };
        for &a in anns {
            walk(a, &mut edges, false);
        }
        for (i, n) in self.g.nodes.iter().enumerate() {
            if matches!(n, Node::Src { .. } | Node::Decl) {
                walk(i as u32, &mut edges, true);
            }
        }
        edges
    }

    fn define(&mut self, name: &str, t: &Ty) {
        let ty = self.zonk(t);
        let (mut vars, mut rows, mut anns) = (vec![], vec![], vec![]);
        self.free(&ty, &mut vars, &mut rows, &mut anns);
        let edges = self.scheme_edges(&anns);
        self.defs.push((name.to_string(), ty.clone()));
        self.globals.insert(name.to_string(), Scheme { vars, rows, anns, edges, ty });
    }

    // ------------------------------------------------------------ patterns

    fn bind(&mut self, pat: &Value, t: Ty) {
        match pat {
            Value::Symbol(s) if &**s != "&" => self.locals.push((s.to_string(), t)),
            Value::Vector(ps) => {
                let elem = match self.resolve(&t) {
                    Ty::Vec(e, _) => *e,
                    Ty::Dyn(a) => {
                        let b = self.ann();
                        self.g.edge(a, b);
                        Ty::Dyn(b)
                    }
                    _ => self.dyn_(),
                };
                let mut i = 0;
                while i < ps.len() {
                    match &ps[i] {
                        Value::Symbol(s) if &**s == "&" => {
                            if let Some(p) = ps.get(i + 1) {
                                let a = self.ann();
                                self.bind(p, Ty::Vec(Box::new(elem.clone()), a));
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
                                _ => self.dyn_(),
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
            Value::Bool(_) => self.prim(Prim::Bool),
            Value::Int(_) | Value::Float(_) => self.prim(Prim::Num),
            Value::Str(_) | Value::Named(_) => self.prim(Prim::Str),
            Value::Keyword(_) => self.prim(Prim::Kw),
            Value::Symbol(s) => match self.lookup(s) {
                Some(t) => t,
                None => self.dyn_(),
            },
            Value::Vector(items) => {
                let mut elem = Ty::Nil;
                for it in items.iter() {
                    let t = self.infer(it);
                    elem = self.join(&elem, &t);
                }
                let elem = match elem {
                    Ty::Nil if items.is_empty() => self.fresh(),
                    e => e,
                };
                Ty::Vec(Box::new(elem), self.ann())
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
                        let opt = matches!(self.resolve(&t), Ty::Nil);
                        f.insert(name, (t, opt));
                    }
                    Ty::Rec(f, Tail::Closed, self.ann())
                } else {
                    let mut vt = Ty::Nil;
                    for (k, v) in entries.iter() {
                        self.infer(k);
                        let t = self.infer(v);
                        vt = self.join(&vt, &t);
                    }
                    Ty::Map(Box::new(vt), self.ann())
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
            _ => self.dyn_(),
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

    /// Infer with a fresh effect node, which flows into the enclosing one.
    fn branch(&mut self, f: impl FnOnce(&mut Self) -> Ty, eff: u32) -> Ty {
        self.effects.push(eff);
        let t = f(self);
        self.effects.pop();
        let top = *self.effects.last().unwrap();
        self.g.edge(eff, top);
        t
    }

    fn infer_list(&mut self, items: &[Value]) -> Ty {
        let head = items[0].sym_name().unwrap_or("");
        if !self.shadowed(head) {
            match head {
                "quote" | "quasiquote" => return self.dyn_(),
                "if" => {
                    let c = match items.get(1) {
                        Some(c) => self.infer(c),
                        None => Ty::Nil,
                    };
                    let eff = self.ann();
                    let a = self.branch(|me| items.get(2).map(|f| me.infer(f)).unwrap_or(Ty::Nil), eff);
                    let b = self.branch(|me| items.get(3).map(|f| me.infer(f)).unwrap_or(Ty::Nil), eff);
                    let cs = self.stage(&c);
                    let pos = self.pos.last().copied();
                    self.pending.push(Pending { branch: eff, cond: cs, pos });
                    let r = self.join(&a, &b);
                    // a value chosen by a condition known after apply is known after apply
                    match self.top(&r) {
                        Some(rt) => self.g.edge(cs, rt),
                        None => {}
                    }
                    return r;
                }
                "do" => return self.body(&items[1..]),
                "and" | "or" => {
                    let mut t = Ty::Nil;
                    for f in &items[1..] {
                        let u = self.infer(f);
                        self.sink(&u, K0315);
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
                                        let c = me.infer(&pair[1]);
                                        // a filter decides which iterations exist
                                        me.sink(&c, K0312);
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
                        let a = me.ann();
                        Ty::Vec(Box::new(b), a)
                    })
                }
                "scope" => {
                    if let Some(n) = items.get(1) {
                        let t = self.infer(n);
                        self.sink(&t, K0314);
                    }
                    self.scope_depth += 1;
                    let r = self.body(&items[2.min(items.len())..]);
                    self.scope_depth -= 1;
                    return r;
                }
                "count-deferred" => {
                    // The condition is evaluated by the engine: no stage requirement.
                    if let Some(c) = items.get(1) {
                        let t = self.infer(c);
                        self.secret_sink(&t, "K0320", "`count` depends on a secret; Terraform rejects sensitive values here".into(), "");
                    }
                    self.inst_frames.push(1);
                    let r = self.body(&items[2.min(items.len())..]);
                    self.inst_frames.pop();
                    return r;
                }
                "for-each-deferred" => {
                    let (pat, coll) = match items.get(1) {
                        Some(Value::Vector(b)) if b.len() == 2 => (b[0].clone(), b[1].clone()),
                        _ => return self.dyn_(),
                    };
                    let ct = self.infer(&coll);
                    self.secret_sink(&ct, "K0320", "`for_each` depends on a secret; Terraform rejects sensitive values here".into(), "");
                    self.inst_frames.push(2);
                    let r = self.scoped(|me| {
                        match &pat {
                            Value::Symbol(x) => {
                                let s = me.src("`each.value`".into());
                                me.locals.push((x.to_string(), Ty::Dyn(s)));
                            }
                            Value::Vector(kv) => {
                                for (p, k) in kv.iter().zip(["key", "value"]) {
                                    if let Value::Symbol(x) = p {
                                        let s = me.src(format!("`each.{}`", k));
                                        me.locals.push((x.to_string(), Ty::Dyn(s)));
                                    }
                                }
                            }
                            _ => {}
                        }
                        me.body(&items[2..])
                    });
                    self.inst_frames.pop();
                    return r;
                }
                "data" => {
                    let ty = items.get(1).and_then(|c| c.sym_name()).and_then(|c| self.ctors.get(c).cloned());
                    let name = self.declaration(&items[2.min(items.len())..], ty.as_ref().map(|t| (t.as_str(), true)));
                    return match ty {
                        Some(t) => Ty::Res(t, true, name, self.inst_kind()),
                        None => self.dyn_(),
                    };
                }
                "module" => return self.infer_module(items),
                "output" => {
                    let name = match items.get(1) {
                        Some(n) => {
                            let t = self.infer(n);
                            self.sink(&t, K0314);
                            match n {
                                Value::Str(s) => s.to_string(),
                                _ => "this".into(),
                            }
                        }
                        None => "this".into(),
                    };
                    let v = items.get(2).map(|f| self.infer(f));
                    for a in &items[3.min(items.len())..] {
                        self.infer(a);
                    }
                    let sensitive = match items.get(3) {
                        Some(Value::Map(m)) => m.iter().any(|(k, v)| {
                            matches!(k, Value::Keyword(x) if &**x == "sensitive") && matches!(v, Value::Bool(true))
                        }),
                        _ => false,
                    };
                    if let (Some(v), false) = (v, sensitive) {
                        self.secret_sink(
                            &v,
                            "K0321",
                            format!("output `{}` exposes a secret", name),
                            "mark the output sensitive: (output \"name\" value {:sensitive true})",
                        );
                    }
                    return Ty::Nil;
                }
                "provider" | "backend" => {
                    for a in &items[2.min(items.len())..] {
                        self.infer(a);
                    }
                    return Ty::Nil;
                }
                "def" | "defn" | "defmacro" | "ns" => return Ty::Nil,
                _ => {}
            }
        }
        if let Some(sym) = items[0].sym_name() {
            if sym.contains('/') && !self.shadowed(sym) {
                if let Some(f) = sym.strip_prefix("tf/") {
                    let ts: Vec<Ty> = items[1..].iter().map(|a| self.infer(a)).collect();
                    let s = if f == "expr" || f == "if" || f == "and" || f == "or" {
                        self.src(format!("the Terraform expression at line {}", self.line()))
                    } else {
                        self.src(format!("the result of the Terraform function `{}`", f))
                    };
                    // The result is known after apply (the source), and what goes into
                    // a Terraform function comes out of it, secrets included, unless
                    // the function is Terraform's own declassification.
                    let r = self.ann();
                    self.g.edge(s, r);
                    if f != "nonsensitive" {
                        for t in &ts {
                            let mut anns = vec![];
                            self.all_anns(t, &mut anns);
                            if anns.is_empty() && matches!(self.resolve(t), Ty::Var(_)) {
                                anns.push(self.stage(t));
                            }
                            for a in anns {
                                self.g.edge(a, r);
                            }
                        }
                    }
                    return Ty::Dyn(r);
                }
                let ctor = self.ctors.get(sym).cloned();
                let name = self.declaration(&items[1..], ctor.as_ref().map(|t| (t.as_str(), false)));
                return match self.ctors.get(sym).cloned() {
                    Some(t) => Ty::Res(t, false, name, self.inst_kind()),
                    None => self.dyn_(),
                };
            }
            if is_builtin(sym) && !self.shadowed(sym) {
                return self.builtin(sym, &items[1..]);
            }
        }
        if let Value::Keyword(k) = &items[0] {
            let m = match items.get(1) {
                Some(f) => self.infer(f),
                None => self.dyn_(),
            };
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

    fn inst_kind(&self) -> u8 {
        self.inst_frames.iter().copied().max().unwrap_or(0)
    }

    fn line(&self) -> u32 {
        self.pos.last().map_or(0, |p| p.line)
    }

    /// A declaration: its name must be known at compile time, and it is an
    /// effect. With a known resource type, its arguments are checked against
    /// the schema. Returns the address name when it is a literal at top level.
    fn declaration(&mut self, args: &[Value], ctor: Option<(&str, bool)>) -> Option<Rc<str>> {
        let literal = match args.first() {
            Some(Value::Str(s)) if self.scope_depth == 0 => Some(s.clone()),
            _ => None,
        };
        if let Some(n) = args.first() {
            let t = self.infer(n);
            self.sink(&t, K0314);
        }
        let mut body = None;
        for (i, a) in args.iter().enumerate().skip(1) {
            let t = self.infer(a);
            if i == 1 {
                body = Some(t);
            }
        }
        self.emit_decl();
        if let (Some((ty, data)), Some(body)) = (ctor, body) {
            if let Some(Some(block)) = self.blocks.get(&(ty.to_string(), data)).cloned() {
                let what = match &literal {
                    Some(n) => format!("{}{}.{}", if data { "data." } else { "" }, ty, n),
                    None => format!("{}{}", if data { "data " } else { "" }, ty),
                };
                let meta: &[&str] = if data { DATA_META } else { RESOURCE_META };
                self.check_args(&block, &body, &what, "", meta);
            }
        }
        literal
    }

    /// A module call: its name is a declaration name, its inputs are checked
    /// against the module's variables, and its outputs can be read.
    fn infer_module(&mut self, items: &[Value]) -> Ty {
        let literal = match items.get(1) {
            Some(Value::Str(s)) if self.scope_depth == 0 => Some(s.clone()),
            _ => None,
        };
        if let Some(n) = items.get(1) {
            let t = self.infer(n);
            self.sink(&t, K0314);
        }
        let mut merged = Fields::new();
        let mut closed = true;
        for f in &items[2.min(items.len())..] {
            let t = self.infer(f);
            match self.resolve(&t) {
                Ty::Rec(fs, tail, _) => {
                    closed &= tail == Tail::Closed;
                    merged.extend(fs);
                }
                _ => closed = false,
            }
        }
        self.emit_decl();
        let source = match items.get(2) {
            Some(Value::Map(m)) => match map_get(m, &Value::kw("source")) {
                Some(Value::Str(s)) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        };
        let iface = source.as_ref().and_then(|s| self.modules.get(s).cloned().flatten());
        let iface = match iface {
            Some(i) => i,
            None => return Ty::Module,
        };
        let what = match &literal {
            Some(n) => format!("module.{}", n),
            None => "module".to_string(),
        };
        let tail = if closed { Tail::Closed } else { self.fresh_row() };
        let a = self.ann();
        let inputs = (*iface.inputs).clone();
        self.check_args(&inputs, &Ty::Rec(merged, tail, a), &what, "", crate::check::MODULE_META);
        let ty = format!("module {}", iface.key);
        self.blocks.insert((ty.clone(), false), Some(iface.outputs.clone()));
        Ty::Res(ty, false, literal, self.inst_kind())
    }

    fn arg_error(&mut self, code: &'static str, what: &str, msg: String, help: Option<String>, stage: Option<u32>) {
        let pos = self.pos.last().copied();
        let e = StageError { code, msg: format!("{}: {}", what, msg), help: help.unwrap_or_default(), pos };
        match stage {
            Some(a) => self.typed.push((a, e)),
            None => {
                if !self.errors.iter().any(|x| x.code == e.code && x.pos == e.pos && x.msg == e.msg) {
                    self.errors.push(e)
                }
            }
        }
    }

    /// Check an argument map's type against a schema block, before evaluation.
    /// Only what is certain is reported: a field whose type is `?` or not yet
    /// known passes, and an open record is not checked for completeness.
    fn check_args(&mut self, block: &J, t: &Ty, what: &str, path: &str, meta: &[&str]) {
        let (fields, closed) = match self.resolve(t) {
            Ty::Rec(f, tail, _) => (f, tail == Tail::Closed),
            Ty::Vec(e, _) => {
                // a list or set block given as a vector of maps
                return self.check_args(block, &e, what, path, meta);
            }
            _ => return,
        };
        let empty = serde_json::Map::new();
        let attrs = block["attributes"].as_object().unwrap_or(&empty).clone();
        let blocks = block["block_types"].as_object().unwrap_or(&empty).clone();
        let join = |k: &str| if path.is_empty() { k.to_string() } else { format!("{}.{}", path, k) };
        for (k, (ft, _opt)) in &fields {
            let p = join(k);
            if meta.contains(&k.as_str()) {
                if k == "count" || k == "for_each" {
                    let msg = format!("`{}` depends on a secret; Terraform rejects sensitive values here", p);
                    self.secret_sink(ft, "K0320", format!("{}: {}", what, msg), "");
                }
                continue;
            }
            if let Some(a) = attrs.get(k) {
                let flag = |x: &str| a[x] == J::Bool(true);
                if flag("computed") && !flag("optional") && !flag("required") {
                    self.arg_error(
                        "K0703",
                        what,
                        format!("`{}` is read-only; the provider computes it", p),
                        Some("read it from the resource instead, e.g. (:attr resource)".into()),
                        None,
                    );
                    continue;
                }
                if !flag("sensitive") && !flag("write_only") {
                    let msg = format!("{}: `{}` is not sensitive, but its value comes from a secret", what, p);
                    match a.get("nested_type") {
                        Some(nt) => {
                            let nt = nt.clone();
                            self.check_args(&nt, ft, what, &p, &[]);
                        }
                        None => self.secret_sink(ft, "K0320", msg, "if exposing it is intended, say so with (tf/nonsensitive x)"),
                    }
                }
                if let Some(ty) = a.get("type").cloned() {
                    self.check_type(&ty, ft, what, &p);
                }
            } else if let Some(bt) = blocks.get(k) {
                let ft_r = self.resolve(ft);
                let mode = bt["nesting_mode"].as_str().unwrap_or("list").to_string();
                let ok = match (&ft_r, mode.as_str()) {
                    (Ty::Rec(..), _) => true,
                    (Ty::Vec(..), "list" | "set") => true,
                    (Ty::Map(..), "map") => true,
                    (Ty::Var(_) | Ty::Dyn(_) | Ty::Nil, _) => true,
                    _ => false,
                };
                if !ok {
                    let shown = self.show(&ft_r);
                    self.arg_error(
                        "K0705",
                        what,
                        format!("`{}` is a nested block and must be a map, got {}", p, shown),
                        None,
                        None,
                    );
                    continue;
                }
                let inner = bt["block"].clone();
                self.check_args(&inner, ft, what, &p, &[]);
            } else if closed {
                let names: Vec<&str> =
                    attrs.keys().chain(blocks.keys()).map(|s| s.as_str()).chain(meta.iter().copied()).collect();
                let help = crate::check::suggest(k, names).map(|s| format!("did you mean `{}`?", s));
                self.arg_error("K0701", what, format!("unknown argument `{}`", p), help, None);
            }
        }
        if closed {
            for (k, a) in &attrs {
                if a["required"] == J::Bool(true) && !fields.contains_key(k) {
                    self.arg_error("K0702", what, format!("missing required argument `{}`", join(k)), None, None);
                }
            }
            for (k, bt) in &blocks {
                let min = bt["min_items"].as_u64().unwrap_or(0);
                if min > 0 && !fields.contains_key(k) {
                    self.arg_error(
                        "K0702",
                        what,
                        format!("missing required block `{}` (at least {})", join(k), min),
                        None,
                        None,
                    );
                }
            }
        }
    }

    /// A definite type mismatch between a schema type and an inferred one.
    /// Values known only after apply pass, as in the evaluator's check, so
    /// the error waits until the stage of the value is known.
    fn check_type(&mut self, ty: &J, t: &Ty, what: &str, path: &str) {
        let r = self.resolve(t);
        let wrong = match (ty, &r) {
            (_, Ty::Var(_) | Ty::Dyn(_) | Ty::Nil) => false,
            (J::String(x), Ty::P(p, _)) => match x.as_str() {
                "number" => *p != Prim::Num,
                "bool" => *p != Prim::Bool,
                _ => false,
            },
            (J::String(x), Ty::Vec(..) | Ty::Map(..) | Ty::Rec(..)) => x == "string" || x == "number" || x == "bool",
            (J::Array(a), Ty::P(..)) => matches!(a.first().and_then(|k| k.as_str()), Some("list" | "set" | "map" | "object")),
            // the attributes of an object type, and the elements of collections
            (J::Array(a), Ty::Rec(fs, _, _)) if a.first().and_then(|k| k.as_str()) == Some("object") => {
                if let Some(J::Object(attrs)) = a.get(1) {
                    for (k, (ft, _)) in fs {
                        if let Some(at) = attrs.get(k) {
                            let p = if path.is_empty() { k.clone() } else { format!("{}.{}", path, k) };
                            self.check_type(at, ft, what, &p);
                        }
                    }
                }
                false
            }
            (J::Array(a), Ty::Vec(e, _)) if matches!(a.first().and_then(|k| k.as_str()), Some("list" | "set")) => {
                if let Some(et) = a.get(1) {
                    let p = format!("{}[*]", path);
                    self.check_type(et, &e, what, &p);
                }
                false
            }
            // a vector is never a map or an object
            (J::Array(a), Ty::Vec(..)) => matches!(a.first().and_then(|k| k.as_str()), Some("map" | "object")),
            (J::Array(a), Ty::Map(v, _)) if a.first().and_then(|k| k.as_str()) == Some("map") => {
                if let Some(vt) = a.get(1) {
                    self.check_type(vt, &v, what, &format!("{}[*]", path));
                }
                false
            }
            _ => false,
        };
        if wrong {
            let expected = match ty {
                J::String(x) => x.clone(),
                J::Array(a) => format!(
                    "{}({})",
                    a.first().and_then(|k| k.as_str()).unwrap_or("?"),
                    a.get(1).map(|t| t.to_string().replace('"', "")).unwrap_or_default()
                ),
                _ => "?".into(),
            };
            let shown = self.show(&r);
            let stage = self.top(&r);
            self.arg_error("K0704", what, format!("`{}` must be {}, got {}", path, expected, shown), None, stage);
        }
    }

    fn apply(&mut self, f: &Ty, args: &[Ty], head: &Value) -> Ty {
        let name = match head {
            Value::Symbol(s) => format!("`{}`", s),
            _ => "this function".to_string(),
        };
        let top = *self.effects.last().unwrap();
        match self.resolve(f) {
            Ty::Fn(ps, rest, ret, eff) => {
                self.g.edge(eff, top);
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
                let eff = self.ann();
                self.g.edge(eff, top);
                self.s.tys[x as usize] = Some(Ty::Fn(args.to_vec(), None, Box::new(ret.clone()), eff));
                ret
            }
            Ty::Map(v, _) => *v,
            Ty::Rec(..) | Ty::Dyn(_) | Ty::Nil | Ty::Module => self.dyn_(),
            other => {
                let shown = self.show(&other);
                self.clash(format!("{} is not a function", shown));
                self.dyn_()
            }
        }
    }

    fn infer_fn(&mut self, name: Option<&str>, params: Option<&Value>, body: &[Value]) -> Ty {
        let pats = match params {
            Some(Value::Vector(p)) => p.clone(),
            _ => return self.dyn_(),
        };
        let fixed_n = pats.iter().position(|p| p.sym_name() == Some("&")).unwrap_or(pats.len());
        let ps: Vec<Ty> = (0..fixed_n).map(|_| self.fresh()).collect();
        let rest = if fixed_n < pats.len() { Some(Box::new(self.dyn_())) } else { None };
        let ret = self.fresh();
        let eff = self.ann();
        let me_ty = Ty::Fn(ps.clone(), rest.clone(), Box::new(ret.clone()), eff);
        self.effects.push(eff);
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
                        let a = me.ann();
                        Ty::Vec(Box::new(e), a)
                    }
                    _ => me.fresh(),
                };
                me.bind(rp, rt);
            }
            me.body(body)
        });
        self.effects.pop();
        self.expect(&body_t, &ret, "the result");
        me_ty
    }

    /// The result of a lifted operation: known after apply if any argument is.
    fn lifted(&mut self, p: Prim, ts: &[Ty]) -> Ty {
        let r = self.ann();
        for t in ts {
            let a = self.stage(t);
            self.g.edge(a, r);
        }
        Ty::P(p, r)
    }

    fn builtin(&mut self, name: &str, args: &[Value]) -> Ty {
        let ts: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
        let arg = |i: usize| ts.get(i).cloned().unwrap_or(Ty::Nil);
        if NUM_OPS.contains(&name) {
            for (i, t) in ts.iter().enumerate() {
                let n = self.prim(Prim::Num);
                self.expect(t, &n, &format!("argument {} of `{}`", i + 1, name));
            }
            return self.lifted(Prim::Num, &ts);
        }
        if LIFTED_BOOL.contains(&name) {
            return self.lifted(Prim::Bool, &ts);
        }
        if STATIC_BOOL.contains(&name) {
            return self.prim(Prim::Bool);
        }
        if name == "str" {
            return self.lifted(Prim::Str, &ts);
        }
        if name == "join" {
            if let Some(sep) = ts.first() {
                self.sink(sep, K0316);
            }
            return self.lifted(Prim::Str, &ts);
        }
        if TEXT_ARG.contains(&name) {
            if let Some(t) = ts.first() {
                self.sink(t, K0316);
            }
            return if name == "keyword" { self.prim(Prim::Kw) } else { self.prim(Prim::Str) };
        }
        if COLLECTION_ARG.contains(&name) {
            if let Some(t) = ts.first() {
                self.sink(t, K0313);
            }
        }
        if COLLECTION_ARGS.contains(&name) {
            for t in &ts {
                self.sink(t, K0313);
            }
        }
        match name {
            "count" => {
                if let Some(t) = ts.first() {
                    if !matches!(self.resolve(t), Ty::P(Prim::Str, _)) {
                        self.element(t);
                    }
                }
                self.prim(Prim::Num)
            }
            "first" | "second" | "last" | "nth" => self.element(&arg(0)),
            "rest" | "vec" => {
                let e = self.element(&arg(0));
                Ty::Vec(Box::new(e), self.ann())
            }
            "conj" => {
                let mut e = self.element(&arg(0));
                for t in ts.iter().skip(1) {
                    e = self.join(&e, t);
                }
                Ty::Vec(Box::new(e), self.ann())
            }
            "concat" | "flatten" => {
                let mut e = Ty::Nil;
                for t in &ts {
                    let x = if name == "flatten" { self.dyn_() } else { self.element(t) };
                    e = self.join(&e, &x);
                }
                Ty::Vec(Box::new(e), self.ann())
            }
            "list" => {
                let mut e = Ty::Nil;
                for t in &ts {
                    e = self.join(&e, t);
                }
                Ty::Vec(Box::new(e), self.ann())
            }
            "range" => {
                for (i, t) in ts.iter().enumerate() {
                    let n = self.prim(Prim::Num);
                    self.expect(t, &n, &format!("argument {} of `range`", i + 1));
                    self.sink(t, K0313);
                }
                let n = self.prim(Prim::Num);
                Ty::Vec(Box::new(n), self.ann())
            }
            "get" => {
                let t = match (self.resolve(&arg(0)), args.get(1)) {
                    // a string or number key on a resource picks an instance, as in the evaluator
                    (Ty::Res(ty, d, name, _), Some(Value::Str(_) | Value::Int(_))) => Ty::Res(ty, d, name, 0),
                    (_, Some(Value::Keyword(k))) | (_, Some(Value::Str(k))) => self.field(&arg(0), k),
                    (Ty::Vec(e, _), _) => *e,
                    (Ty::Map(v, _), _) => *v,
                    (Ty::Dyn(a), _) => {
                        let b = self.ann();
                        self.g.edge(a, b);
                        Ty::Dyn(b)
                    }
                    _ => self.dyn_(),
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
                        Ty::Rec(g, Tail::Closed, _) => f.extend(g),
                        Ty::Nil => {}
                        _ => return self.dyn_(),
                    }
                }
                Ty::Rec(f, Tail::Closed, self.ann())
            }
            "assoc" => match self.resolve(&arg(0)) {
                Ty::Rec(mut f, Tail::Closed, _) => {
                    for pair in args[1.min(args.len())..].chunks(2).zip(ts[1.min(ts.len())..].chunks(2)) {
                        match (pair.0.first(), pair.1.get(1)) {
                            (Some(Value::Keyword(k)), Some(t)) => {
                                f.insert(k.to_string(), (t.clone(), false));
                            }
                            _ => return self.dyn_(),
                        }
                    }
                    Ty::Rec(f, Tail::Closed, self.ann())
                }
                _ => self.dyn_(),
            },
            "map" | "mapcat" | "filter" if !args.is_empty() => {
                let mut elems = vec![];
                for t in ts.iter().skip(1) {
                    elems.push(self.element(t));
                }
                let r = self.apply(&arg(0), &elems, &args[0]);
                match name {
                    "map" => Ty::Vec(Box::new(r), self.ann()),
                    "mapcat" => {
                        let e = self.element(&r);
                        Ty::Vec(Box::new(e), self.ann())
                    }
                    _ => {
                        // the predicate decides which elements exist
                        self.sink(&r, K0312);
                        let e = match elems.first() {
                            Some(e) => e.clone(),
                            None => self.dyn_(),
                        };
                        Ty::Vec(Box::new(e), self.ann())
                    }
                }
            }
            "reduce" => {
                if ts.len() == 3 {
                    let e = self.element(&ts[2]);
                    let r = self.apply(&ts[0], &[ts[1].clone(), e], &args[0]);
                    self.join(&ts[1], &r)
                } else {
                    if let Some(t) = ts.last() {
                        self.element(t);
                    }
                    self.dyn_()
                }
            }
            "apply" => {
                if let Some(t) = ts.last() {
                    self.sink(t, K0313);
                }
                self.dyn_()
            }
            "keys" => {
                let s = self.prim(Prim::Str);
                Ty::Vec(Box::new(s), self.ann())
            }
            "identity" | "with-meta" => arg(0),
            "secret" => {
                let t = arg(0);
                let r = self.copy_fresh(&t);
                self.flow(&t, &r);
                let s = self.secret_src("a value marked with `secret`".into());
                if let Some(top) = self.top(&r) {
                    self.g.edge(s, top);
                }
                r
            }
            _ => self.dyn_(),
        }
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
            Value::List(items, _) if items.first().and_then(|h| h.sym_name()) == Some("module") => {
                if let Some(Value::Map(m)) = items.get(2) {
                    if let Some(Value::Str(src)) = map_get(m, &Value::kw("source")) {
                        let version = match map_get(m, &Value::kw("version")) {
                            Some(Value::Str(v)) => Some(v.to_string()),
                            _ => None,
                        };
                        let (iface, _) = it.module_interface(&src, version.as_deref());
                        self.modules.insert(src.to_string(), iface);
                    }
                }
                items.iter().for_each(|i| self.learn_ctors(it, i));
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

    /// Infer one top-level form, using the interpreter as it stands before
    /// the form is evaluated. Returns the stage errors this form introduced.
    pub fn toplevel(&mut self, it: &mut Interp, form: &Value) -> usize {
        let expanded = match it.expand_all(form, 0) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        self.learn_ctors(it, &expanded);
        let before = self.errors.len();
        self.form(&expanded);
        self.settle();
        self.errors.len() - before
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
                    self.conditionals();
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
                    self.conditionals();
                    self.define(n, &t);
                }
            }
            _ => {
                self.infer_list(items);
            }
        }
    }

    /// An `if` whose branches declare resources needs a compile-time
    /// condition. Turn each such `if` into a sink on its condition.
    fn conditionals(&mut self) {
        let declares = self.g.reach(&[self.decl]);
        for p in std::mem::take(&mut self.pending) {
            if declares.contains_key(&p.branch) {
                let s = self.g.add(Node::Sink { code: K0312.0, msg: K0312.1.into(), help: K0312.2, pos: p.pos, secret: false });
                self.g.edge(p.cond, s);
            }
        }
    }

    /// Report every sink a source reaches, once, for each lattice: stage
    /// sources (all of them) to stage sinks, secret sources to secret sinks.
    fn settle(&mut self) {
        self.conditionals();
        let stage_sources: Vec<u32> = self
            .g
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n, Node::Src { .. }))
            .map(|(i, _)| i as u32)
            .collect();
        let secret_sources: Vec<u32> = self
            .g
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n, Node::Src { secret: true, .. }))
            .map(|(i, _)| i as u32)
            .collect();
        let staged = self.g.reach(&stage_sources);
        // type mismatches stand only for values known at compile time
        for (a, e) in std::mem::take(&mut self.typed) {
            if !staged.contains_key(&a) && !self.errors.iter().any(|x| x.code == e.code && x.pos == e.pos && x.msg == e.msg) {
                self.errors.push(e);
            }
        }
        let secret = self.g.reach(&secret_sources);
        self.report(&staged, false);
        self.report(&secret, true);
    }

    fn report(&mut self, reached: &HashMap<u32, u32>, secret_lattice: bool) {
        let mut hits: Vec<(u32, u32)> = reached
            .iter()
            .filter(|(n, _)| {
                matches!(self.g.nodes[**n as usize], Node::Sink { secret, .. } if secret == secret_lattice)
                    && !self.reported.contains(n)
            })
            .map(|(n, o)| (*n, *o))
            .collect();
        let pos_of = |g: &Graph, n: u32| match &g.nodes[n as usize] {
            Node::Sink { pos, .. } => pos.map_or((u32::MAX, u32::MAX), |p| (p.line, p.col)),
            _ => (u32::MAX, u32::MAX),
        };
        hits.sort_by_key(|(sink, _)| (pos_of(&self.g, *sink), *sink));
        let mut seen_sites = HashSet::new();
        for (sink, origin) in hits {
            self.reported.insert(sink);
            // one stage error per cause, so a deferred list that is iterated does not
            // also report every name made from it; every place a secret leaks counts
            if !secret_lattice && !self.blamed.insert(origin) {
                continue;
            }
            if let (Node::Sink { code, msg, help, pos, .. }, Node::Src { desc, pos: spos, .. }) =
                (self.g.nodes[sink as usize].clone(), self.g.nodes[origin as usize].clone())
            {
                if !seen_sites.insert((code, pos.map(|p| (p.line, p.col)))) {
                    continue;
                }
                if self.errors.iter().any(|e| e.code == code && e.pos == pos) {
                    continue;
                }
                let at = spos.map(|p| format!(" (line {}, column {})", p.line, p.col)).unwrap_or_default();
                let msg = if secret_lattice {
                    format!("{}: the secret is {}{}", msg, desc, at)
                } else {
                    format!("{}: it depends on {}{}", msg, desc, at)
                };
                self.errors.push(StageError { code, msg, help: help.to_string(), pos });
            }
        }
    }

    // ------------------------------------------------------------ printing

    pub fn show(&self, t: &Ty) -> String {
        let mut names = HashMap::new();
        self.show_in(&self.zonk(t), &mut names)
    }

    fn show_in(&self, t: &Ty, names: &mut HashMap<String, String>) -> String {
        match t {
            Ty::Var(v) => {
                let n = names.len();
                names.entry(format!("t{}", v)).or_insert_with(|| ((b'a' + (n % 26) as u8) as char).to_string()).clone()
            }
            Ty::Dyn(_) => "?".into(),
            Ty::Nil => "Nil".into(),
            Ty::P(p, _) => format!("{:?}", p),
            Ty::Module => "Module".into(),
            Ty::Res(ty, d, _, i) => format!(
                "{}{}{}",
                if *d { "data " } else { "" },
                ty,
                match i {
                    1 => " (count)",
                    2 => " (for_each)",
                    _ => "",
                }
            ),
            Ty::Vec(e, _) => format!("[{}]", self.show_in(e, names)),
            Ty::Map(e, _) => format!("(Map {})", self.show_in(e, names)),
            Ty::Rec(f, tail, _) => {
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
            Ty::Fn(ps, rest, ret, _) => {
                let mut parts: Vec<String> = ps.iter().map(|p| self.show_in(p, names)).collect();
                if rest.is_some() {
                    parts.push("& ?".into());
                }
                format!("(Fn [{}] {})", parts.join(" "), self.show_in(ret, names))
            }
        }
    }
}
