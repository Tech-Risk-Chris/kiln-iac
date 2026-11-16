//! Kiln: a Lisp that compiles to Terraform JSON.

pub mod builtins;
pub mod emit;
pub mod eval;
pub mod reader;
pub mod tf;
pub mod value;

use value::{pp, pr_str, KilnError, Value};

fn run<T>(src: &str, file: &str, f: impl FnOnce() -> Result<T, KilnError>) -> Result<T, String> {
    f().map_err(|e| e.render(file, src))
}

/// Compile one stack to the text of its `main.tf.json`.
pub fn compile(src: &str, file: &str) -> Result<String, String> {
    run(src, file, || {
        let mut it = eval::Interp::new()?;
        for form in reader::read_all(src)? {
            it.eval_toplevel(&form)?;
        }
        Ok(emit::emit(&it, file))
    })
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
