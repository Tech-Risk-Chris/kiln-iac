//! Provider schemas: the cache under `.kiln/schemas/`, conversion from
//! `tofu providers schema -json`, and version constraints.
//!
//! A cache file holds one provider at one version, stripped of descriptions.
//! It is committed with the code, so a build never needs the engine or the
//! network: the same source and the same cache give the same verdict.

use serde_json::{json, Map as JMap, Value as J};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const FORMAT: &str = "kiln-schema/1";

pub struct ProviderSchema {
    pub source: String,
    pub version: String,
    pub provider: Rc<J>,
    pub resources: HashMap<String, Rc<J>>,
    pub data_sources: HashMap<String, Rc<J>>,
}

impl ProviderSchema {
    pub fn parse(text: &str) -> Result<ProviderSchema, String> {
        let j: J = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {}", e))?;
        if j["format"] != json!(FORMAT) {
            return Err(format!("unsupported format (this kiln reads {})", FORMAT));
        }
        let table = |key: &str| -> HashMap<String, Rc<J>> {
            j[key]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), Rc::new(v.clone()))).collect())
                .unwrap_or_default()
        };
        Ok(ProviderSchema {
            source: j["source"].as_str().ok_or("missing \"source\"")?.to_string(),
            version: j["version"].as_str().ok_or("missing \"version\"")?.to_string(),
            provider: Rc::new(j["provider"].clone()),
            resources: table("resources"),
            data_sources: table("data_sources"),
        })
    }
}

/// The schemas available to a build. Files are read on first use.
pub struct SchemaSet {
    dir: Option<PathBuf>,
    loaded: RefCell<HashMap<String, Option<Rc<ProviderSchema>>>>,
    modules: RefCell<HashMap<String, Option<Rc<ModuleInterface>>>>,
    errors: RefCell<Vec<String>>,
}

impl SchemaSet {
    pub fn empty() -> SchemaSet {
        SchemaSet {
            dir: None,
            loaded: RefCell::new(HashMap::new()),
            modules: RefCell::new(HashMap::new()),
            errors: RefCell::new(vec![]),
        }
    }

    pub fn from_dir(dir: PathBuf) -> SchemaSet {
        SchemaSet { dir: Some(dir), ..SchemaSet::empty() }
    }

