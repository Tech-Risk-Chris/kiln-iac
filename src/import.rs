//! `kiln import`: decompile HCL into Kiln.
//!
//! Every resource keeps its address, so a stack imported from a configuration
//! that is already applied plans as a no-op. HCL is declarative and unordered;
//! Kiln has no forward references, so declarations are emitted in dependency
//! order. Expressions with a direct Kiln counterpart are translated; the rest
//! are carried over as raw Terraform expressions with `tf/expr`.

use crate::builtins;
use crate::tf::hcl_quote;
use crate::value::quote_str;
use hcl::expr::{
    BinaryOperator, Expression, HeredocStripMode, ObjectKey, Operation, TemplateExpr, Traversal,
    TraversalOperator, UnaryOperator,
};
use hcl::template::{Element, Template};
use hcl::{Block, Body, Structure};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;

pub struct Imported {
    pub source: String,
    pub notes: Vec<String>,
}

const SPECIAL: &[&str] = &[
    "def", "defn", "defmacro", "ns", "fn", "let", "if", "do", "for", "and", "or", "quote",
    "quasiquote", "unquote", "unquote-splicing", "scope", "data", "provider", "output", "backend",
    "module", "when", "when-not", "cond", "defcomponent", "nil", "true", "false", "&",
];

struct Item {
    class: u8,
    order: usize,
    key: Option<String>,
    text: String,
    deps: BTreeSet<String>,
}

enum Piece {
    Text(String),
    Code(String),
}

#[derive(Clone, Copy, PartialEq)]
enum BodyKind {
    Declaration,
    Lifecycle,
    Plain,
}

#[derive(Default)]
struct Tr {
    syms: HashMap<String, String>,
    used: BTreeSet<String>,
    errors: Vec<String>,
    notes: Vec<String>,
    /// provider name -> (source, version)
    providers: BTreeMap<String, (Option<String>, Option<String>)>,
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

fn simple_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !s.chars().next().unwrap().is_ascii_digit()
}

fn fmt_map(entries: Vec<(String, String)>, col: usize) -> String {
    if entries.is_empty() {
        return "{}".into();
    }
    let one: Vec<String> = entries.iter().map(|(k, v)| format!("{} {}", k, v)).collect();
    let flat = format!("{{{}}}", one.join(" "));
    if entries.len() <= 2 && !flat.contains('\n') && flat.len() <= 56 {
        return flat;
    }
    format!("{{{}}}", one.join(&format!("\n{}", pad(col + 1))))
}

fn fmt_vec(items: Vec<String>, col: usize) -> String {
    let flat = format!("[{}]", items.join(" "));
    if !flat.contains('\n') && flat.len() + col <= 84 {
        return flat;
    }
    format!("[{}]", items.join(&format!("\n{}", pad(col + 1))))
}

fn dedent(s: &str) -> String {
    let indent = s
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for l in s.split_inclusive('\n') {
        out.push_str(if l.len() >= indent && l.trim() != "" { &l[indent..] } else { l.trim_start_matches([' ', '\t']) });
    }
    out
}

fn split_type(ty: &str) -> Option<(&str, &str)> {
    match ty.split_once('_') {
        Some((p, rest)) if !p.is_empty() && !rest.is_empty() => Some((p, rest)),
        _ => None,
    }
}

impl Tr {
    fn unsupported(&mut self, at: &str, what: &str) {
        self.errors.push(format!("{}: {}", at, what));
    }

    fn assign(&mut self, key: String, candidates: &[String]) {
        let reserved = |s: &str| SPECIAL.contains(&s) || builtins::NAMES.contains(&s);
        let mut pick = None;
        for c in candidates {
            if !self.used.contains(c) && !reserved(c) {
                pick = Some(c.clone());
                break;
            }
        }
        let pick = pick.unwrap_or_else(|| {
            let base = candidates.last().unwrap().clone();
            let mut n = 2;
            loop {
                let c = format!("{}-{}", base, n);
                if !self.used.contains(&c) {
                    break c;
                }
                n += 1;
            }
        });
        self.used.insert(pick.clone());
        self.syms.insert(key, pick);
    }

