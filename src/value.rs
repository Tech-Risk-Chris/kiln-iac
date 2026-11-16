//! Core data types: values (which are also the syntax), environments, errors.

use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct KilnError {
    pub code: &'static str,
    pub msg: String,
    pub pos: Option<Pos>,
    pub help: Option<String>,
}

pub type Res<T> = Result<T, KilnError>;

impl KilnError {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Self {
        KilnError { code, msg: msg.into(), pos: None, help: None }
    }
    pub fn help(mut self, h: impl Into<String>) -> Self {
        self.help = Some(h.into());
        self
    }
    pub fn at(mut self, pos: Option<Pos>) -> Self {
        if self.pos.is_none() {
            self.pos = pos;
        }
        self
    }
    /// Render in the rustc style, with the offending source line.
    pub fn render(&self, file: &str, src: &str) -> String {
        let mut s = format!("error[{}]: {}\n", self.code, self.msg);
        if let Some(p) = self.pos {
            s.push_str(&format!("  --> {}:{}:{}\n", file, p.line, p.col));
            if let Some(line) = src.lines().nth(p.line as usize - 1) {
                let n = p.line.to_string();
                let pad = " ".repeat(n.len());
                s.push_str(&format!("{} |\n{} | {}\n{} | {}^\n", pad, n, line, pad,
                    " ".repeat(p.col as usize - 1)));
            }
        } else {
            s.push_str(&format!("  --> {}\n", file));
        }
        if let Some(h) = &self.help {
            s.push_str(&format!("   = help: {}\n", h));
        }
        s
    }
}

pub fn fail<T>(code: &'static str, msg: impl Into<String>) -> Res<T> {
    Err(KilnError::new(code, msg))
}

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    Keyword(Rc<str>),
    Symbol(Rc<str>),
    List(Rc<Vec<Value>>, Option<Pos>),
    Vector(Rc<Vec<Value>>),
    /// Insertion-ordered; maps in infrastructure code are small.
    Map(Rc<Vec<(Value, Value)>>),
    Fn(Rc<Closure>),
    Macro(Rc<Closure>),
    Builtin(&'static str),
    /// Resource constructor for a Terraform type, e.g. `google_container_cluster`.
    Ctor(Rc<str>),
    /// Terraform built-in function, e.g. `tf/cidrsubnet`.
    TfFn(Rc<str>),
    /// A declared resource, data source or module.
    Handle(Rc<Handle>),
    /// A value known only after apply: a Terraform expression.
    Deferred(Rc<TfExpr>),
    /// A logical name that records what it used to be: `^{:was "old"} "new"`.
    Named(Rc<Named>),
}

pub struct Named {
    pub name: String,
    pub was: String,
}

pub struct Closure {
    pub name: Option<Rc<str>>,
    pub params: Value,
    pub body: Vec<Value>,
    pub env: Env,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleKind {
    Resource,
    Data,
    Module,
}

pub struct Handle {
    pub kind: HandleKind,
    pub ty: String,
    pub name: String,
}

impl Handle {
    pub fn address(&self) -> String {
        match self.kind {
            HandleKind::Resource => format!("{}.{}", self.ty, self.name),
            HandleKind::Data => format!("data.{}.{}", self.ty, self.name),
            HandleKind::Module => format!("module.{}", self.name),
        }
    }
}

pub enum TfExpr {
    Ref(String),
    Attr(Rc<TfExpr>, String),
    Index(Rc<TfExpr>, Value),
    Template(Vec<TplPart>),
    Call(String, Vec<Value>),
    Bin(&'static str, Value, Value),
    Not(Value),
    Cond(Value, Value, Value),
    Raw(String),
}

pub enum TplPart {
    Lit(String),
    Expr(Rc<TfExpr>),
}

// ---------------------------------------------------------------- environments

/// Persistent environment. A closure captures the chain as it stood at its
/// definition, so it cannot see itself or anything defined later: no recursion,
/// no forward references, and therefore termination by construction.
pub type Env = Option<Rc<Frame>>;

pub struct Frame {
    pub vars: Vec<(Rc<str>, Value)>,
    pub parent: Env,
}

pub fn lookup(env: &Env, name: &str) -> Option<Value> {
    let mut cur = env;
    while let Some(f) = cur {
        for (k, v) in f.vars.iter().rev() {
            if &**k == name {
                return Some(v.clone());
            }
        }
        cur = &f.parent;
    }
    None
}

pub fn extend(env: &Env, vars: Vec<(Rc<str>, Value)>) -> Env {
    Some(Rc::new(Frame { vars, parent: env.clone() }))
}

// ---------------------------------------------------------------- helpers

impl Value {
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items), None)
    }
    pub fn vector(items: Vec<Value>) -> Value {
        Value::Vector(Rc::new(items))
    }
    pub fn map(items: Vec<(Value, Value)>) -> Value {
        Value::Map(Rc::new(items))
    }
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Rc::from(s.as_ref()))
    }
    pub fn sym(s: &str) -> Value {
        Value::Symbol(Rc::from(s))
    }
    pub fn kw(s: &str) -> Value {
        Value::Keyword(Rc::from(s))
    }
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
    pub fn is_deferred(&self) -> bool {
        matches!(self, Value::Deferred(_))
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Keyword(_) => "keyword",
            Value::Symbol(_) => "symbol",
            Value::List(..) => "list",
            Value::Vector(_) => "vector",
            Value::Map(_) => "map",
            Value::Fn(_) | Value::Builtin(_) | Value::Ctor(_) | Value::TfFn(_) => "function",
            Value::Macro(_) => "macro",
            Value::Handle(_) => "resource handle",
            Value::Deferred(_) => "deferred value",
            Value::Named(_) => "string",
        }
    }
    pub fn sym_name(&self) -> Option<&str> {
        match self {
            Value::Symbol(s) => Some(s),
            _ => None,
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Nil, Nil) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a == b,
            (Int(a), Float(b)) | (Float(b), Int(a)) => (*a as f64) == *b,
            (Str(a), Str(b)) | (Keyword(a), Keyword(b)) | (Symbol(a), Symbol(b)) => a == b,
            (List(a, _), List(b, _)) | (Vector(a), Vector(b)) => a == b,
            (List(a, _), Vector(b)) | (Vector(b), List(a, _)) => a == b,
            (Map(a), Map(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(k, v)| b.iter().any(|(k2, v2)| k == k2 && v == v2))
            }
            (Handle(a), Handle(b)) => a.address() == b.address(),
            (Named(a), Named(b)) => a.name == b.name,
            (Named(a), Str(b)) | (Str(b), Named(a)) => a.name == **b,
            _ => false,
        }
    }
}

