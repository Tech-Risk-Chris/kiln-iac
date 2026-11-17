//! Evaluator and macro expander.
//!
//! The language is pure and strict. There is no recursion and there are no
//! forward references (see `Env`), so every program terminates; the only
//! unbounded process is macro expansion, which is capped.

use crate::builtins;
use crate::check;
use crate::reader;
use crate::recursion;
use crate::schema::{self, SchemaSet};
use crate::tf;
use crate::value::*;
use serde_json::{Map as JMap, Value as J};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

const PRELUDE: &str = include_str!("prelude.kiln");
const MAX_DEPTH: u32 = 2000;
const MAX_EXPANSIONS: u32 = 500;

pub struct ProviderReq {
    pub source: String,
    pub version: Option<String>,
    pub prefix: String,
}

#[derive(Default)]
pub struct Graph {
    pub resources: BTreeMap<String, BTreeMap<String, J>>,
    pub data: BTreeMap<String, BTreeMap<String, J>>,
    pub modules: BTreeMap<String, J>,
    pub outputs: BTreeMap<String, J>,
    pub provider_cfg: BTreeMap<String, Vec<J>>,
    pub backend: Option<(String, J)>,
    /// Declarations whose name carries `:was`: the address now, and the
    /// addresses it may have had before.
    pub renames: Vec<(String, Vec<String>)>,
    /// Number of declarations so far; used to detect declarations made
    /// under a deferred condition.
    pub count: usize,
}

enum Frame {
    Count(Value),
    Each(u32, Value),
}

pub struct Interp {
    pub schemas: Option<Rc<SchemaSet>>,
    /// Problems found by the schema checks; reported together after evaluation.
    pub diags: Vec<KilnError>,
    pub warnings: Vec<KilnError>,
    /// Providers that have no cached schema; their declarations are not checked.
    pub unchecked: std::collections::BTreeSet<String>,
    /// Modules that have no cached interface.
    pub unchecked_modules: std::collections::BTreeSet<String>,
    /// The directory of the stack's source file, for local module paths.
    pub stack_dir: std::path::PathBuf,
    /// Where the output goes; None: `out/` in the stack directory.
    pub out_dir: Option<std::path::PathBuf>,
    /// Write local module sources relative to the output; off when only the
    /// sources as written are wanted (`kiln schema`).
    pub rebase_sources: bool,
    version_checked: std::collections::HashSet<String>,
    pos_stack: Vec<Pos>,
    /// Active calls of recursive functions: (function, size of its shrinking argument).
    rec_stack: Vec<(*const Closure, usize)>,
    /// Enclosing `count-deferred` and `for-each-deferred` forms.
    inst_stack: Vec<Frame>,
    next_each: u32,
    pub global: Env,
    pub providers: BTreeMap<String, ProviderReq>,
    aliases: HashMap<String, String>,
    scope: Vec<(String, Option<String>)>,
    pub graph: Graph,
    gensym: u64,
    depth: u32,
}

