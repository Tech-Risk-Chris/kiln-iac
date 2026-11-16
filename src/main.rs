use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
kiln: a Lisp that compiles to Terraform JSON

USAGE:
    kiln build <file.kiln> [-o <dir>] [--check]
    kiln expand <file.kiln>
    kiln --version

    build     Compile a stack to <dir>/main.tf.json (default: out/ beside the source).
              With --check, write nothing and fail if the committed output is stale.
    expand    Print the program with all macros expanded.
";

fn real_main(args: Vec<String>) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "--version" | "-V" => {
            println!("kiln {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "build" | "expand" => {
            let mut file: Option<PathBuf> = None;
            let mut out: Option<PathBuf> = None;
            let mut check = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--check" => check = true,
                    "-o" | "--out" => {
                        i += 1;
                        out = Some(PathBuf::from(args.get(i).ok_or("-o needs a directory")?));
                    }
                    a if a.starts_with('-') => return Err(format!("unknown option {}\n\n{}", a, USAGE)),
                    a => file = Some(PathBuf::from(a)),
                }
                i += 1;
            }
            let file = file.ok_or_else(|| format!("no input file\n\n{}", USAGE))?;
            let src = std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read {}: {}", file.display(), e))?;
            // Forward slashes keep the generated header identical across platforms.
            let shown = file.to_string_lossy().replace('\\', "/");
            if cmd == "expand" {
                print!("{}", kiln::expand(&src, &shown)?);
                return Ok(());
            }
            let json = kiln::compile(&src, &shown)?;
            let dir = out.unwrap_or_else(|| file.parent().unwrap_or(Path::new(".")).join("out"));
            let target = dir.join("main.tf.json");
            if check {
                let existing = std::fs::read_to_string(&target).unwrap_or_default();
                if existing != json {
                    return Err(format!(
                        "{} is stale: run `kiln build {}` and commit the result",
                        target.display(),
                        shown
                    ));
                }
                println!("{} is up to date", target.display());
            } else {
                std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
                std::fs::write(&target, json).map_err(|e| format!("cannot write {}: {}", target.display(), e))?;
                println!("wrote {}", target.display());
            }
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The evaluator is recursive over the program's nesting; give it room.
    let worker = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || real_main(args))
        .expect("spawn worker");
    match worker.join() {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(msg)) => {
            eprint!("{}", msg);
            if !msg.ends_with('\n') {
                eprintln!();
            }
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}