    /// For tests and tools: schemas given as cache-file texts.
    pub fn from_texts(texts: &[&str]) -> Result<SchemaSet, String> {
        let set = SchemaSet::empty();
        for t in texts {
            let p = ProviderSchema::parse(t)?;
            set.loaded.borrow_mut().insert(p.source.clone(), Some(Rc::new(p)));
        }
        Ok(set)
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn get(&self, source: &str) -> Option<Rc<ProviderSchema>> {
        let source = normalize_source(source);
        if let Some(v) = self.loaded.borrow().get(&source) {
            return v.clone();
        }
        let found = self.dir.as_ref().and_then(|d| {
            let path = cache_path(d, &source);
            let text = std::fs::read_to_string(&path).ok()?;
            match ProviderSchema::parse(&text) {
                Ok(p) => Some(Rc::new(p)),
                Err(e) => {
                    self.errors.borrow_mut().push(format!("{}: {}", path.display(), e));
                    None
                }
            }
        });
        self.loaded.borrow_mut().insert(source, found.clone());
        found
    }

    pub fn load_errors(&self) -> Vec<String> {
        self.errors.borrow().clone()
    }
}

/// `registry.opentofu.org/hashicorp/google` and `hashicorp/google` are the same provider.
pub fn normalize_source(s: &str) -> String {
    for host in ["registry.opentofu.org/", "registry.terraform.io/"] {
        if let Some(rest) = s.strip_prefix(host) {
            return rest.to_string();
        }
    }
    s.to_string()
}

pub fn cache_path(dir: &Path, source: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for part in normalize_source(source).split('/') {
        p.push(part);
    }
    p.set_extension("json");
    p
}

/// The `.kiln` directory that applies to a source file: the nearest one in
/// the file's directory or above it.
pub fn find_kiln_dir(start: &Path) -> Option<PathBuf> {
    let start = if start.as_os_str().is_empty() { Path::new(".") } else { start };
    let mut cur = std::fs::canonicalize(start).ok()?;
    loop {
        let candidate = cur.join(".kiln");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Where a new `.kiln` directory should go: the repository root if there is
/// one, otherwise the source file's directory.
pub fn default_kiln_dir(start: &Path) -> PathBuf {
    if let Some(d) = find_kiln_dir(start) {
        return d;
    }
    let start = if start.as_os_str().is_empty() { Path::new(".") } else { start };
    if let Ok(mut cur) = std::fs::canonicalize(start) {
        let base = cur.clone();
        loop {
            if cur.join(".git").exists() {
                return cur.join(".kiln");
            }
            if !cur.pop() {
                return base.join(".kiln");
            }
        }
    }
    start.join(".kiln")
}

// ---------------------------------------------------------------- conversion

fn strip_block(b: &J) -> J {
    let mut out = JMap::new();
    if let Some(attrs) = b["attributes"].as_object() {
        let mut m = JMap::new();
        for (name, a) in attrs {
            let mut x = JMap::new();
            for k in ["type", "required", "optional", "computed", "sensitive", "deprecated", "write_only"] {
                if let Some(v) = a.get(k) {
                    if *v != J::Bool(false) {
                        x.insert(k.into(), v.clone());
                    }
                }
            }
            if let Some(nt) = a.get("nested_type") {
                let mut y = strip_block(nt);
                if let J::Object(o) = &mut y {
                    for k in ["nesting_mode", "min_items", "max_items"] {
                        if let Some(v) = nt.get(k) {
                            o.insert(k.into(), v.clone());
                        }
                    }
                }
                x.insert("nested_type".into(), y);
            }
            m.insert(name.clone(), J::Object(x));
        }
        if !m.is_empty() {
            out.insert("attributes".into(), J::Object(m));
        }
    }
    if let Some(blocks) = b["block_types"].as_object() {
        let mut m = JMap::new();
        for (name, bt) in blocks {
            let mut y = JMap::new();
            for k in ["nesting_mode", "min_items", "max_items"] {
                if let Some(v) = bt.get(k) {
                    if *v != json!(0) {
                        y.insert(k.into(), v.clone());
                    }
                }
            }
            y.insert("block".into(), strip_block(&bt["block"]));
            m.insert(name.clone(), J::Object(y));
        }
        if !m.is_empty() {
            out.insert("block_types".into(), J::Object(m));
        }
    }
    if b["deprecated"] == J::Bool(true) {
        out.insert("deprecated".into(), J::Bool(true));
    }
    J::Object(out)
}

/// One line per resource type: small diffs when a provider is upgraded.
fn render_cache(source: &str, version: &str, provider: &J, resources: &JMap<String, J>, data: &JMap<String, J>) -> String {
    let line = |v: &J| serde_json::to_string(v).expect("JSON serialisation");
    let table = |m: &JMap<String, J>| -> String {
        if m.is_empty() {
            return "{}".into();
        }
        let mut keys: Vec<&String> = m.keys().collect();
        keys.sort();
        let rows: Vec<String> =
            keys.iter().map(|k| format!("    {}: {}", line(&J::String((*k).clone())), line(&m[*k]))).collect();
        format!("{{\n{}\n  }}", rows.join(",\n"))
    };
    format!(
        "{{\n  \"format\": {},\n  \"source\": {},\n  \"version\": {},\n  \"provider\": {},\n  \"resources\": {},\n  \"data_sources\": {}\n}}\n",
        line(&json!(FORMAT)),
        line(&json!(source)),
        line(&json!(version)),
        line(provider),
        table(resources),
        table(data)
    )
}

pub struct Converted {
    pub source: String,
    pub version: String,
    pub resources: usize,
    pub data_sources: usize,
    pub text: String,
}

/// Provider versions from a `.terraform.lock.hcl`.
pub fn lock_versions(lock_hcl: &str) -> Result<HashMap<String, String>, String> {
    let body = hcl::parse(lock_hcl).map_err(|e| format!(".terraform.lock.hcl: {}", e))?;
    let mut out = HashMap::new();
    for b in body.blocks() {
        if b.identifier.as_str() != "provider" {
            continue;
        }
        let source = match b.labels.first() {
            Some(l) => normalize_source(l.as_str()),
            None => continue,
        };
        for a in b.body.attributes() {
            if a.key.as_str() == "version" {
                if let hcl::Expression::String(v) = &a.expr {
                    out.insert(source.clone(), v.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Convert `tofu providers schema -json` output into cache files, one per provider.
pub fn convert(schema_json: &str, versions: &HashMap<String, String>) -> Result<Vec<Converted>, String> {
    let j: J = serde_json::from_str(schema_json).map_err(|e| format!("provider schema is not valid JSON: {}", e))?;
    let providers = j["provider_schemas"].as_object().ok_or("no \"provider_schemas\" in the provider schema")?;
    let mut out = vec![];
    for (full, ps) in providers {
        let source = normalize_source(full);
        let version = match versions.get(&source) {
            Some(v) => v.clone(),
            // Built-in providers are not in the lock file; they version with the engine.
            None if source.starts_with("terraform.io/builtin/") => "0.0.0".into(),
            None => return Err(format!("no version for {} in .terraform.lock.hcl", source)),
        };
        let strip_table = |key: &str| -> JMap<String, J> {
            ps[key]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), strip_block(&v["block"]))).collect())
                .unwrap_or_default()
        };
        let resources = strip_table("resource_schemas");
        let data = strip_table("data_source_schemas");
        let provider = strip_block(&ps["provider"]["block"]);
        out.push(Converted {
            text: render_cache(&source, &version, &provider, &resources, &data),
            resources: resources.len(),
            data_sources: data.len(),
            source,
            version,
        });
    }
    out.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(out)
}

// ---------------------------------------------------------------- versions

fn parse_version(s: &str) -> Option<Vec<u64>> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    core.split('.').map(|p| p.parse().ok()).collect()
}

fn cmp(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

/// Does `version` satisfy a Terraform version constraint such as
/// `"~> 6.0"` or `">= 1.2, < 2.0"`?
pub fn satisfies(version: &str, constraint: &str) -> Result<bool, String> {
    let v = parse_version(version).ok_or_else(|| format!("invalid version `{}`", version))?;
    for part in constraint.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (op, rest) = ["~>", ">=", "<=", "!=", ">", "<", "="]
            .iter()
            .find_map(|op| part.strip_prefix(op).map(|r| (*op, r.trim())))
            .unwrap_or(("=", part));
        let c = parse_version(rest).ok_or_else(|| format!("invalid version constraint `{}`", part))?;
        use std::cmp::Ordering::*;
        let ok = match op {
            "=" => cmp(&v, &c) == Equal,
            "!=" => cmp(&v, &c) != Equal,
            ">" => cmp(&v, &c) == Greater,
            ">=" => cmp(&v, &c) != Less,
            "<" => cmp(&v, &c) == Less,
            "<=" => cmp(&v, &c) != Greater,
            _ => {
                // ~> 6.0 allows >= 6.0, < 7.0; ~> 6.1.2 allows >= 6.1.2, < 6.2.0; ~> 6 allows < 7.
                let mut upper = c.clone();
                if upper.len() == 1 {
                    upper[0] += 1;
                } else {
                    upper.pop();
                    *upper.last_mut().unwrap() += 1;
                }
                cmp(&v, &c) != Less && cmp(&v, &upper) == Less
            }
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------- module interfaces
//
// A module's interface is its variables and outputs, read from its .tf files.
// It is cached like a provider schema, under `.kiln/modules/`, in the shape of
// a schema block so that the same checks apply: the inputs as a block whose
// attributes are the variables, the outputs as a block of computed attributes.

pub const MODULE_FORMAT: &str = "kiln-module/1";

pub struct ModuleInterface {
    pub key: String,
    pub version: String,
    pub inputs: Rc<J>,
    pub outputs: Rc<J>,
}

impl ModuleInterface {
    pub fn parse(text: &str) -> Result<ModuleInterface, String> {
        let j: J = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {}", e))?;
        if j["format"] != json!(MODULE_FORMAT) {
            return Err(format!("unsupported format (this kiln reads {})", MODULE_FORMAT));
        }
        Ok(ModuleInterface {
            key: j["key"].as_str().ok_or("missing \"key\"")?.to_string(),
            version: j["version"].as_str().unwrap_or("").to_string(),
            inputs: Rc::new(j["inputs"].clone()),
            outputs: Rc::new(j["outputs"].clone()),
        })
    }
}

/// The cache key of a module source. Registry sources lose their host; local
/// paths become paths relative to the directory that holds `.kiln`, so that
/// stacks in different directories agree on them.
pub fn module_key(source: &str, stack_dir: &Path, kiln_dir: Option<&Path>) -> String {
    if source.starts_with("./") || source.starts_with("../") {
        let abs = std::fs::canonicalize(stack_dir.join(source)).unwrap_or_else(|_| stack_dir.join(source));
        let rel = kiln_dir
            .and_then(|k| k.parent())
            .map(|root| if root.as_os_str().is_empty() { Path::new(".") } else { root })
            .and_then(|root| std::fs::canonicalize(root).ok())
            .and_then(|root| abs.strip_prefix(&root).ok().map(|p| p.to_path_buf()))
            .unwrap_or(abs);
        return format!("local:{}", rel.to_string_lossy().replace('\\', "/"));
    }
    normalize_source(source)
}

/// A local module source, given relative to the stack, made relative to the
/// output directory, where Terraform resolves it. Without an output directory,
/// `out/` in the stack directory.
pub fn rebase_source(source: &str, stack_dir: &Path, out_dir: Option<&Path>) -> String {
    use std::path::Component;
    fn absolute(p: &Path) -> Vec<String> {
        let p = std::path::absolute(if p.as_os_str().is_empty() { Path::new(".") } else { p }).unwrap_or_else(|_| p.to_path_buf());
        let mut parts: Vec<String> = vec![];
        for c in p.components() {
            match c {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(x) => parts.push(x.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) => parts.clear(),
            }
        }
        parts
    }
    let out = match out_dir {
        Some(o) => absolute(o),
        None => absolute(&stack_dir.join("out")),
    };
    let target = absolute(&stack_dir.join(source));
    let common = out.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut rel: Vec<String> = std::iter::repeat("..".to_string()).take(out.len() - common).collect();
    rel.extend(target[common..].iter().cloned());
    let joined = rel.join("/");
    if joined.starts_with("..") { joined } else if joined.is_empty() { ".".into() } else { format!("./{}", joined) }
}

pub fn module_cache_path(modules_dir: &Path, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    modules_dir.join(format!("{}.json", name))
}

impl SchemaSet {
    /// The cached interface of a module, by key.
    pub fn module(&self, key: &str) -> Option<Rc<ModuleInterface>> {
        if let Some(m) = self.modules.borrow().get(key) {
            return m.clone();
        }
        let found = self.dir.as_ref().and_then(|d| {
            let path = module_cache_path(&d.parent()?.join("modules"), key);
            let text = std::fs::read_to_string(&path).ok()?;
            match ModuleInterface::parse(&text) {
                Ok(m) => Some(Rc::new(m)),
                Err(e) => {
                    self.errors.borrow_mut().push(format!("{}: {}", path.display(), e));
                    None
                }
            }
        });
        self.modules.borrow_mut().insert(key.to_string(), found.clone());
        found
    }

    /// For tests and tools: add module interfaces given as cache-file texts.
    pub fn add_module_texts(&self, texts: &[&str]) -> Result<(), String> {
        for t in texts {
            let m = ModuleInterface::parse(t)?;
            self.modules.borrow_mut().insert(m.key.clone(), Some(Rc::new(m)));
        }
        Ok(())
    }
}

/// An HCL type constraint, as the JSON type notation provider schemas use.
pub fn type_of_expr(e: &hcl::Expression) -> J {
    use hcl::Expression as E;
    match e {
        E::Variable(v) => match v.as_str() {
            "string" | "number" | "bool" => json!(v.as_str()),
            _ => json!("dynamic"), // any
        },
        E::FuncCall(f) => {
            let name = f.name.name.as_str();
            let arg = f.args.first();
            match (name, arg) {
                ("list" | "set" | "map", Some(a)) => json!([name, type_of_expr(a)]),
                ("optional", Some(a)) => type_of_expr(a),
                ("tuple", Some(E::Array(items))) => json!(["tuple", items.iter().map(type_of_expr).collect::<Vec<_>>()]),
                ("object", Some(E::Object(obj))) => {
                    let mut attrs = JMap::new();
                    let mut optional = vec![];
                    for (k, v) in obj.iter() {
                        let key = k.to_string().trim_matches('"').to_string();
                        if let E::FuncCall(g) = v {
                            if g.name.name.as_str() == "optional" {
                                optional.push(json!(key.clone()));
                            }
                        }
                        attrs.insert(key, type_of_expr(v));
                    }
                    if optional.is_empty() {
                        json!(["object", attrs])
                    } else {
                        json!(["object", attrs, optional])
                    }
                }
                _ => json!("dynamic"),
            }
        }
        _ => json!("dynamic"),
    }
}

/// Read a module's interface from the .tf files in its directory.
pub fn module_interface_from_dir(dir: &Path) -> Result<(J, J), String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read module {}: {}", dir.display(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |x| x == "tf"))
        .collect();
    names.sort();
    let mut vars = JMap::new();
    let mut outs = JMap::new();
    for p in names {
        let text = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {}", p.display(), e))?;
        let body = hcl::parse(&text).map_err(|e| format!("{}: {}", p.display(), e))?;
        for b in body.blocks() {
            let label = match b.labels.first() {
                Some(l) => l.as_str().to_string(),
                None => continue,
            };
            let attr = |k: &str| b.body.attributes().find(|a| a.key.as_str() == k).map(|a| a.expr.clone());
            let is_true = |k: &str| matches!(attr(k), Some(hcl::Expression::Bool(true)));
            match b.identifier.as_str() {
                "variable" => {
                    let mut v = JMap::new();
                    v.insert("type".into(), attr("type").map(|t| type_of_expr(&t)).unwrap_or(json!("dynamic")));
                    if attr("default").is_some() {
                        v.insert("optional".into(), json!(true));
                    } else {
                        v.insert("required".into(), json!(true));
                    }
                    if is_true("sensitive") {
                        v.insert("sensitive".into(), json!(true));
                    }
                    vars.insert(label, J::Object(v));
                }
                "output" => {
                    let mut o = JMap::new();
                    o.insert("type".into(), json!("dynamic"));
                    o.insert("computed".into(), json!(true));
                    if is_true("sensitive") {
                        o.insert("sensitive".into(), json!(true));
                    }
                    outs.insert(label, J::Object(o));
                }
                _ => {}
            }
        }
    }
    Ok((json!({ "attributes": vars }), json!({ "attributes": outs })))
}

pub fn render_module_cache(key: &str, version: &str, inputs: &J, outputs: &J) -> String {
    let mut s = serde_json::to_string_pretty(&json!({
        "format": MODULE_FORMAT,
        "key": key,
        "version": version,
        "inputs": inputs,
        "outputs": outputs,
    }))
    .expect("JSON serialisation");
    s.push('\n');
    s
}

/// The modules a `.terraform/modules/modules.json` lists: key, source, version, directory.
pub fn installed_modules(modules_json: &str) -> Result<Vec<(String, String, String, String)>, String> {
    let j: J = serde_json::from_str(modules_json).map_err(|e| format!("modules.json: {}", e))?;
    let mut out = vec![];
    for m in j["Modules"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let key = m["Key"].as_str().unwrap_or("");
        if key.is_empty() || key.contains('.') {
            continue; // the root module, and modules nested inside modules
        }
        out.push((
            key.to_string(),
            m["Source"].as_str().unwrap_or("").to_string(),
            m["Version"].as_str().unwrap_or("").to_string(),
            m["Dir"].as_str().unwrap_or("").to_string(),
        ));
    }
    Ok(out)
}