    // -------------------------------------------------------- references

    /// Resolve the head of a traversal to a declared thing. Returns its
    /// symbol, how many operators the head consumed, and whether it is a
    /// resource-like handle (as opposed to a local or variable value).
    fn resolve(&self, root: &str, ops: &[TraversalOperator]) -> Option<(String, String, usize, bool)> {
        let attr = |i: usize| match ops.get(i) {
            Some(TraversalOperator::GetAttr(id)) => Some(id.as_str().to_string()),
            _ => None,
        };
        let (key, n, handle) = match root {
            "var" | "local" => (format!("{}.{}", root, attr(0)?), 1, false),
            "data" => (format!("data.{}.{}", attr(0)?, attr(1)?), 2, true),
            "module" => (format!("module.{}", attr(0)?), 1, true),
            _ => (format!("{}.{}", root, attr(0)?), 1, true),
        };
        let sym = self.syms.get(&key)?.clone();
        Some((key, sym, n, handle))
    }

    fn traversal(&mut self, e: &Expression, t: &Traversal, col: usize, deps: &mut BTreeSet<String>, at: &str) -> String {
        let splat = t
            .operators
            .iter()
            .any(|o| matches!(o, TraversalOperator::AttrSplat | TraversalOperator::FullSplat));
        if splat {
            return self.raw(e, deps, at);
        }
        let (mut cur, rest) = match &t.expr {
            Expression::Variable(v) => match self.resolve(v.as_str(), &t.operators) {
                Some((key, sym, n, handle)) => {
                    if handle && t.operators.len() == n {
                        return self.raw(e, deps, at); // the whole object
                    }
                    deps.insert(key);
                    (sym, &t.operators[n..])
                }
                None => return self.raw(e, deps, at),
            },
            base => (self.expr(base, col, deps, at), &t.operators[..]),
        };
        for op in rest {
            cur = match op {
                TraversalOperator::GetAttr(id) => format!("(:{} {})", id.as_str(), cur),
                TraversalOperator::Index(ix) => format!("(get {} {})", cur, self.expr(ix, col, deps, at)),
                TraversalOperator::LegacyIndex(n) => format!("(get {} {})", cur, n),
                _ => unreachable!(),
            };
        }
        cur
    }

    // -------------------------------------------------------- raw fallback

    fn raw(&mut self, e: &Expression, deps: &mut BTreeSet<String>, at: &str) -> String {
        let mut pieces = vec![];
        self.pieces(e, &mut pieces, deps, at);
        let mut merged: Vec<Piece> = vec![];
        for p in pieces {
            match (merged.last_mut(), p) {
                (Some(Piece::Text(a)), Piece::Text(b)) => a.push_str(&b),
                (_, p) => merged.push(p),
            }
        }
        let parts: Vec<String> = merged
            .iter()
            .map(|p| match p {
                Piece::Text(t) => quote_str(t),
                Piece::Code(c) => c.clone(),
            })
            .collect();
        format!("(tf/expr {})", parts.join(" "))
    }