fn valid_name(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn get_value(v: &Value, key: &Value) -> Res<Value> {
    Ok(match v {
        // Both spell the same Terraform key, so either finds the other.
        Value::Map(e) => map_get(e, key)
            .or_else(|| match key {
                Value::Str(s) => map_get(e, &Value::Keyword(s.clone())),
                Value::Keyword(s) => map_get(e, &Value::Str(s.clone())),
                _ => None,
            })
            .unwrap_or(Value::Nil),
        Value::Nil => Value::Nil,
        Value::Vector(items) | Value::List(items, _) => match key {
            Value::Int(i) if *i >= 0 => items.get(*i as usize).cloned().unwrap_or(Value::Nil),
            _ => Value::Nil,
        },
        Value::Handle(h) => match key {
            Value::Keyword(k) => {
                let base = Rc::new(match h.inst {
                    Inst::One => TfExpr::Ref(h.address()),
                    Inst::Count => TfExpr::Instance(h.address(), InstMode::Count),
                    Inst::Each(id) if each_active(id) => TfExpr::Instance(h.address(), InstMode::EachKey),
                    Inst::Each(_) => TfExpr::Instance(h.address(), InstMode::All),
                });
                match &h.schema {
                    Some(block) => {
                        let what = match h.kind {
                            HandleKind::Module => format!("`{}`", h.address()),
                            _ => h.ty.to_string(),
                        };
                        read_shaped(base, &Shape::Block(block.clone()), k, &what)?
                    }
                    None => Value::Deferred(Rc::new(TfExpr::Attr(base, k.to_string()))),
                }
            }
            // instances of a resource declared with :count or :for_each
            Value::Int(_) | Value::Str(_) => Value::Deferred(Rc::new(TfExpr::Index(
                Rc::new(TfExpr::Ref(h.address())),
                key.clone(),
            ))),
            other => {
                return fail(
                    "K0103",
                    format!("a resource attribute is named by a keyword, not a {}", other.type_name()),
                )
            }
        },
        Value::Deferred(e) => match (key, &**e) {
            (Value::Keyword(k), TfExpr::Shaped(_, shape)) => {
                let what = format!("`{}`", tf::render_tf(e).unwrap_or_else(|_| "the block".into()));
                read_shaped(e.clone(), shape, k, &what)?
            }
            (Value::Int(_) | Value::Str(_) | Value::Deferred(_), TfExpr::Shaped(_, Shape::Blocks(block, _))) => {
                let x = Rc::new(TfExpr::Index(e.clone(), key.clone()));
                Value::Deferred(Rc::new(TfExpr::Shaped(x, Shape::Block(block.clone()))))
            }
            (Value::Keyword(k), _) => Value::Deferred(Rc::new(TfExpr::Attr(e.clone(), k.to_string()))),
            (Value::Int(_) | Value::Str(_) | Value::Deferred(_), _) => {
                Value::Deferred(Rc::new(TfExpr::Index(e.clone(), key.clone())))
            }
            (other, _) => {
                return fail(
                    "K0103",
                    format!("cannot index a deferred value with a {}", other.type_name()),
                )
            }
        },
        other => return fail("K0103", format!("cannot look up a key in a {}", other.type_name())),
    })
}

/// Read `key` from something whose schema is known: check that it exists,
/// mark it secret if the schema says it is sensitive, and keep the schema of
/// nested blocks so that reads further down are checked too.
fn read_shaped(base: Rc<TfExpr>, shape: &Shape, key: &str, what: &str) -> Res<Value> {
    let block = match shape {
        Shape::Block(b) => b,
        Shape::Blocks(_, mode) => {
            let help = if mode == "set" {
                "it is a set of blocks: take one with (tf/one x), or iterate in Terraform"
            } else {
                "it is a list of blocks: index it first, e.g. (get x 0)"
            };
            return Err(KilnError::new("K0708", format!("cannot read `{}` from a collection of blocks", key)).help(help));
        }
    };
    let attr_expr = Rc::new(TfExpr::Attr(base, key.to_string()));
    if let Some(a) = block["attributes"].get(key) {
        let mut e = attr_expr;
        if a["sensitive"] == J::Bool(true) {
            e = Rc::new(TfExpr::Secret(e));
        }
        if let Some(nt) = a.get("nested_type") {
            let inner = Rc::new(nt.clone());
            let s = match nt["nesting_mode"].as_str().unwrap_or("single") {
                "single" => Shape::Block(inner),
                m => Shape::Blocks(inner, m.to_string()),
            };
            e = Rc::new(TfExpr::Shaped(e, s));
        }
        return Ok(Value::Deferred(e));
    }
    if let Some(bt) = block["block_types"].get(key) {
        let inner = Rc::new(bt["block"].clone());
        let s = match bt["nesting_mode"].as_str().unwrap_or("list") {
            "single" | "group" => Shape::Block(inner),
            m => Shape::Blocks(inner, m.to_string()),
        };
        return Ok(Value::Deferred(Rc::new(TfExpr::Shaped(attr_expr, s))));
    }
    let mut e = KilnError::new("K0707", format!("{} has no attribute `{}`", what, key));
    if let Some(s) = check::suggest(key, check::readable(block)) {
        e = e.help(format!("did you mean `{}`?", s));
    }
    Err(e)
}

/// The elements of a compile-time collection. A deferred collection has no
/// elements yet, which is the point of the error.
pub fn seq_of(v: &Value) -> Res<Vec<Value>> {
    match v {
        Value::Nil => Ok(vec![]),
        Value::Vector(items) | Value::List(items, _) => Ok(items.to_vec()),
        Value::Map(entries) => Ok(entries
            .iter()
            .map(|(k, v)| Value::vector(vec![k.clone(), v.clone()]))
            .collect()),
        Value::Deferred(_) => Err(KilnError::new(
            "K0313",
            "cannot iterate over a collection known only after apply",
        )
        .help("iterate over compile-time data, or pass a :for_each meta-argument explicitly")),
        other => fail("K0103", format!("a {} is not a collection", other.type_name())),
    }
}

fn deferred_condition(c: &Value) -> KilnError {
    KilnError::new(
        "K0312",
        format!(
            "resource existence depends on a value known only after apply: {}",
            tf::render_expr(c).unwrap_or_else(|_| "?".into())
        ),
    )
    .help(
        "branch on a compile-time value, or declare the resource unconditionally \
         with an explicit :count meta-argument",
    )
}

impl Graph {
    /// Every address that holds state: resources and modules, not data sources.
    pub fn addresses(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for (ty, items) in &self.resources {
            for name in items.keys() {
                out.insert(format!("{}.{}", ty, name));
            }
        }
        for name in self.modules.keys() {
            out.insert(format!("module.{}", name));
        }
        out
    }

    pub fn body(&self, address: &str) -> Option<&J> {
        if let Some(name) = address.strip_prefix("module.") {
            return self.modules.get(name);
        }
        let (ty, name) = address.split_once('.')?;
        self.resources.get(ty)?.get(name)
    }
}

impl Interp {
    pub fn new() -> Res<Interp> {
        let vars = builtins::NAMES.iter().map(|n| (Rc::from(*n), Value::Builtin(n))).collect();
        let mut it = Interp {
            schemas: None,
            diags: vec![],
            warnings: vec![],
            unchecked: Default::default(),
            unchecked_modules: Default::default(),
            stack_dir: std::path::PathBuf::from("."),
            out_dir: None,
            rebase_sources: true,
            version_checked: Default::default(),
            pos_stack: vec![],
            rec_stack: vec![],
            inst_stack: vec![],
            next_each: 0,
            global: extend(&None, vars),
            providers: BTreeMap::new(),
            aliases: HashMap::new(),
            scope: vec![],
            graph: Graph::default(),
            gensym: 0,
            depth: 0,
        };
        for f in reader::read_all(PRELUDE)? {
            it.eval_toplevel(&f)?;
        }
        Ok(it)
    }

    // ------------------------------------------------------------ top level

    pub fn eval_toplevel(&mut self, form: &Value) -> Res<Value> {
        let pos = if let Value::List(_, p) = form { *p } else { None };
        self.toplevel_inner(form, 0).map_err(|e| e.at(pos))
    }

    fn toplevel_inner(&mut self, form: &Value, _n: u32) -> Res<Value> {
        // Expand macros at the head iteratively, so a runaway macro hits the
        // limit instead of the stack.
        let mut form = form.clone();
        let mut n = 0;
        loop {
            let ex = match &form {
                Value::List(items, _) => match items.first() {
                    Some(Value::Symbol(s)) => match lookup(&self.global, s) {
                        Some(Value::Macro(c)) => Some(self.apply_closure(&c, items[1..].to_vec())?),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            };
            match ex {
                Some(e) => {
                    n += 1;
                    if n > MAX_EXPANSIONS {
                        return fail("K0400", "macro expansion did not terminate");
                    }
                    form = e;
                }
                None => break,
            }
        }
        let form = &form;
        if let Value::List(items, _) = form {
            if let Some(Value::Symbol(s)) = items.first() {
                let bound = lookup(&self.global, s);
                if bound.is_none() {
                    match &**s {
                        "def" => {
                            if items.len() != 3 {
                                return fail("K0102", "def takes a name and a value: (def name value)");
                            }
                            let name = self.binding_name(&items[1], "def")?;
                            let env = self.global.clone();
                            let v = self.eval(&items[2], &env)?;
                            self.define(name, v);
                            return Ok(Value::Nil);
                        }
                        "defn" | "defmacro" => {
                            let mut c = self.make_named_closure(items, s)?;
                            if &**s == "defn" {
                                let mut body = vec![];
                                for f in &c.body {
                                    body.push(self.expand_all(f, 0)?);
                                }
                                let name = c.name.clone().unwrap();
                                c.decreasing = recursion::decreasing_param(&name, &c.params, &body)?;
                            }
                            let name = c.name.clone().unwrap();
                            let v = if &**s == "defn" {
                                Value::Fn(Rc::new(c))
                            } else {
                                Value::Macro(Rc::new(c))
                            };
                            self.define(name, v);
                            return Ok(Value::Nil);
                        }
                        "ns" => {
                            self.ns(items)?;
                            return Ok(Value::Nil);
                        }
                        "do" => {
                            let mut last = Value::Nil;
                            for f in &items[1..] {
                                last = self.eval_toplevel(f)?;
                            }
                            return Ok(last);
                        }
                        _ => {}
                    }
                }
            }
        }
        let env = self.global.clone();
        self.eval(form, &env)
    }

    fn define(&mut self, name: Rc<str>, v: Value) {
        self.global = extend(&self.global, vec![(name, v)]);
    }

    fn binding_name(&self, v: &Value, what: &str) -> Res<Rc<str>> {
        match v {
            Value::Symbol(s) => Ok(s.clone()),
            other => fail("K0103", format!("{} needs a symbol as its name, got a {}", what, other.type_name())),
        }
    }

    fn make_named_closure(&mut self, items: &[Value], what: &str) -> Res<Closure> {
        if items.len() < 3 {
            return fail("K0102", format!("({} name [params] body...)", what));
        }
        let name = self.binding_name(&items[1], what)?;
        let mut i = 2;
        if matches!(items[i], Value::Str(_)) && items.len() > 3 {
            i += 1; // docstring
        }
        if !matches!(items[i], Value::Vector(_)) {
            return fail("K0103", format!("{} `{}` needs a parameter vector", what, name));
        }
        Ok(Closure {
            name: Some(name),
            params: items[i].clone(),
            body: items[i + 1..].to_vec(),
            env: self.global.clone(),
            decreasing: None,
        })
    }

    fn ns(&mut self, items: &[Value]) -> Res<()> {
        for clause in items.iter().skip(2) {
            let parts = match clause {
                Value::List(p, _) => p,
                _ => return fail("K0103", "ns clauses are lists, e.g. (:provider [google :as g])"),
            };
            match parts.first() {
                Some(Value::Keyword(k)) if &**k == "provider" => {
                    for spec in &parts[1..] {
                        self.provider_spec(spec)?;
                    }
                }
                _ => return fail("K0103", "unknown ns clause; this version supports (:provider ...)"),
            }
        }
        Ok(())
    }

    fn provider_spec(&mut self, spec: &Value) -> Res<()> {
        let v = match spec {
            Value::Vector(v) if !v.is_empty() => v,
            _ => return fail("K0103", "a provider spec is a vector: [google :as g :version \"~> 6.0\"]"),
        };
        let name = self.binding_name(&v[0], "a provider spec")?.to_string();
        let mut req = ProviderReq {
            source: format!("hashicorp/{}", name),
            version: None,
            prefix: name.clone(),
        };
        let mut alias = name.clone();
        let mut i = 1;
        while i + 1 < v.len() {
            let key = match &v[i] {
                Value::Keyword(k) => k.to_string(),
                _ => return fail("K0103", "provider options are keyword/value pairs"),
            };
            match (key.as_str(), &v[i + 1]) {
                ("as", Value::Symbol(s)) => alias = s.to_string(),
                ("version", Value::Str(s)) => req.version = Some(s.to_string()),
                ("source", Value::Str(s)) => req.source = s.to_string(),
                ("prefix", Value::Str(s)) => req.prefix = s.to_string(),
                _ => return fail("K0103", format!("invalid provider option :{}", key)),
            }
            i += 2;
        }
        if i != v.len() {
            return fail("K0103", "provider options are keyword/value pairs");
        }
        self.aliases.insert(alias, name.clone());
        self.providers.insert(name, req);
        Ok(())
    }

    // ------------------------------------------------------------ eval

    pub fn eval(&mut self, form: &Value, env: &Env) -> Res<Value> {
        match form {
            Value::Symbol(s) => self.resolve(s, env),
            Value::Vector(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items.iter() {
                    out.push(self.eval(it, env)?);
                }
                Ok(Value::vector(out))
            }
            Value::Map(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries.iter() {
                    out.push((self.eval(k, env)?, self.eval(v, env)?));
                }
                Ok(Value::map(out))
            }
            Value::List(items, pos) => {
                if items.is_empty() {
                    return Ok(form.clone());
                }
                self.depth += 1;
                if let Some(p) = pos {
                    self.pos_stack.push(*p);
                }
                let r = if self.depth > MAX_DEPTH {
                    fail("K0400", "evaluation nested too deeply; a macro probably expands into itself")
                } else {
                    self.eval_list(items, env)
                };
                if pos.is_some() {
                    self.pos_stack.pop();
                }
                self.depth -= 1;
                r.map_err(|e| e.at(*pos))
            }
            _ => Ok(form.clone()),
        }
    }

    fn resolve(&mut self, s: &Rc<str>, env: &Env) -> Res<Value> {
        if let Some(v) = lookup(env, s) {
            return Ok(v);
        }
        if let Some((ns, name)) = s.split_once('/') {
            if ns == "tf" && !name.is_empty() {
                return Ok(Value::TfFn(Rc::from(name)));
            }
            if let Some(p) = self.aliases.get(ns) {
                let prefix = &self.providers[p].prefix;
                return Ok(Value::Ctor(Rc::from(format!("{}_{}", prefix, name))));
            }
            return Err(KilnError::new("K0100", format!("unknown namespace in `{}`", s))
                .help("declare the provider in ns: (ns my.stack (:provider [google :as g]))"));
        }
        Err(KilnError::new("K0100", format!("unbound symbol `{}`", s)).help(
            "a name is visible only after its definition; Kiln has no forward references and no recursion",
        ))
    }

    fn body(&mut self, forms: &[Value], env: &Env) -> Res<Value> {
        let mut last = Value::Nil;
        for f in forms {
            last = self.eval(f, env)?;
        }
        Ok(last)
    }

    fn eval_list(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if let Value::Symbol(s) = &items[0] {
            if lookup(env, s).is_none() {
                if let Some(r) = self.special(s, items, env) {
                    return r;
                }
            }
        }
        let f = self.eval(&items[0], env)?;
        if let Value::Macro(c) = &f {
            let ex = self.apply_closure(c, items[1..].to_vec())?;
            return self.eval(&ex, env);
        }
        let mut args = Vec::with_capacity(items.len() - 1);
        for a in &items[1..] {
            args.push(self.eval(a, env)?);
        }
        self.apply(&f, args)
    }

    fn special(&mut self, name: &str, items: &[Value], env: &Env) -> Option<Res<Value>> {
        Some(match name {
            "quote" => match items.len() {
                2 => Ok(items[1].clone()),
                _ => fail("K0102", "(quote form)"),
            },
            "quasiquote" => match items.len() {
                2 => {
                    let mut syms = HashMap::new();
                    self.quasi(&items[1], env, &mut syms)
                }
                _ => fail("K0102", "(quasiquote form)"),
            },
            "unquote" | "unquote-splicing" => fail("K0103", "unquote outside a quasiquote"),
            "if" => self.sf_if(items, env),
            "do" => self.body(&items[1..], env),
            "let" => self.sf_let(items, env),
            "fn" => {
                if items.len() < 2 || !matches!(items[1], Value::Vector(_)) {
                    return Some(fail("K0102", "(fn [params] body...)"));
                }
                Ok(Value::Fn(Rc::new(Closure {
                    name: None,
                    params: items[1].clone(),
                    body: items[2..].to_vec(),
                    env: env.clone(),
                    decreasing: None,
                })))
            }
            "for" => self.sf_for(items, env),
            "and" | "or" => self.sf_logic(name == "and", items, env),
            "scope" => self.sf_scope(items, env),
            "count-deferred" => self.sf_count_deferred(items, env),
            "for-each-deferred" => self.sf_for_each_deferred(items, env),
            "data" => self.sf_data(items, env),
            "provider" => self.sf_provider(items, env),
            "output" => self.sf_output(items, env),
            "backend" => self.sf_backend(items, env),
            "module" => self.sf_module(items, env),
            "def" | "defn" | "defmacro" | "ns" => Err(KilnError::new(
                "K0104",
                format!("`{}` is allowed only at the top level of a file", name),
            )
            .help("use let for local bindings and fn for local functions")),
            _ => return None,
        })
    }

    fn sf_if(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 3 || items.len() > 4 {
            return fail("K0102", "(if test then else?)");
        }
        let c = self.eval(&items[1], env)?;
        if c.is_deferred() {
            // Both branches become a Terraform conditional. That is sound only
            // if neither branch declares anything.
            let before = self.graph.count;
            let a = self.eval(&items[2], env)?;
            let b = match items.get(3) {
                Some(f) => self.eval(f, env)?,
                None => Value::Nil,
            };
            if self.graph.count != before {
                return Err(deferred_condition(&c));
            }
            return Ok(Value::Deferred(Rc::new(TfExpr::Cond(c, a, b))));
        }
        if c.truthy() {
            self.eval(&items[2], env)
        } else {
            match items.get(3) {
                Some(f) => self.eval(f, env),
                None => Ok(Value::Nil),
            }
        }
    }

    fn sf_let(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        let binds = match items.get(1) {
            Some(Value::Vector(b)) if b.len() % 2 == 0 => b.clone(),
            _ => return fail("K0102", "(let [pattern value ...] body...)"),
        };
        let mut env = env.clone();
        for pair in binds.chunks(2) {
            let v = self.eval(&pair[1], &env)?;
            let mut out = vec![];
            self.bind(&pair[0], v, &env, &mut out)?;
            env = extend(&env, out);
        }
        self.body(&items[2..], &env)
    }

    fn sf_for(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        let binds = match items.get(1) {
            Some(Value::Vector(b)) if b.len() % 2 == 0 && !b.is_empty() => b.clone(),
            _ => return fail("K0102", "(for [pattern collection ...] body...)"),
        };
        let mut out = vec![];
        self.for_rec(&binds, 0, env, &items[2..], &mut out)?;
        Ok(Value::vector(out))
    }

    fn for_rec(&mut self, binds: &[Value], i: usize, env: &Env, body: &[Value], out: &mut Vec<Value>) -> Res<()> {
        if i >= binds.len() {
            out.push(self.body(body, env)?);
            return Ok(());
        }
        match &binds[i] {
            Value::Keyword(k) if &**k == "when" => {
                let c = self.eval(&binds[i + 1], env)?;
                if c.is_deferred() {
                    return Err(deferred_condition(&c));
                }
                if c.truthy() {
                    self.for_rec(binds, i + 2, env, body, out)?;
                }
                Ok(())
            }
            Value::Keyword(k) if &**k == "let" => {
                let lb = match &binds[i + 1] {
                    Value::Vector(b) if b.len() % 2 == 0 => b.clone(),
                    _ => return fail("K0102", ":let takes a binding vector"),
                };
                let mut env2 = env.clone();
                for pair in lb.chunks(2) {
                    let v = self.eval(&pair[1], &env2)?;
                    let mut vars = vec![];
                    self.bind(&pair[0], v, &env2, &mut vars)?;
                    env2 = extend(&env2, vars);
                }
                self.for_rec(binds, i + 2, &env2, body, out)
            }
            pat => {
                let coll = self.eval(&binds[i + 1], env)?;
                for item in seq_of(&coll)? {
                    let mut vars = vec![];
                    self.bind(pat, item, env, &mut vars)?;
                    let env2 = extend(env, vars);
                    self.for_rec(binds, i + 2, &env2, body, out)?;
                }
                Ok(())
            }
        }
    }

    fn sf_logic(&mut self, is_and: bool, items: &[Value], env: &Env) -> Res<Value> {
        let mut last = Value::Bool(is_and);
        for f in &items[1..] {
            last = self.eval(f, env)?;
            if last.is_deferred() {
                return Err(KilnError::new(
                    "K0315",
                    "and/or cannot short-circuit on a value known only after apply",
                )
                .help("write the condition as a Terraform expression: (tf/expr \"a && b\")"));
            }
            if last.truthy() != is_and {
                return Ok(last);
            }
        }
        Ok(last)
    }

    fn sf_scope(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 2 {
            return fail("K0102", "(scope name body...)");
        }
        let n = self.eval(&items[1], env)?;
        let seg = self.logical_name(&n, "a component instance name")?;
        self.scope.push(seg);
        let r = self.body(&items[2..], env);
        self.scope.pop();
        r
    }

    fn sf_count_deferred(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 3 {
            return fail("K0102", "(count-deferred condition body...)");
        }
        if self.inst_stack.iter().any(|f| matches!(f, Frame::Each(..))) {
            return Err(KilnError::new("K0318", "count-deferred inside for-each-deferred")
                .help("Terraform allows count or for_each on a resource, not both"));
        }
        let cond = self.eval(&items[1], env)?;
        self.inst_stack.push(Frame::Count(cond));
        let r = self.body(&items[2..], env);
        self.inst_stack.pop();
        r
    }

    fn sf_for_each_deferred(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        let (pat, coll) = match items.get(1) {
            Some(Value::Vector(b)) if b.len() == 2 => (b[0].clone(), b[1].clone()),
            _ => return fail("K0102", "(for-each-deferred [x collection] body...)"),
        };
        if let Some(f) = self.inst_stack.first() {
            let msg = match f {
                Frame::Each(..) => "for-each-deferred inside for-each-deferred",
                Frame::Count(_) => "for-each-deferred inside count-deferred",
            };
            return Err(KilnError::new("K0318", msg).help(
                "a resource has at most one for_each and cannot have count as well; \
                 build the combined collection with a Terraform expression instead",
            ));
        }
        let c = self.eval(&coll, env)?;
        let each = |k: &str| Value::Deferred(Rc::new(TfExpr::Raw(format!("each.{}", k))));
        let vars: Vec<(Rc<str>, Value)> = match &pat {
            Value::Symbol(x) => vec![(x.clone(), each("value"))],
            Value::Vector(kv) if kv.len() == 2 => match (&kv[0], &kv[1]) {
                (Value::Symbol(k), Value::Symbol(v)) => vec![(k.clone(), each("key")), (v.clone(), each("value"))],
                _ => return fail("K0103", "for-each-deferred binds a name, or [key value]"),
            },
            _ => return fail("K0103", "for-each-deferred binds a name, or [key value]"),
        };
        let id = self.next_each;
        self.next_each += 1;
        let env2 = extend(env, vars);
        self.inst_stack.push(Frame::Each(id, c));
        each_enter(id);
        let r = self.body(&items[2..], &env2);
        each_leave();
        self.inst_stack.pop();
        r
    }

    /// Add the `count` or `for_each` that enclosing forms ask for.
    fn instancing(&mut self, body: &mut J, what: &str) -> Res<Inst> {
        if self.inst_stack.is_empty() {
            return Ok(Inst::One);
        }
        let obj = match body {
            J::Object(o) => o,
            _ => return Ok(Inst::One),
        };
        for k in ["count", "for_each"] {
            if obj.contains_key(k) {
                return Err(KilnError::new(
                    "K0317",
                    format!("{} sets :{} itself inside count-deferred or for-each-deferred", what, k),
                )
                .help("let the enclosing form decide, or move the declaration out of it"));
            }
        }
        let mut conds = vec![];
        let mut inst = Inst::One;
        let mut meta = vec![];
        for f in &self.inst_stack {
            match f {
                Frame::Count(c) => {
                    conds.push(c.clone());
                    inst = Inst::Count;
                }
                Frame::Each(id, coll) => {
                    let v = match coll {
                        Value::Vector(_) | Value::List(..) => {
                            Value::Deferred(Rc::new(TfExpr::Call("toset".into(), vec![coll.clone()])))
                        }
                        Value::Map(_) | Value::Deferred(_) => coll.clone(),
                        other => {
                            return fail(
                                "K0103",
                                format!("for-each-deferred needs a map or a collection, got a {}", other.type_name()),
                            )
                        }
                    };
                    meta.push(("for_each", v));
                    inst = Inst::Each(*id);
                }
            }
        }
        if !conds.is_empty() {
            let v = if conds.iter().all(|c| !c.is_deferred()) {
                Value::Int(if conds.iter().all(|c| c.truthy()) { 1 } else { 0 })
            } else {
                let mut test = conds.pop().unwrap();
                while let Some(c) = conds.pop() {
                    test = Value::Deferred(Rc::new(TfExpr::Bin("&&", c, test)));
                }
                Value::Deferred(Rc::new(TfExpr::Cond(test, Value::Int(1), Value::Int(0))))
            };
            meta.push(("count", v));
        }
        for (k, v) in meta {
            if let Some(src) = secret_source(&v) {
                let e = KilnError::new(
                    "K0320",
                    format!("{}: `{}` depends on the secret `{}`; Terraform rejects sensitive values here", what, k, src),
                )
                .at(self.current_pos());
                self.diags.push(e);
            }
            obj.insert(k.to_string(), tf::to_json(&v)?);
        }
        Ok(inst)
    }

    fn sf_data(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 3 {
            return fail("K0102", "(data provider/type \"name\" {args})");
        }
        let ty = match self.eval(&items[1], env)? {
            Value::Ctor(t) => t,
            _ => return fail("K0103", "data needs a provider type, e.g. (data g/client_config \"me\" {})"),
        };
        let mut args = vec![];
        for a in &items[2..] {
            args.push(self.eval(a, env)?);
        }
        self.declare(HandleKind::Data, &ty, &args)
    }

    fn sf_provider(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() != 3 {
            return fail("K0102", "(provider alias {config})");
        }
        let alias = self.binding_name(&items[1], "provider")?;
        let name = match self.aliases.get(&*alias) {
            Some(n) => n.clone(),
            None => {
                return Err(KilnError::new("K0100", format!("unknown provider `{}`", alias))
                    .help("declare it in ns: (ns my.stack (:provider [google :as g]))"))
            }
        };
        let cfg = self.eval(&items[2], env)?;
        if !matches!(cfg, Value::Map(_)) {
            return fail("K0103", "provider configuration must be a map");
        }
        let j = tf::to_json(&cfg)?;
        if let Some(ps) = self.provider_schema(&name) {
            let mut issues = check::check_body(check::Kind::Provider, &ps.provider, &j);
            issues.extend(check::check_secrets(check::Kind::Provider, &ps.provider, &cfg));
            self.report(&format!("provider {}", name), issues);
        }
        self.graph.provider_cfg.entry(name).or_default().push(j);
        Ok(Value::Nil)
    }

    fn sf_output(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 3 || items.len() > 4 {
            return fail("K0102", "(output \"name\" value {options}?)");
        }
        let n = self.eval(&items[1], env)?;
        let name = self.addr_names(&self.logical_name(&n, "an output name")?).0;
        let v = self.eval(&items[2], env)?;
        let mut m = JMap::new();
        if let Some(o) = items.get(3) {
            match tf::to_json(&self.eval(o, env)?)? {
                J::Object(o) => m = o,
                _ => return fail("K0103", "output options must be a map"),
            }
        }
        if let Some(src) = secret_source(&v) {
            if m.get("sensitive") != Some(&J::Bool(true)) {
                let e = KilnError::new(
                    "K0321",
                    format!("output `{}` exposes the secret `{}`", name, src),
                )
                .help("mark the output sensitive: (output \"name\" value {:sensitive true})")
                .at(self.current_pos());
                self.diags.push(e);
            }
        }
        m.insert("value".into(), tf::to_json(&v)?);
        if self.graph.outputs.insert(name.clone(), J::Object(m)).is_some() {
            return fail("K0202", format!("output `{}` is declared twice", name));
        }
        Ok(Value::Nil)
    }

    fn sf_backend(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() != 3 {
            return fail("K0102", "(backend \"type\" {config})");
        }
        let ty = match self.eval(&items[1], env)? {
            Value::Str(s) => s.to_string(),
            _ => return fail("K0103", "backend type must be a string"),
        };
        let cfg = self.eval(&items[2], env)?;
        if !matches!(cfg, Value::Map(_)) {
            return fail("K0103", "backend configuration must be a map");
        }
        if self.graph.backend.is_some() {
            return fail("K0202", "a stack has exactly one backend");
        }
        self.graph.backend = Some((ty, tf::to_json(&cfg)?));
        Ok(Value::Nil)
    }

    fn sf_module(&mut self, items: &[Value], env: &Env) -> Res<Value> {
        if items.len() < 3 || items.len() > 4 {
            return fail("K0102", "(module \"name\" {:source ... :version ...} {inputs}?)");
        }
        let n = self.eval(&items[1], env)?;
        let (name, olds) = self.addr_names(&self.logical_name(&n, "a module name")?);
        let mut m = JMap::new();
        for f in &items[2..] {
            match tf::to_json(&self.eval(f, env)?)? {
                J::Object(o) => m.extend(o),
                _ => return fail("K0103", "module source and inputs must be maps"),
            }
        }
        if !m.contains_key("source") {
            return fail("K0103", "a module needs a :source");
        }
        let mut body = J::Object(m);
        let inst = self.instancing(&mut body, &format!("module.{}", name))?;
        let m = match body {
            J::Object(o) => o,
            _ => unreachable!(),
        };
        // check the inputs against the module's variables
        let source = m.get("source").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let version = m.get("version").and_then(|s| s.as_str()).map(String::from);
        let mut outputs = None;
        if self.schemas.is_some() {
            match self.module_interface(&source, version.as_deref()) {
                (Some(iface), _) => {
                    let issues = check::check_body(check::Kind::Module, &iface.inputs, &J::Object(m.clone()));
                    self.report(&format!("module.{}", name), issues);
                    outputs = Some(iface.outputs.clone());
                }
                (None, Some(mismatch)) => {
                    let e = KilnError::new("K0715", mismatch)
                        .help("refresh it with `kiln schema <file.kiln>`")
                        .at(self.current_pos());
                    self.diags.push(e);
                }
                (None, None) => {
                    self.unchecked_modules.insert(source.clone());
                }
            }
        }
        // Kiln resolves a local source from the stack; Terraform from the output directory.
        let mut m = m;
        if self.rebase_sources && (source.starts_with("./") || source.starts_with("../")) {
            let rebased = schema::rebase_source(&source, &self.stack_dir, self.out_dir.as_deref());
            m.insert("source".into(), J::String(rebased));
        }
        let h = Handle { kind: HandleKind::Module, ty: "module".into(), name: name.clone(), schema: outputs, inst };
        if self.graph.modules.insert(name, J::Object(m)).is_some() {
            return fail("K0202", format!("`{}` is declared twice", h.address()));
        }
        if !olds.is_empty() {
            let olds = olds.iter().map(|o| format!("module.{}", o)).collect();
            self.graph.renames.push((h.address(), olds));
        }
        self.graph.count += 1;
        Ok(Value::Handle(Rc::new(h)))
    }

    // ------------------------------------------------------------ quasiquote

    fn quasi(&mut self, form: &Value, env: &Env, syms: &mut HashMap<String, Value>) -> Res<Value> {
        match form {
            Value::List(items, pos) => {
                if let Some(h) = items.first().and_then(|v| v.sym_name()) {
                    if h == "unquote" {
                        return match items.len() {
                            2 => self.eval(&items[1], env),
                            _ => fail("K0102", "(unquote form)"),
                        };
                    }
                    if h == "unquote-splicing" {
                        return fail("K0103", "~@ must appear inside a list or vector");
                    }
                }
                // Template lists carry no position: an error inside an expansion
                // is reported at the macro call, which is in the user's file.
                // Forms spliced in from the call keep their own positions.
                let _ = pos;
                Ok(Value::List(Rc::new(self.quasi_seq(items, env, syms)?), None))
            }
            Value::Vector(items) => Ok(Value::vector(self.quasi_seq(items, env, syms)?)),
            Value::Map(entries) => {
                let mut out = vec![];
                for (k, v) in entries.iter() {
                    out.push((self.quasi(k, env, syms)?, self.quasi(v, env, syms)?));
                }
                Ok(Value::map(out))
            }
            Value::Symbol(s) if s.len() > 1 && s.ends_with('#') => {
                if let Some(v) = syms.get(&**s) {
                    return Ok(v.clone());
                }
                self.gensym += 1;
                let g = Value::sym(&format!("{}__{}__auto", &s[..s.len() - 1], self.gensym));
                syms.insert(s.to_string(), g.clone());
                Ok(g)
            }
            _ => Ok(form.clone()),
        }
    }

    fn quasi_seq(&mut self, items: &[Value], env: &Env, syms: &mut HashMap<String, Value>) -> Res<Vec<Value>> {
        let mut out = vec![];
        for it in items {
            if let Value::List(inner, _) = it {
                if inner.len() == 2 && inner[0].sym_name() == Some("unquote-splicing") {
                    match self.eval(&inner[1], env)? {
                        Value::Nil => {}
                        Value::List(xs, _) | Value::Vector(xs) => out.extend(xs.iter().cloned()),
                        other => {
                            return fail(
                                "K0103",
                                format!("~@ needs a list or vector, got a {}", other.type_name()),
                            )
                        }
                    }
                    continue;
                }
            }
            out.push(self.quasi(it, env, syms)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------ application

    pub fn apply(&mut self, f: &Value, args: Vec<Value>) -> Res<Value> {
        match f {
            Value::Fn(c) => self.apply_closure(c, args),
            Value::Builtin(name) => builtins::call(self, name, args),
            Value::Keyword(_) => match args.len() {
                1 => get_value(&args[0], f),
                2 => {
                    let v = get_value(&args[0], f)?;
                    Ok(if matches!(v, Value::Nil) { args[1].clone() } else { v })
                }
                _ => fail("K0102", "a keyword takes a map and an optional default"),
            },
            Value::Map(_) => match args.len() {
                1 => get_value(f, &args[0]),
                _ => fail("K0102", "a map takes one key"),
            },
            Value::Ctor(ty) => self.declare(HandleKind::Resource, ty, &args),
            Value::TfFn(name) => match &**name {
                // Raw Terraform expression. Strings are expression text; any
                // other value is rendered as an expression and spliced in.
                "expr" => {
                    if args.is_empty() {
                        return fail("K0102", "(tf/expr \"raw terraform expression\" value...)");
                    }
                    let mut text = String::new();
                    for a in &args {
                        match a {
                            Value::Str(s) => text.push_str(s),
                            other => text.push_str(&tf::render_expr(other)?),
                        }
                    }
                    Ok(Value::Deferred(Rc::new(TfExpr::Raw(text))))
                }
                "and" | "or" => match args.len() {
                    2 => Ok(Value::Deferred(Rc::new(TfExpr::Bin(
                        if &**name == "and" { "&&" } else { "||" },
                        args[0].clone(),
                        args[1].clone(),
                    )))),
                    _ => fail("K0102", "(tf/and a b) or (tf/or a b)"),
                },
                "if" => match args.len() {
                    3 => Ok(Value::Deferred(Rc::new(TfExpr::Cond(
                        args[0].clone(),
                        args[1].clone(),
                        args[2].clone(),
                    )))),
                    _ => fail("K0102", "(tf/if test then else)"),
                },
                _ => Ok(Value::Deferred(Rc::new(TfExpr::Call(name.to_string(), args)))),
            },
            other => fail("K0101", format!("a {} is not callable", other.type_name())),
        }
    }

    pub fn apply_closure(&mut self, c: &Rc<Closure>, args: Vec<Value>) -> Res<Value> {
        let i = match c.decreasing {
            None => {
                let mut vars = vec![];
                self.bind_params(c, args, &mut vars)?;
                let env = extend(&c.env, vars);
                return self.body(&c.body, &env);
            }
            Some(i) => i,
        };
        // A recursive function: it sees itself, and its shrinking argument must shrink.
        let name = c.name.clone().unwrap();
        let size = args.get(i).map(value_size).unwrap_or(0);
        let me = Rc::as_ptr(c);
        if let Some((_, prev)) = self.rec_stack.iter().rev().find(|(f, _)| *f == me) {
            if size >= *prev {
                return Err(KilnError::new(
                    "K0402",
                    format!(
                        "`{}` called itself without its argument {} getting smaller (size {} -> {})",
                        name,
                        i + 1,
                        prev,
                        size
                    ),
                )
                .help("stop the recursion when the argument is empty, e.g. (if (empty? xs) ... (f (rest xs)))"));
            }
        }
        let mut vars = vec![(name, Value::Fn(c.clone()))];
        self.bind_params(c, args, &mut vars)?;
        let env = extend(&c.env, vars);
        self.rec_stack.push((me, size));
        let r = self.body(&c.body, &env);
        self.rec_stack.pop();
        r
    }

    fn bind_params(&mut self, c: &Rc<Closure>, args: Vec<Value>, out: &mut Vec<(Rc<str>, Value)>) -> Res<()> {
        let pats = match &c.params {
            Value::Vector(p) => p.clone(),
            _ => return fail("K0103", "parameter list must be a vector"),
        };
        let fname = c.name.as_deref().unwrap_or("anonymous function");
        let fixed = pats.iter().position(|p| p.sym_name() == Some("&")).unwrap_or(pats.len());
        let variadic = fixed < pats.len();
        if args.len() < fixed || (!variadic && args.len() > fixed) {
            return fail(
                "K0102",
                format!(
                    "`{}` takes {}{} argument{}, got {}",
                    fname,
                    if variadic { "at least " } else { "" },
                    fixed,
                    if fixed == 1 { "" } else { "s" },
                    args.len()
                ),
            );
        }
        let env = c.env.clone();
        let mut args = args.into_iter();
        for p in pats.iter().take(fixed) {
            self.bind(p, args.next().unwrap(), &env, out)?;
        }
        if variadic {
            let rest_pat = match pats.get(fixed + 1) {
                Some(p) if pats.len() == fixed + 2 => p,
                _ => return fail("K0103", "`&` must be followed by exactly one pattern"),
            };
            let rest: Vec<Value> = args.collect();
            let v = if let Value::Map(_) = rest_pat {
                if rest.len() % 2 != 0 {
                    return fail("K0102", format!("`{}` takes keyword arguments in pairs", fname));
                }
                let mut entries = vec![];
                for pair in rest.chunks(2) {
                    map_assoc(&mut entries, pair[0].clone(), pair[1].clone());
                }
                Value::map(entries)
            } else if rest.is_empty() {
                Value::Nil
            } else {
                Value::list(rest)
            };
            self.bind(rest_pat, v, &env, out)?;
        }
        Ok(())
    }

    /// Destructuring bind. `env` is where `:or` defaults are evaluated.
    fn bind(&mut self, pat: &Value, val: Value, env: &Env, out: &mut Vec<(Rc<str>, Value)>) -> Res<()> {
        match pat {
            Value::Symbol(s) => {
                out.push((s.clone(), val));
                Ok(())
            }
            Value::Vector(ps) => {
                let items = seq_of(&val)?;
                let mut i = 0;
                let mut n = 0;
                while i < ps.len() {
                    match &ps[i] {
                        Value::Symbol(s) if &**s == "&" => {
                            let rest: Vec<Value> = items.iter().skip(n).cloned().collect();
                            let rv = if rest.is_empty() { Value::Nil } else { Value::list(rest) };
                            let p = ps.get(i + 1).ok_or_else(|| KilnError::new("K0103", "`&` needs a pattern"))?;
                            self.bind(p, rv, env, out)?;
                            i += 2;
                        }
                        Value::Keyword(k) if &**k == "as" => {
                            let p = ps.get(i + 1).ok_or_else(|| KilnError::new("K0103", ":as needs a name"))?;
                            self.bind(p, val.clone(), env, out)?;
                            i += 2;
                        }
                        p => {
                            self.bind(p, items.get(n).cloned().unwrap_or(Value::Nil), env, out)?;
                            n += 1;
                            i += 1;
                        }
                    }
                }
                Ok(())
            }
            Value::Map(entries) => {
                let start = out.len();
                let mut defaults: Option<Value> = None;
                for (k, v) in entries.iter() {
                    match k {
                        Value::Keyword(kw) if &**kw == "keys" => {
                            for sym in seq_of(v)? {
                                let name = self.binding_name(&sym, ":keys")?;
                                let got = get_value(&val, &Value::Keyword(name.clone()))?;
                                out.push((name, got));
                            }
                        }
                        Value::Keyword(kw) if &**kw == "or" => defaults = Some(v.clone()),
                        Value::Keyword(kw) if &**kw == "as" => {
                            out.push((self.binding_name(v, ":as")?, val.clone()));
                        }
                        p => {
                            let got = get_value(&val, v)?;
                            self.bind(p, got, env, out)?;
                        }
                    }
                }
                if let Some(Value::Map(ds)) = defaults {
                    for (name, expr) in ds.iter() {
                        let name = self.binding_name(name, ":or")?;
                        let idx = out[start..]
                            .iter()
                            .position(|(n, v)| *n == name && matches!(v, Value::Nil));
                        if let Some(idx) = idx {
                            out[start + idx].1 = self.eval(expr, env)?;
                        }
                    }
                }
                Ok(())
            }
            other => fail("K0103", format!("a {} is not a binding pattern", other.type_name())),
        }
    }

    // ------------------------------------------------------------ declarations

    /// The scoped name, and every name it may have had before: one for each
    /// way of reverting the `:was` annotations on the enclosing component
    /// instances and on the name itself.
    fn addr_names(&self, leaf: &(String, Option<String>)) -> (String, Vec<String>) {
        let segs: Vec<&(String, Option<String>)> = self.scope.iter().chain(std::iter::once(leaf)).collect();
        let join = |pick: &dyn Fn(usize) -> bool| {
            segs.iter()
                .enumerate()
                .map(|(i, (name, was))| match was {
                    Some(w) if pick(i) => w.as_str(),
                    _ => name.as_str(),
                })
                .collect::<Vec<_>>()
                .join("__")
        };
        let current = join(&|_| false);
        let renamed: Vec<usize> = (0..segs.len()).filter(|i| segs[*i].1.is_some()).collect();
        let mut olds = vec![];
        if renamed.len() <= 12 {
            for mask in 1u32..(1u32 << renamed.len()) {
                let old = join(&|i| renamed.iter().position(|r| *r == i).map_or(false, |b| mask & (1 << b) != 0));
                if old != current && !olds.contains(&old) {
                    olds.push(old);
                }
            }
        }
        (current, olds)
    }

    fn logical_name(&self, v: &Value, what: &str) -> Res<(String, Option<String>)> {
        if let Value::Named(n) = v {
            let (name, _) = self.logical_name(&Value::str(&n.name), what)?;
            let (was, _) = self.logical_name(&Value::str(&n.was), "a :was name")?;
            return Ok((name, Some(was)));
        }
        let s = match v {
            Value::Str(s) | Value::Keyword(s) => s.to_string(),
            Value::Deferred(_) => {
                return Err(KilnError::new(
                    "K0314",
                    format!("{} depends on a value known only after apply", what),
                )
                .help("addresses are fixed at compile time; derive the name from compile-time data"))
            }
            other => {
                return fail("K0201", format!("{} must be a string, got a {}", what, other.type_name()))
            }
        };
        if !valid_name(&s) {
            return Err(KilnError::new("K0201", format!("`{}` is not a valid Terraform name", s)).help(
                "names start with a letter or underscore and contain only letters, digits, \
                 underscores and hyphens; (slug x) converts arbitrary text",
            ));
        }
        Ok((s, None))
    }

    fn declare(&mut self, kind: HandleKind, ty: &str, args: &[Value]) -> Res<Value> {
        if args.is_empty() || args.len() > 2 {
            return fail("K0102", format!("a declaration takes a name and a map: ({} \"name\" {{...}})", ty));
        }
        let (name, olds) = self.addr_names(&self.logical_name(&args[0], "a resource name")?);
        let body = match args.get(1) {
            None | Some(Value::Nil) => J::Object(JMap::new()),
            Some(m @ Value::Map(_)) => tf::to_json(m)?,
            Some(other) => {
                return fail("K0103", format!("resource arguments must be a map, got a {}", other.type_name()))
            }
        };
        let mut body = body;
        let what = match kind {
            HandleKind::Data => format!("data.{}.{}", ty, name),
            _ => format!("{}.{}", ty, name),
        };
        let inst = self.instancing(&mut body, &what)?;
        let schema = self.type_schema(kind, ty);
        let h = Handle { kind, ty: ty.to_string(), name: name.clone(), schema: schema.clone(), inst };
        if let Some(block) = &schema {
            let ck = if kind == HandleKind::Data { check::Kind::Data } else { check::Kind::Resource };
            let mut issues = check::check_body(ck, block, &body);
            if let Some(args) = args.get(1) {
                issues.extend(check::check_secrets(ck, block, args));
            }
            self.report(&h.address(), issues);
        }
        let table = match kind {
            HandleKind::Data => &mut self.graph.data,
            _ => &mut self.graph.resources,
        };
        let slot = table.entry(ty.to_string()).or_default();
        if slot.contains_key(&name) {
            return Err(KilnError::new("K0202", format!("`{}` is declared twice", h.address())).help(
                "give each declaration a distinct name, or place them in components with different instance names",
            ));
        }
        slot.insert(name, body);
        self.graph.count += 1;
        if kind == HandleKind::Resource && !olds.is_empty() {
            let olds = olds.iter().map(|o| format!("{}.{}", ty, o)).collect();
            self.graph.renames.push((h.address(), olds));
        }
        Ok(Value::Handle(Rc::new(h)))
    }

    // ------------------------------------------------------------ for the type checker

    /// The Terraform type a namespaced constructor symbol such as `g/compute_network` names.
    pub fn ctor_type(&self, sym: &str) -> Option<String> {
        let (ns, name) = sym.split_once('/')?;
        let p = self.aliases.get(ns)?;
        Some(format!("{}_{}", self.providers[p].prefix, name))
    }

    /// The cached schema block of a resource or data source type, without
    /// reporting anything when there is none.
    pub fn peek_schema(&self, data: bool, ty: &str) -> Option<Rc<J>> {
        let set = self.schemas.as_ref()?;
        let name = self.provider_of(ty)?;
        let req = self.providers.get(&name)?;
        let ps = set.get(&req.source)?;
        // a schema of the wrong version is not used to check anything
        if let Some(c) = &req.version {
            if !schema::satisfies(&ps.version, c).unwrap_or(false) {
                return None;
            }
        }
        let table = if data { &ps.data_sources } else { &ps.resources };
        table.get(ty).cloned()
    }

    /// The cached interface of a module, if there is one whose version
    /// satisfies the constraint. The second value is a version mismatch.
    pub fn module_interface(&self, source: &str, version: Option<&str>) -> (Option<Rc<schema::ModuleInterface>>, Option<String>) {
        let set = match &self.schemas {
            Some(s) => s,
            None => return (None, None),
        };
        let key = schema::module_key(source, &self.stack_dir, set.dir().and_then(|d| d.parent()));
        match set.module(&key) {
            Some(m) => match version {
                Some(c) if !m.version.is_empty() && !schema::satisfies(&m.version, c).unwrap_or(true) => {
                    (None, Some(format!("the cached interface of {} is version {}, which does not satisfy \"{}\"", key, m.version, c)))
                }
                _ => (Some(m), None),
            },
            None => (None, None),
        }
    }

    // ------------------------------------------------------------ schemas

    fn current_pos(&self) -> Option<Pos> {
        self.pos_stack.last().copied()
    }

    fn report(&mut self, what: &str, issues: Vec<check::Issue>) {
        let pos = self.current_pos();
        for i in issues {
            let mut e = KilnError::new(i.code, format!("{}: {}", what, i.msg)).at(pos);
            e.help = i.help;
            if i.warning {
                self.warnings.push(e);
            } else {
                self.diags.push(e);
            }
        }
    }

    /// The declared provider a resource type belongs to: the longest
    /// matching prefix, so `google_beta_x` goes to `google-beta` if declared
    /// with that prefix.
    fn provider_of(&self, ty: &str) -> Option<String> {
        self.providers
            .iter()
            .filter(|(_, p)| ty.starts_with(&format!("{}_", p.prefix)))
            .max_by_key(|(_, p)| p.prefix.len())
            .map(|(n, _)| n.clone())
    }

    /// The cached schema for a declared provider, after checking that the
    /// cached version satisfies the stack's constraint.
    fn provider_schema(&mut self, name: &str) -> Option<Rc<schema::ProviderSchema>> {
        let set = self.schemas.clone()?;
        let req = self.providers.get(name)?;
        let source = schema::normalize_source(&req.source);
        let ps = match set.get(&source) {
            Some(ps) => ps,
            None => {
                self.unchecked.insert(source);
                return None;
            }
        };
        if let Some(c) = req.version.clone() {
            if self.version_checked.insert(source.clone()) {
                match schema::satisfies(&ps.version, &c) {
                    Ok(true) => {}
                    Ok(false) => {
                        let e = KilnError::new(
                            "K0711",
                            format!(
                                "the cached schema for {} is version {}, which does not satisfy \"{}\"",
                                source, ps.version, c
                            ),
                        )
                        .help("refresh it with `kiln schema <file.kiln>`")
                        .at(self.current_pos());
                        self.diags.push(e);
                    }
                    Err(msg) => self.diags.push(KilnError::new("K0711", msg).at(self.current_pos())),
                }
            }
            if !schema::satisfies(&ps.version, &c).unwrap_or(false) {
                return None;
            }
        }
        Some(ps)
    }

    fn type_schema(&mut self, kind: HandleKind, ty: &str) -> Option<Rc<J>> {
        let name = self.provider_of(ty)?;
        let ps = self.provider_schema(&name)?;
        let table = if kind == HandleKind::Data { &ps.data_sources } else { &ps.resources };
        if let Some(b) = table.get(ty) {
            return Some(b.clone());
        }
        let what = if kind == HandleKind::Data { "data source" } else { "resource type" };
        let mut e = KilnError::new(
            "K0700",
            format!("{} {} has no {} `{}`", ps.source, ps.version, what, ty),
        )
        .at(self.current_pos());
        if let Some(s) = check::suggest(ty, table.keys().map(|k| k.as_str())) {
            e = e.help(format!("did you mean `{}`?", s));
        }
        self.diags.push(e);
        None
    }

    // ------------------------------------------------------------ expansion

    /// Expand every macro call in `form`, for `kiln expand`.
    pub fn expand_all(&mut self, form: &Value, n: u32) -> Res<Value> {
        if n > MAX_EXPANSIONS {
            return fail("K0400", "macro expansion did not terminate");
        }
        match form {
            Value::List(items, pos) if !items.is_empty() => {
                if let Some(h) = items[0].sym_name() {
                    if h == "quote" || h == "quasiquote" {
                        return Ok(form.clone());
                    }
                    if let Some(Value::Macro(c)) = lookup(&self.global, h) {
                        let ex = self.apply_closure(&c, items[1..].to_vec()).map_err(|e| e.at(*pos))?;
                        let ex = match ex {
                            Value::List(xs, None) => Value::List(xs, *pos),
                            other => other,
                        };
                        return self.expand_all(&ex, n + 1);
                    }
                }
                let mut out = vec![];
                for it in items.iter() {
                    out.push(self.expand_all(it, n)?);
                }
                Ok(Value::List(Rc::new(out), *pos))
            }
            Value::Vector(items) => {
                let mut out = vec![];
                for it in items.iter() {
                    out.push(self.expand_all(it, n)?);
                }
                Ok(Value::vector(out))
            }
            Value::Map(entries) => {
                let mut out = vec![];
                for (k, v) in entries.iter() {
                    out.push((self.expand_all(k, n)?, self.expand_all(v, n)?));
                }
                Ok(Value::map(out))
            }
            _ => Ok(form.clone()),
        }
    }
}
