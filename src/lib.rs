//! Kiln: a Lisp that compiles to Terraform JSON.

pub mod builtins;
pub mod emit;
pub mod eval;
pub mod import;
pub mod ledger;
pub mod reader;
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
}

#[derive(Default)]
pub struct BuildOpts<'a> {
    /// The current `kiln.lock`, if there is one.
    pub lock: Option<&'a str>,
    /// The previously generated `main.tf.json`; used only to rank rename proposals.
    pub old_output: Option<&'a str>,
    /// Accept disappearing and appearing addresses as real deletions and creations.
    pub accept_replace: bool,
}

/// Build one stack: evaluate, reconcile addresses against the ledger, emit.
/// A pure function of its arguments.
pub fn build(src: &str, file: &str, opts: &BuildOpts) -> Result<Build, String> {
    let it = run(src, file, || {
        let mut it = eval::Interp::new()?;
        for form in reader::read_all(src)? {
            it.eval_toplevel(&form)?;
        }
        Ok(it)
    })?;
    let old = opts.lock.map(ledger::Ledger::parse).transpose()?;
    let (ledger, new_moves) =
        ledger::reconcile(old.as_ref(), &it.graph, opts.old_output, opts.accept_replace)
            .map_err(|m| format!("{}  --> {}\n", m, file))?;
    Ok(Build { json: emit::emit(&it, file, &ledger.moved), lock: ledger.render(), new_moves })
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