pub fn map_get(entries: &[(Value, Value)], key: &Value) -> Option<Value> {
    entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

pub fn map_assoc(entries: &mut Vec<(Value, Value)>, key: Value, val: Value) {
    if let Some(e) = entries.iter_mut().find(|(k, _)| *k == key) {
        e.1 = val;
    } else {
        entries.push((key, val));
    }
}

// ---------------------------------------------------------------- printing

pub fn quote_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
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

pub fn fmt_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

pub fn pr_str(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Str(s) => quote_str(s),
        Value::Keyword(k) => format!(":{}", k),
        Value::Symbol(s) => s.to_string(),
        Value::List(items, _) => seq_str("(", ")", items),
        Value::Vector(items) => seq_str("[", "]", items),
        Value::Map(entries) => {
            let parts: Vec<String> =
                entries.iter().map(|(k, v)| format!("{} {}", pr_str(k), pr_str(v))).collect();
            format!("{{{}}}", parts.join(" "))
        }
        Value::Fn(c) => format!("#<fn {}>", c.name.as_deref().unwrap_or("anonymous")),
        Value::Macro(c) => format!("#<macro {}>", c.name.as_deref().unwrap_or("anonymous")),
        Value::Builtin(n) => format!("#<builtin {}>", n),
        Value::Ctor(t) => format!("#<resource-constructor {}>", t),
        Value::TfFn(n) => format!("#<terraform-function {}>", n),
        Value::Handle(h) => format!("#<{}>", h.address()),
        Value::Named(n) => format!("^{{:was {}}} {}", quote_str(&n.was), quote_str(&n.name)),
        Value::Deferred(e) => match crate::tf::render_tf(e) {
            Ok(s) => format!("#<deferred {}>", s),
            Err(_) => "#<deferred ?>".into(),
        },
    }
}

fn seq_str(open: &str, close: &str, items: &[Value]) -> String {
    let parts: Vec<String> = items.iter().map(pr_str).collect();
    format!("{}{}{}", open, parts.join(" "), close)
}

/// Pretty-printer for `kiln expand`: breaks forms that do not fit on a line.
pub fn pp(v: &Value, indent: usize) -> String {
    let flat = pr_str(v);
    if flat.len() + indent <= 88 {
        return flat;
    }
    let pad = |n: usize| " ".repeat(n);
    match v {
        Value::List(items, _) if !items.is_empty() => {
            let mut s = format!("({}", pr_str(&items[0]));
            let mut rest = &items[1..];
            // keep a name or binding vector on the head line when short
            if let Some(first) = rest.first() {
                let f = pr_str(first);
                if f.len() <= 40 {
                    s.push(' ');
                    s.push_str(&f);
                    rest = &rest[1..];
                }
            }
            for it in rest {
                s.push('\n');
                s.push_str(&pad(indent + 2));
                s.push_str(&pp(it, indent + 2));
            }
            s.push(')');
            s
        }
        Value::Vector(items) => {
            let parts: Vec<String> = items.iter().map(|i| pp(i, indent + 1)).collect();
            format!("[{}]", parts.join(&format!("\n{}", pad(indent + 1))))
        }
        Value::Map(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    let ks = pr_str(k);
                    format!("{} {}", ks, pp(v, indent + 2 + ks.len()))
                })
                .collect();
            format!("{{{}}}", parts.join(&format!("\n{}", pad(indent + 1))))
        }
        _ => flat,
    }
}