    /// Print an expression as HCL text, splicing in Kiln values where it
    /// refers to something that is now a compile-time definition.
    fn pieces(&mut self, e: &Expression, out: &mut Vec<Piece>, deps: &mut BTreeSet<String>, at: &str) {
        let text = |out: &mut Vec<Piece>, s: &str| out.push(Piece::Text(s.to_string()));
        match e {
            Expression::Null | Expression::Bool(_) | Expression::Number(_) | Expression::String(_) => {
                text(out, &e.to_string())
            }
            Expression::Variable(v) => text(out, v.as_str()),
            Expression::Array(items) => {
                text(out, "[");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        text(out, ", ");
                    }
                    self.pieces(it, out, deps, at);
                }
                text(out, "]");
            }
            Expression::Object(obj) => {
                text(out, "{");
                for (i, (k, v)) in obj.iter().enumerate() {
                    if i > 0 {
                        text(out, ", ");
                    }
                    match k {
                        ObjectKey::Identifier(id) => text(out, id.as_str()),
                        ObjectKey::Expression(Expression::String(s)) => text(out, &hcl_quote(s)),
                        ObjectKey::Expression(ke) => {
                            text(out, "(");
                            self.pieces(ke, out, deps, at);
                            text(out, ")");
                        }
                        _ => self.unsupported(at, "an object key of an unknown kind"),
                    }
                    text(out, " = ");
                    self.pieces(v, out, deps, at);
                }
                text(out, "}");
            }
            Expression::TemplateExpr(t) => match self.elements(t) {
                Some(elements) if !elements.iter().any(|el| matches!(el, Element::Directive(_))) => {
                    text(out, "\"");
                    for el in &elements {
                        match el {
                            Element::Literal(l) => {
                                let q = hcl_quote(l);
                                text(out, &q[1..q.len() - 1]);
                            }
                            Element::Interpolation(i) => {
                                text(out, "${");
                                self.pieces(&i.expr, out, deps, at);
                                text(out, "}");
                            }
                            Element::Directive(_) => unreachable!(),
                        }
                    }
                    text(out, "\"");
                }
                _ => self.unsupported(at, "a template with %{ } directives"),
            },
            Expression::Traversal(t) => {
                let mut ops = &t.operators[..];
                match &t.expr {
                    Expression::Variable(v) => match self.resolve(v.as_str(), ops) {
                        Some((key, sym, n, handle)) => {
                            deps.insert(key);
                            if handle {
                                text(out, v.as_str()); // stays a Terraform reference
                            } else {
                                out.push(Piece::Code(sym));
                                ops = &ops[n..];
                            }
                        }
                        None => text(out, v.as_str()),
                    },
                    base => self.pieces(base, out, deps, at),
                }
                for op in ops {
                    match op {
                        TraversalOperator::GetAttr(id) => text(out, &format!(".{}", id.as_str())),
                        TraversalOperator::LegacyIndex(n) => text(out, &format!(".{}", n)),
                        TraversalOperator::AttrSplat => text(out, ".*"),
                        TraversalOperator::FullSplat => text(out, "[*]"),
                        TraversalOperator::Index(ix) => {
                            text(out, "[");
                            self.pieces(ix, out, deps, at);
                            text(out, "]");
                        }
                    }
                }
            }
            Expression::FuncCall(f) => {
                text(out, &format!("{}(", f.name));
                for (i, a) in f.args.iter().enumerate() {
                    if i > 0 {
                        text(out, ", ");
                    }
                    self.pieces(a, out, deps, at);
                }
                text(out, if f.expand_final { "...)" } else { ")" });
            }
            Expression::Parenthesis(inner) => {
                text(out, "(");
                self.pieces(inner, out, deps, at);
                text(out, ")");
            }
            Expression::Conditional(c) => {
                text(out, "(");
                self.pieces(&c.cond_expr, out, deps, at);
                text(out, " ? ");
                self.pieces(&c.true_expr, out, deps, at);
                text(out, " : ");
                self.pieces(&c.false_expr, out, deps, at);
                text(out, ")");
            }
            Expression::Operation(op) => match &**op {
                Operation::Unary(u) => {
                    text(out, u.operator.as_str());
                    self.pieces(&u.expr, out, deps, at);
                }
                Operation::Binary(b) => {
                    text(out, "(");
                    self.pieces(&b.lhs_expr, out, deps, at);
                    text(out, &format!(" {} ", b.operator.as_str()));
                    self.pieces(&b.rhs_expr, out, deps, at);
                    text(out, ")");
                }
            },
            Expression::ForExpr(f) => {
                let object = f.key_expr.is_some();
                text(out, if object { "{for " } else { "[for " });
                if let Some(k) = &f.key_var {
                    text(out, &format!("{}, ", k.as_str()));
                }
                text(out, &format!("{} in ", f.value_var.as_str()));
                self.pieces(&f.collection_expr, out, deps, at);
                text(out, " : ");
                if let Some(k) = &f.key_expr {
                    self.pieces(k, out, deps, at);
                    text(out, " => ");
                }
                self.pieces(&f.value_expr, out, deps, at);
                if f.grouping {
                    text(out, "...");
                }
                if let Some(c) = &f.cond_expr {
                    text(out, " if ");
                    self.pieces(c, out, deps, at);
                }
                text(out, if object { "}" } else { "]" });
            }
            _ => self.unsupported(at, "an expression of an unknown kind"),
        }
    }

    // -------------------------------------------------------- expressions

    fn elements(&mut self, t: &TemplateExpr) -> Option<Vec<Element>> {
        let parsed = match t {
            TemplateExpr::Heredoc(h) if h.strip == HeredocStripMode::Indent => {
                Template::from_str(&dedent(&h.template))
            }
            other => Template::from_expr(other),
        };
        parsed.ok().map(|t| t.elements().to_vec())
    }

    fn template(&mut self, e: &Expression, t: &TemplateExpr, col: usize, deps: &mut BTreeSet<String>, at: &str) -> String {
        let elements = match self.elements(t) {
            Some(els) => els,
            None => {
                self.unsupported(at, "a template that could not be parsed");
                return "nil".into();
            }
        };
        if elements.iter().any(|el| matches!(el, Element::Directive(_))) {
            return self.raw(e, deps, at);
        }
        let mut parts = vec![];
        for el in &elements {
            match el {
                Element::Literal(l) => parts.push(quote_str(l)),
                Element::Interpolation(i) => parts.push(self.expr(&i.expr, col, deps, at)),
                Element::Directive(_) => unreachable!(),
            }
        }
        match (parts.len(), elements.first()) {
            (0, _) => "\"\"".into(),
            (1, Some(Element::Literal(_))) => parts.remove(0),
            _ => {
                let flat = format!("(str {})", parts.join(" "));
                if flat.len() + col <= 84 {
                    flat
                } else {
                    format!("(str {})", parts.join(&format!("\n{}", pad(col + 5))))
                }
            }
        }
    }

    fn expr(&mut self, e: &Expression, col: usize, deps: &mut BTreeSet<String>, at: &str) -> String {
        match e {
            Expression::Null => "nil".into(),
            Expression::Bool(b) => b.to_string(),
            Expression::Number(n) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => i.to_string(),
                (None, Some(f)) => crate::value::fmt_float(f),
                _ => n.to_string(),
            },
            Expression::String(s) => quote_str(s),
            Expression::Array(items) => {
                let parts = items.iter().map(|i| self.expr(i, col + 1, deps, at)).collect();
                fmt_vec(parts, col)
            }
            Expression::Object(obj) => {
                let plain = obj.keys().all(|k| {
                    matches!(k, ObjectKey::Identifier(_) | ObjectKey::Expression(Expression::String(_)))
                });
                if !plain {
                    return self.raw(e, deps, at);
                }
                let mut entries = vec![];
                for (k, v) in obj.iter() {
                    let name = match k {
                        ObjectKey::Identifier(id) => id.as_str().to_string(),
                        ObjectKey::Expression(Expression::String(s)) => s.clone(),
                        _ => unreachable!(),
                    };
                    let key = if simple_key(&name) { format!(":{}", name) } else { quote_str(&name) };
                    let vcol = col + 1 + key.len() + 1;
                    entries.push((key, self.expr(v, vcol, deps, at)));
                }
                fmt_map(entries, col)
            }
            Expression::TemplateExpr(t) => self.template(e, t, col, deps, at),
            Expression::Variable(_) | Expression::ForExpr(_) => self.raw(e, deps, at),
            Expression::Traversal(t) => self.traversal(e, t, col, deps, at),
            Expression::FuncCall(f) => {
                if f.expand_final || f.name.is_namespaced() {
                    return self.raw(e, deps, at);
                }
                let mut s = format!("(tf/{}", f.name.name.as_str());
                for a in &f.args {
                    s.push(' ');
                    s.push_str(&self.expr(a, col, deps, at));
                }
                s.push(')');
                s
            }
            Expression::Parenthesis(inner) => self.expr(inner, col, deps, at),
            Expression::Conditional(c) => format!(
                "(tf/if {} {} {})",
                self.expr(&c.cond_expr, col, deps, at),
                self.expr(&c.true_expr, col, deps, at),
                self.expr(&c.false_expr, col, deps, at)
            ),
            Expression::Operation(op) => match &**op {
                Operation::Unary(u) => {
                    let f = match u.operator {
                        UnaryOperator::Neg => "-",
                        UnaryOperator::Not => "not",
                    };
                    format!("({} {})", f, self.expr(&u.expr, col, deps, at))
                }
                Operation::Binary(b) => {
                    let f = match b.operator {
                        BinaryOperator::Eq => "=",
                        BinaryOperator::NotEq => "not=",
                        BinaryOperator::LessEq => "<=",
                        BinaryOperator::GreaterEq => ">=",
                        BinaryOperator::Less => "<",
                        BinaryOperator::Greater => ">",
                        BinaryOperator::Plus => "+",
                        BinaryOperator::Minus => "-",
                        BinaryOperator::Mul => "*",
                        BinaryOperator::Div => "/",
                        BinaryOperator::Mod => "mod",
                        BinaryOperator::And => "tf/and",
                        BinaryOperator::Or => "tf/or",
                    };
                    format!(
                        "({} {} {})",
                        f,
                        self.expr(&b.lhs_expr, col, deps, at),
                        self.expr(&b.rhs_expr, col, deps, at)
                    )
                }
            },
            _ => {
                self.unsupported(at, "an expression of an unknown kind");
                "nil".into()
            }
        }
    }

    // -------------------------------------------------------- bodies

    /// A reference written where Terraform wants an address, not a value.
    fn reference(&mut self, e: &Expression, deps: &mut BTreeSet<String>) -> String {
        if let Expression::Traversal(t) = e {
            if let Expression::Variable(v) = &t.expr {
                if let Some((key, sym, n, true)) = self.resolve(v.as_str(), &t.operators) {
                    if n == t.operators.len() {
                        deps.insert(key);
                        return sym; // a handle; emitted as its address
                    }
                }
            }
        }
        match e {
            Expression::String(s) => quote_str(s),
            other => quote_str(&other.to_string()),
        }
    }

    fn reference_list(&mut self, e: &Expression, col: usize, deps: &mut BTreeSet<String>, as_text: bool) -> String {
        match e {
            Expression::Array(items) => {
                let parts = items
                    .iter()
                    .map(|i| if as_text { quote_str(&i.to_string()) } else { self.reference(i, deps) })
                    .collect();
                fmt_vec(parts, col)
            }
            other => quote_str(&other.to_string()), // e.g. ignore_changes = all
        }
    }

    fn body(&mut self, body: &Body, kind: BodyKind, col: usize, deps: &mut BTreeSet<String>, at: &str) -> String {
        let mut entries: Vec<(String, String)> = vec![];
        let mut blocks: Vec<(String, Vec<&Block>)> = vec![];
        for s in body.iter() {
            match s {
                Structure::Attribute(a) => {
                    let name = a.key.as_str();
                    let key = format!(":{}", name);
                    let vcol = col + 1 + key.len() + 1;
                    let value = match (kind, name) {
                        (BodyKind::Declaration, "depends_on") => self.reference_list(&a.expr, vcol, deps, false),
                        (BodyKind::Declaration, "provider") => quote_str(&a.expr.to_string()),
                        (BodyKind::Declaration, "providers") => match &a.expr {
                            Expression::Object(obj) => {
                                let es = obj
                                    .iter()
                                    .map(|(k, v)| (quote_str(&k.to_string()), quote_str(&v.to_string())))
                                    .collect();
                                fmt_map(es, vcol)
                            }
                            other => self.expr(other, vcol, deps, at),
                        },
                        (BodyKind::Lifecycle, "ignore_changes") | (BodyKind::Lifecycle, "replace_triggered_by") => {
                            self.reference_list(&a.expr, vcol, deps, true)
                        }
                        _ => self.expr(&a.expr, vcol, deps, at),
                    };
                    entries.push((key, value));
                }
                Structure::Block(b) => {
                    let name = b.identifier.as_str().to_string();
                    if !b.labels.is_empty() {
                        let what = if name == "dynamic" {
                            "a dynamic block (write it as a `for` over a map in Kiln)".to_string()
                        } else {
                            format!("a labelled nested block `{}`", name)
                        };
                        self.unsupported(at, &what);
                        continue;
                    }
                    match blocks.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, v)) => v.push(b),
                        None => blocks.push((name, vec![b])),
                    }
                }
            }
        }
        for (name, bs) in blocks {
            let key = format!(":{}", name);
            let vcol = col + 1 + key.len() + 1;
            let inner = if kind == BodyKind::Declaration && name == "lifecycle" {
                BodyKind::Lifecycle
            } else {
                BodyKind::Plain
            };
            let value = if bs.len() == 1 {
                self.body(&bs[0].body, inner, vcol, deps, at)
            } else {
                let parts = bs.iter().map(|b| self.body(&b.body, inner, vcol + 1, deps, at)).collect::<Vec<_>>();
                format!("[{}]", parts.join(&format!("\n{}", pad(vcol + 1))))
            };
            entries.push((key, value));
        }
        fmt_map(entries, col)
    }
}

