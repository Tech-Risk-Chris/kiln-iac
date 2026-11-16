use kiln::BuildOpts;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
kiln: a Lisp that compiles to Terraform JSON

USAGE:
    kiln build <file.kiln> [-o <dir>] [--check] [--replace]
    kiln import <dir> -o <dir> [--force]
    kiln mv <file.kiln> <from-address> <to-address>
    kiln expand <file.kiln>
    kiln --version

    build     Compile a stack to <dir>/main.tf.json (default: out/ beside the source)
              and update kiln.lock beside the source.
              --check    write nothing; fail if the committed output or lock is stale
              --replace  accept disappearing and appearing addresses as real
                         deletions and creations instead of stopping
    import    Translate the .tf files in a directory into <dir>/main.kiln, then build
              it. Addresses are preserved.
    mv        Declare that a resource was renamed; the next build emits a `moved` block.
    expand    Print the program with all macros expanded.
";

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {}", p.display(), e))
}

fn write(p: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    }
    std::fs::write(p, text).map_err(|e| format!("cannot write {}: {}", p.display(), e))
}

/// Forward slashes keep the generated header identical across platforms.
fn shown(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn lock_path(file: &Path) -> PathBuf {
    file.parent().unwrap_or(Path::new(".")).join("kiln.lock")
}

struct Args {
    positional: Vec<String>,
    out: Option<PathBuf>,
    check: bool,
    replace: bool,
    force: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args { positional: vec![], out: None, check: false, replace: false, force: false };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => a.check = true,
            "--replace" => a.replace = true,
            "--force" => a.force = true,
            "-o" | "--out" => {
                i += 1;
                a.out = Some(PathBuf::from(args.get(i).ok_or("-o needs a directory")?));
            }
            s if s.starts_with('-') => return Err(format!("unknown option {}\n\n{}", s, USAGE)),
            s => a.positional.push(s.to_string()),
        }
        i += 1;
    }
    Ok(a)
}

fn build(file: &Path, out: Option<PathBuf>, check: bool, replace: bool) -> Result<(), String> {
    let src = read(file)?;
    let dir = out.unwrap_or_else(|| file.parent().unwrap_or(Path::new(".")).join("out"));
    let target = dir.join("main.tf.json");
    let lockfile = lock_path(file);
    let old_lock = std::fs::read_to_string(&lockfile).ok();
    let old_out = std::fs::read_to_string(&target).ok();
    let b = kiln::build(
        &src,
        &shown(file),
        &BuildOpts { lock: old_lock.as_deref(), old_output: old_out.as_deref(), accept_replace: replace },
    )?;
    if check {
        let mut stale = vec![];
        if old_out.as_deref() != Some(b.json.as_str()) {
            stale.push(shown(&target));
        }
        if old_lock.as_deref() != Some(b.lock.as_str()) {
            stale.push(shown(&lockfile));
        }
        if !stale.is_empty() {
            return Err(format!(
                "stale: {}\nrun `kiln build {}` and commit the result",
                stale.join(", "),
                shown(file)
            ));
        }
        println!("{} and {} are up to date", shown(&target), shown(&lockfile));
        return Ok(());
    }
    write(&target, &b.json)?;
    write(&lockfile, &b.lock)?;
    for (from, to) in &b.new_moves {
        println!("moved {} -> {}", from, to);
    }
    println!("wrote {}", shown(&target));
    Ok(())
}

fn import(dir: &Path, out: Option<PathBuf>, force: bool) -> Result<(), String> {
    let out = out.ok_or("import needs a destination: kiln import <dir> -o <dir>")?;
    let target = out.join("main.kiln");
    if target.exists() && !force {
        return Err(format!("{} already exists; pass --force to overwrite it", shown(&target)));
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {}", dir.display(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |x| x == "tf"))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("no .tf files in {}", dir.display()));
    }
    let mut files = vec![];
    for p in &names {
        files.push((shown(p), read(p)?));
    }
    let imported = kiln::import::import(&files, &shown(dir))?;
    write(&target, &imported.source)?;
    println!("wrote {} from {} file{}", shown(&target), files.len(), if files.len() == 1 { "" } else { "s" });
    for n in &imported.notes {
        println!("note: {}", n);
    }
    if force {
        // A forced re-import starts a fresh ledger.
        let _ = std::fs::remove_file(lock_path(&target));
    }
    build(&target, None, false, false)
}

fn mv(file: &Path, from: &str, to: &str) -> Result<(), String> {
    let lockfile = lock_path(file);
    let text = read(&lockfile).map_err(|_| {
        format!("{} does not exist yet; build the stack once before declaring moves", shown(&lockfile))
    })?;
    let mut ledger = kiln::ledger::Ledger::parse(&text)?;
    if !ledger.addresses.contains(from) {
        return Err(format!("{} is not an address in {}", from, shown(&lockfile)));
    }
    if kiln::ledger::addr_type(from) != kiln::ledger::addr_type(to) {
        return Err("a move cannot change the resource type".into());
    }
    ledger.pending.push((from.to_string(), to.to_string()));
    write(&lockfile, &ledger.render())?;
    println!("recorded {} -> {}; the next build emits the moved block", from, to);
    Ok(())
}

fn real_main(args: Vec<String>) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if cmd == "--version" || cmd == "-V" {
        println!("kiln {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let a = parse_args(args.get(1..).unwrap_or(&[]))?;
    let p = |i: usize| a.positional.get(i).map(PathBuf::from);
    match (cmd, a.positional.len()) {
        ("build", 1) => build(&p(0).unwrap(), a.out, a.check, a.replace),
        ("expand", 1) => {
            let file = p(0).unwrap();
            print!("{}", kiln::expand(&read(&file)?, &shown(&file))?);
            Ok(())
        }
        ("import", 1) => import(&p(0).unwrap(), a.out, a.force),
        ("mv", 3) => mv(&p(0).unwrap(), &a.positional[1], &a.positional[2]),
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
