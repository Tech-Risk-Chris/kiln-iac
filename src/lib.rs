//! Kiln: a Lisp that compiles to Terraform JSON.

pub mod builtins;
pub mod check;
pub mod emit;
pub mod eval;
pub mod import;
pub mod infer;
pub mod ledger;
pub mod reader;
pub mod recursion;
pub mod schema;
pub mod tf;
pub mod value;

use value::{pp, pr_str, KilnError, Value};

fn run<T>(src: &str, file: &str, f: impl FnOnce() -> Result<T, KilnError>) -> Result<T, String> {
    f().map_err(|e| e.render(file, src))
}

/// The result of building one stack.
pub struct Build {
    /// Text of `main.tf.json`.
    pub json: String,
    /// Text of the updated `kiln.lock`.
    pub lock: String,
    /// Moves introduced by this build.
    pub new_moves: Vec<ledger::Move>,
    /// Rendered warnings: deprecated arguments, providers without a schema.
    pub warnings: Vec<String>,
    /// Clashes the type checker found. Not reported yet (M3 step 3): the
    /// checker must first accept all valid code.
    pub type_clashes: Vec<String>,
}

#[derive(Default)]
pub struct BuildOpts<'a> {
    /// The current `kiln.lock`, if there is one.
    pub lock: Option<&'a str>,
    /// The previously generated `main.tf.json`; used only to rank rename proposals.
    pub old_output: Option<&'a str>,
    /// Accept disappearing and appearing addresses as real deletions and creations.
    pub accept_replace: bool,
    /// Provider schemas to check declarations against. None: no checking.
    pub schemas: Option<std::rc::Rc<schema::SchemaSet>>,
    /// A provider without a cached schema is an error, not a warning.
    pub strict: bool,
}

fn unchecked_provider(source: &str) -> KilnError {
    KilnError::new("K0712", format!("no cached schema for {}; its declarations are not checked", source))
        .help("run `kiln schema <file.kiln>` and commit the files it writes under .kiln/schemas/")
}

/// Build one stack: evaluate, check against provider schemas, reconcile
/// addresses against the ledger, emit. A pure function of its arguments and
/// the schema files.
pub fn build(src: &str, file: &str, opts: &BuildOpts) -> Result<Build, String> {
    let mut it = eval::Interp::new().map_err(|e| e.render(file, src))?;
    it.schemas = opts.schemas.clone();
    let mut errors: Vec<String> = vec![];
    let mut fatal = None;
    let mut ty = infer::Infer::new();
    match reader::read_all(src) {
        Ok(forms) => {
            for form in forms {
                ty.toplevel(&mut it, &form);
                if let Err(e) = it.eval_toplevel(&form) {
                    fatal = Some(e);
                    break;
                }
            }
        }
        Err(e) => fatal = Some(e),
    }
    // Problems found before evaluation stopped are still worth reporting.
    errors.extend(it.diags.iter().map(|e| e.render(file, src)));
    if let Some(e) = fatal {
        errors.push(e.render(file, src));
    }
    let mut warnings: Vec<String> = it.warnings.iter().map(|e| e.render_as("warning", file, src)).collect();
    if let Some(set) = &opts.schemas {
        for e in set.load_errors() {
            errors.push(format!("error[K0713]: cannot read a cached schema: {}\n", e));
        }
    }
    for source in &it.unchecked {
        let e = unchecked_provider(source);
        if opts.strict {
            errors.push(e.render(file, src));
        } else {
            warnings.push(e.render_as("warning", file, src));
        }
    }
    if !errors.is_empty() {
        let n = errors.len();
        let mut msg = warnings.join("\n");
        if !msg.is_empty() {
            msg.push('\n');
        }
        msg.push_str(&errors.join("\n"));
        msg.push_str(&format!("\nerror: {} problem{} found; nothing was written\n", n, if n == 1 { "" } else { "s" }));
        return Err(msg);
    }
    let old = opts.lock.map(ledger::Ledger::parse).transpose()?;
    let (ledger, new_moves) =
        ledger::reconcile(old.as_ref(), &it.graph, opts.old_output, opts.accept_replace)
            .map_err(|m| format!("{}  --> {}\n", m, file))?;
    let type_clashes = render_clashes(&ty, file, src);
    Ok(Build { json: emit::emit(&it, file, &ledger.moved), lock: ledger.render(), new_moves, warnings, type_clashes })
}

fn render_clashes(ty: &infer::Infer, file: &str, src: &str) -> Vec<String> {
    ty.clashes
        .iter()
        .map(|c| KilnError { code: "T", msg: c.msg.clone(), pos: c.pos, help: None }.render_as("type", file, src))
        .collect()
}

/// What the type checker infers for a stack: the type of each top-level
/// definition, and the clashes it would report.
pub struct TypeReport {
    pub defs: Vec<(String, String)>,
    pub clashes: Vec<String>,
}

pub fn types(src: &str, file: &str, schemas: Option<std::rc::Rc<schema::SchemaSet>>) -> Result<TypeReport, String> {
    let mut it = eval::Interp::new().map_err(|e| e.render(file, src))?;
    it.schemas = schemas;
    let mut ty = infer::Infer::new();
    let forms = reader::read_all(src).map_err(|e| e.render(file, src))?;
    for form in forms {
        ty.toplevel(&mut it, &form);
        // Evaluation keeps macros and providers current for the next form;
        // its own errors are not the subject here.
        if it.eval_toplevel(&form).is_err() {
            break;
        }
    }
    let defs = ty.defs.iter().map(|(n, t)| (n.clone(), ty.show(t))).collect();
    Ok(TypeReport { defs, clashes: render_clashes(&ty, file, src) })
}

/// The providers a stack declares in its `ns` form: (name, source, version constraint).
pub fn providers_of(src: &str, file: &str) -> Result<Vec<(String, String, Option<String>)>, String> {
    run(src, file, || {
        let mut it = eval::Interp::new()?;
        for form in reader::read_all(src)? {
            if let Value::List(items, _) = &form {
                if items.first().and_then(|h| h.sym_name()) == Some("ns") {
                    it.eval_toplevel(&form)?;
                }
            }
        }
        Ok(it
            .providers
            .iter()
            .map(|(n, p)| (n.clone(), p.source.clone(), p.version.clone()))
            .collect())
    })
}

/// Compile one stack with no ledger to the text of its `main.tf.json`.
pub fn compile(src: &str, file: &str) -> Result<String, String> {
    build(src, file, &BuildOpts::default()).map(|b| b.json)
}

/// The program with every macro call expanded.
pub fn expand(src: &str, file: &str) -> Result<String, String> {
    run(src, file, || {
        let mut it = eval::Interp::new()?;
        let mut out = String::new();
        for form in reader::read_all(src)? {
            let ex = it.expand_all(&form, 0)?;
            out.push_str(&pp(&ex, 0));
            out.push_str("\n\n");
            it.eval_toplevel(&form)?;
        }
        Ok(out)
    })
}

/// Evaluate a program and print its last value. For tests and exploration.
pub fn eval_last(src: &str) -> Result<String, String> {
    run(src, "<input>", || {
        let mut it = eval::Interp::new()?;
        let mut last = Value::Nil;
        for form in reader::read_all(src)? {
            last = it.eval_toplevel(&form)?;
        }
        Ok(pr_str(&last))
    })
}