fn def_form(sym: &str, value: &str) -> String {
    if !value.contains('\n') && sym.len() + value.len() <= 70 {
        format!("(def {} {})", sym, value)
    } else {
        format!("(def {}\n  {})", sym, value)
    }
}

fn labels(b: &Block) -> Vec<String> {
    b.labels.iter().map(|l| l.as_str().to_string()).collect()
}

/// Import a set of `.tf` files (name, text) as one Kiln stack.
pub fn import(files: &[(String, String)], origin: &str) -> Result<Imported, String> {
    let mut tr = Tr::default();
    let mut bodies = vec![];
    for (name, text) in files {
        let body = hcl::parse(text).map_err(|e| format!("{}: {}", name, e))?;
        bodies.push((name.clone(), body));
    }

    // Pass 1: every declared name gets a symbol.
    for (_, body) in &bodies {
        for b in body.blocks() {
            let l = labels(b);
            match (b.identifier.as_str(), l.as_slice()) {
                ("resource", [ty, name]) => {
                    tr.assign(format!("{}.{}", ty, name), &[name.clone(), format!("{}-{}", ty, name)])
                }
                ("data", [ty, name]) => tr.assign(
                    format!("data.{}.{}", ty, name),
                    &[name.clone(), format!("data-{}", name), format!("data-{}-{}", ty, name)],
                ),
                ("module", [name]) => {
                    tr.assign(format!("module.{}", name), &[name.clone(), format!("module-{}", name)])
                }
                ("variable", [name]) => {
                    tr.assign(format!("var.{}", name), &[name.clone(), format!("var-{}", name)])
                }
                ("locals", []) => {
                    for a in b.body.attributes() {
                        let n = a.key.as_str().to_string();
                        tr.assign(format!("local.{}", n), &[n.clone(), format!("local-{}", n)]);
                    }
                }
                _ => {}
            }
        }
    }

    // Pass 2: translate.
    let mut items: Vec<Item> = vec![];
    let mut backend: Option<String> = None;
    let mut order = 0;
    let mut push = |items: &mut Vec<Item>, class: u8, key: Option<String>, text: String, deps: BTreeSet<String>| {
        items.push(Item { class, order, key, text, deps });
        order += 1;
    };
    for (file, body) in &bodies {
        for a in body.attributes() {
            tr.unsupported(file, &format!("a top-level attribute `{}`", a.key.as_str()));
        }
        for b in body.blocks() {
            let l = labels(b);
            let mut deps = BTreeSet::new();
            match (b.identifier.as_str(), l.as_slice()) {
                ("terraform", []) => {
                    for a in b.body.attributes() {
                        match a.key.as_str() {
                            "required_version" => {
                                if a.expr != Expression::String(crate::emit::MIN_ENGINE.into()) {
                                    tr.notes.push(format!(
                                        "required_version {} is replaced by Kiln's own minimum, \"{}\"",
                                        a.expr,
                                        crate::emit::MIN_ENGINE
                                    ));
                                }
                            }
                            other => tr.unsupported("terraform", &format!("the `{}` setting", other)),
                        }
                    }
                    for inner in b.body.blocks() {
                        let il = labels(inner);
                        match (inner.identifier.as_str(), il.as_slice()) {
                            ("required_providers", []) => {
                                for a in inner.body.attributes() {
                                    let (mut source, mut version) = (None, None);
                                    match &a.expr {
                                        Expression::String(v) => version = Some(v.clone()),
                                        Expression::Object(o) => {
                                            for (k, v) in o.iter() {
                                                let k = k.to_string();
                                                match (k.trim_matches('"'), v) {
                                                    ("source", Expression::String(s)) => source = Some(s.clone()),
                                                    ("version", Expression::String(s)) => version = Some(s.clone()),
                                                    (other, _) => tr.unsupported(
                                                        "terraform.required_providers",
                                                        &format!("the `{}` setting", other),
                                                    ),
                                                }
                                            }
                                        }
                                        _ => tr.unsupported("terraform.required_providers", "a non-literal entry"),
                                    }
                                    tr.providers.insert(a.key.as_str().to_string(), (source, version));
                                }
                            }
                            ("backend", [ty]) => {
                                let at = format!("backend.{}", ty);
                                let m = tr.body(&inner.body, BodyKind::Plain, 2, &mut deps, &at);
                                backend = Some(format!("(backend {}\n  {})", quote_str(ty), m));
                            }
                            (other, _) => tr.unsupported("terraform", &format!("a `{}` block", other)),
                        }
                    }
                }
                ("provider", [name]) => {
                    tr.providers.entry(name.clone()).or_insert((None, None));
                    let at = format!("provider.{}", name);
                    let m = tr.body(&b.body, BodyKind::Plain, 2, &mut deps, &at);
                    push(&mut items, 0, None, format!("(provider {}\n  {})", name, m), deps);
                }
                ("variable", [name]) => {
                    let key = format!("var.{}", name);
                    let at = key.clone();
                    let default = b.body.attributes().find(|a| a.key.as_str() == "default");
                    match default {
                        Some(a) => {
                            let sym = tr.syms[&key].clone();
                            let v = tr.expr(&a.expr, 2, &mut deps, &at);
                            tr.notes.push(format!(
                                "variable \"{}\" is now the compile-time definition `{}`, fixed at its default",
                                name, sym
                            ));
                            push(&mut items, 1, Some(key), def_form(&sym, &v), deps);
                        }
                        None => tr.unsupported(
                            &at,
                            "a variable without a default (inputs arrive with contexts; give it a default for now)",
                        ),
                    }
                }
                ("locals", []) => {
                    for a in b.body.attributes() {
                        let key = format!("local.{}", a.key.as_str());
                        let mut deps = BTreeSet::new();
                        let sym = tr.syms[&key].clone();
                        let v = tr.expr(&a.expr, 2, &mut deps, &key);
                        push(&mut items, 1, Some(key), def_form(&sym, &v), deps);
                    }
                }
                ("resource", [ty, name]) | ("data", [ty, name]) => {
                    let is_data = b.identifier.as_str() == "data";
                    let key = if is_data { format!("data.{}.{}", ty, name) } else { format!("{}.{}", ty, name) };
                    let (provider, rest) = match split_type(ty) {
                        Some(x) => x,
                        None => {
                            tr.unsupported(&key, "a resource type without a provider prefix");
                            continue;
                        }
                    };
                    if provider == "terraform" {
                        tr.providers
                            .entry("terraform".into())
                            .or_insert((Some("terraform.io/builtin/terraform".into()), None));
                    } else {
                        tr.providers.entry(provider.to_string()).or_insert((None, None));
                    }
                    let sym = tr.syms[&key].clone();
                    let m = tr.body(&b.body, BodyKind::Declaration, 4, &mut deps, &key);
                    let head = if is_data { "(data " } else { "(" };
                    let text = format!(
                        "(def {}\n  {}{}/{} {}\n    {}))",
                        sym, head, provider, rest, quote_str(name), m
                    );
                    push(&mut items, 2, Some(key), text, deps);
                }
                ("module", [name]) => {
                    let key = format!("module.{}", name);
                    let sym = tr.syms[&key].clone();
                    let m = tr.body(&b.body, BodyKind::Declaration, 4, &mut deps, &key);
                    let text = format!("(def {}\n  (module {}\n    {}))", sym, quote_str(name), m);
                    push(&mut items, 2, Some(key), text, deps);
                }
                ("output", [name]) => {
                    let at = format!("output.{}", name);
                    let mut value = None;
                    let mut opts = vec![];
                    for a in b.body.attributes() {
                        let k = a.key.as_str();
                        if k == "value" {
                            value = Some(tr.expr(&a.expr, 2, &mut deps, &at));
                        } else if k == "depends_on" {
                            opts.push((":depends_on".to_string(), tr.reference_list(&a.expr, 16, &mut deps, false)));
                        } else {
                            opts.push((format!(":{}", k), tr.expr(&a.expr, 4, &mut deps, &at)));
                        }
                    }
                    if b.body.blocks().next().is_some() {
                        tr.unsupported(&at, "a nested block in an output");
                    }
                    let value = value.unwrap_or_else(|| {
                        tr.unsupported(&at, "an output without a value");
                        "nil".into()
                    });
                    let text = if opts.is_empty() {
                        format!("(output {} {})", quote_str(name), value)
                    } else {
                        format!("(output {} {}\n  {})", quote_str(name), value, fmt_map(opts, 2))
                    };
                    push(&mut items, 3, None, text, deps);
                }
                (other, _) => tr.unsupported(file, &format!("a `{}` block", other)),
            }
        }
    }

    if !tr.errors.is_empty() {
        let mut msg = format!(
            "error[K0600]: {} construct{} in {} {} outside the subset kiln import understands\n",
            tr.errors.len(),
            if tr.errors.len() == 1 { "" } else { "s" },
            origin,
            if tr.errors.len() == 1 { "is" } else { "are" }
        );
        for e in &tr.errors {
            msg.push_str(&format!("   = {}\n", e));
        }
        return Err(msg);
    }

    // Order: nothing may be used before it is defined.
    let index: HashMap<String, usize> =
        items.iter().enumerate().filter_map(|(i, it)| it.key.clone().map(|k| (k, i))).collect();
    let mut done = vec![false; items.len()];
    let mut sequence = vec![];
    while sequence.len() < items.len() {
        let ready = (0..items.len())
            .filter(|i| !done[*i])
            .filter(|i| {
                items[*i].deps.iter().all(|d| match index.get(d) {
                    Some(j) => done[*j] || j == i,
                    None => true,
                })
            })
            .min_by_key(|i| (items[*i].class, items[*i].order));
        match ready {
            Some(i) => {
                done[i] = true;
                sequence.push(i);
            }
            None => {
                let stuck: Vec<String> =
                    (0..items.len()).filter(|i| !done[*i]).filter_map(|i| items[i].key.clone()).collect();
                return Err(format!(
                    "error[K0601]: these declarations in {} refer to each other in a cycle: {}\n",
                    origin,
                    stuck.join(", ")
                ));
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        ";; Imported from {} by `kiln import`.\n;; Every address is unchanged, so the first plan against existing state is a no-op.\n",
        origin
    ));
    for n in &tr.notes {
        out.push_str(&format!(";; Note: {}.\n", n));
    }
    out.push('\n');
    if tr.providers.is_empty() {
        out.push_str("(ns imported)\n");
    } else {
        let specs: Vec<String> = tr
            .providers
            .iter()
            .map(|(name, (source, version))| {
                let mut s = format!("[{}", name);
                if let Some(src) = source {
                    if *src != format!("hashicorp/{}", name) {
                        s.push_str(&format!(" :source {}", quote_str(src)));
                    }
                }
                if let Some(v) = version {
                    s.push_str(&format!(" :version {}", quote_str(v)));
                }
                s.push(']');
                s
            })
            .collect();
        out.push_str(&format!("(ns imported\n  (:provider {}))\n", specs.join(&format!("\n{}", pad(13)))));
    }
    if let Some(b) = backend {
        out.push('\n');
        out.push_str(&b);
        out.push('\n');
    }
    for i in sequence {
        out.push('\n');
        out.push_str(&items[i].text);
        out.push('\n');
    }
    Ok(Imported { source: out, notes: tr.notes })
}
