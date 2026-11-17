use kiln::schema::{self, SchemaSet};
use kiln::BuildOpts;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::process::ExitCode;

const USAGE: &str = "\
kiln: a Lisp that compiles to Terraform JSON

USAGE:
    kiln build <file.kiln> [-o <dir>] [--check] [--replace] [--strict]
    kiln schema <file.kiln> [--if-missing]
    kiln schema --from <schema.json> --lock <.terraform.lock.hcl> [-o <dir>]
    kiln import <dir> -o <dir> [--force]
    kiln mv <file.kiln> <from-address> <to-address>
    kiln expand <file.kiln>
    kiln --version

    build     Compile a stack to <dir>/main.tf.json (default: out/ beside the source)
              and update kiln.lock beside the source.
              --check    write nothing; fail if the committed output or lock is stale
              --replace  accept disappearing and appearing addresses as real
                         deletions and creations instead of stopping
              --strict   a provider without a cached schema is an error
    schema    Fetch the schemas of the providers a stack declares, using the engine
              ($TOFU, default `tofu`), into .kiln/schemas/. With --from, convert
              the output of `tofu providers schema -json` instead.
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
    from: Option<PathBuf>,
    lock: Option<PathBuf>,
    check: bool,
    replace: bool,
    force: bool,
    strict: bool,
    if_missing: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        positional: vec![],
        out: None,
        from: None,
        lock: None,
        check: false,
        replace: false,
        force: false,
        strict: false,
        if_missing: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => a.check = true,
            "--replace" => a.replace = true,
            "--force" => a.force = true,
            "--strict" => a.strict = true,
            "--if-missing" => a.if_missing = true,
            "--from" | "--lock" => {
                let flag = args[i].clone();
                i += 1;
                let v = PathBuf::from(args.get(i).ok_or(format!("{} needs a file", flag))?);
                if flag == "--from" {
                    a.from = Some(v);
                } else {
                    a.lock = Some(v);
                }
            }
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

fn dir_of(file: &Path) -> &Path {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn schemas_for(file: &Path) -> Rc<SchemaSet> {
    match schema::find_kiln_dir(dir_of(file)) {
        Some(d) => Rc::new(SchemaSet::from_dir(d.join("schemas"))),
        None => Rc::new(SchemaSet::empty()),
    }
}

fn build(file: &Path, out: Option<PathBuf>, check: bool, replace: bool, strict: bool) -> Result<(), String> {
    let src = read(file)?;
    let dir = out.unwrap_or_else(|| file.parent().unwrap_or(Path::new(".")).join("out"));
    let target = dir.join("main.tf.json");
    let lockfile = lock_path(file);
    let old_lock = std::fs::read_to_string(&lockfile).ok();
    let old_out = std::fs::read_to_string(&target).ok();
    let b = kiln::build(
        &src,
        &shown(file),
        &BuildOpts {
            lock: old_lock.as_deref(),
            old_output: old_out.as_deref(),
            accept_replace: replace,
            schemas: Some(schemas_for(file)),
            strict,
        },
    )?;
    for w in &b.warnings {
        eprint!("{}", w);
    }
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
    build(&target, None, false, false, false)
}

fn write_schemas(dir: &Path, schema_json: &str, lock_hcl: &str) -> Result<(), String> {
    let versions = schema::lock_versions(lock_hcl)?;
    for c in schema::convert(schema_json, &versions)? {
        let path = schema::cache_path(dir, &c.source);
        write(&path, &c.text)?;
        println!(
            "wrote {} ({} {}, {} resource types, {} data sources)",
            shown(&path),
            c.source,
            c.version,
            c.resources,
            c.data_sources
        );
    }
    Ok(())
}

fn engine(dir: &Path, args: &[&str]) -> Result<String, String> {
    let bin = std::env::var("TOFU").unwrap_or_else(|_| "tofu".into());
    let mut cmd = Command::new(&bin);
    cmd.arg(format!("-chdir={}", dir.display())).args(args).env("TF_IN_AUTOMATION", "1");
    let out = cmd.output().map_err(|e| format!("cannot run `{}`: {} (set TOFU to the engine binary)", bin, e))?;
    if !out.status.success() {
        return Err(format!(
            "`{} {}` failed:\n{}{}",
            bin,
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "the engine wrote invalid UTF-8".into())
}

fn schema_cmd(a: &Args) -> Result<(), String> {
    if let Some(from) = &a.from {
        let lock = a.lock.as_ref().ok_or("--from needs --lock <.terraform.lock.hcl> for the provider versions")?;
        let dir = a.out.clone().unwrap_or_else(|| PathBuf::from(".kiln/schemas"));
        return write_schemas(&dir, &read(from)?, &read(lock)?);
    }
    let file = match a.positional.first() {
        Some(f) => PathBuf::from(f),
        None => return Err(USAGE.to_string()),
    };
    let src = read(&file)?;
    let providers = kiln::providers_of(&src, &shown(&file))?;
    if providers.is_empty() {
        println!("{} declares no providers", shown(&file));
        return Ok(());
    }
    let kiln_dir = schema::default_kiln_dir(dir_of(&file));
    let cache = kiln_dir.join("schemas");
    if a.if_missing && providers.iter().all(|(_, s, _)| schema::cache_path(&cache, s).exists()) {
        println!("schemas for {} are cached", shown(&file));
        return Ok(());
    }
    // A configuration that only declares the providers: no resources, no backend.
    let mut rp = serde_json::Map::new();
    for (name, source, version) in &providers {
        let mut p = serde_json::Map::new();
        p.insert("source".into(), source.clone().into());
        if let Some(v) = version {
            p.insert("version".into(), v.clone().into());
        }
        rp.insert(name.clone(), serde_json::Value::Object(p));
    }
    let work = kiln_dir.join("work").join("schema");
    let _ = std::fs::remove_file(work.join(".terraform.lock.hcl"));
    let config = serde_json::json!({ "terraform": { "required_providers": rp } });
    write(&work.join("main.tf.json"), &serde_json::to_string_pretty(&config).unwrap())?;
    println!("fetching provider schemas with the engine in {}", shown(&work));
    engine(&work, &["init", "-backend=false", "-input=false", "-no-color", "-upgrade"])?;
    let schema_json = engine(&work, &["providers", "schema", "-json"])?;
    let lock = read(&work.join(".terraform.lock.hcl"))?;
    write_schemas(&cache, &schema_json, &lock)
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
        ("build", 1) => build(&p(0).unwrap(), a.out, a.check, a.replace, a.strict),
        ("schema", _) => schema_cmd(&a),
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
