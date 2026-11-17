//! M1: the address ledger and `kiln import`.

use kiln::{build, Build, BuildOpts};
use serde_json::Value as J;

const NS: &str = "(ns t (:provider [random :as r :version \"~> 3.6\"]))\n";

fn b(body: &str, lock: Option<&str>) -> Result<Build, String> {
    let src = format!("{}{}", NS, body);
    build(&src, "t.kiln", &BuildOpts { lock, ..BuildOpts::default() })
}

fn ok(body: &str, lock: Option<&str>) -> Build {
    b(body, lock).unwrap_or_else(|e| panic!("{}", e))
}

fn moved(build: &Build) -> Vec<(String, String)> {
    let j: J = serde_json::from_str(&build.json).unwrap();
    j["moved"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| (m["from"].as_str().unwrap().to_string(), m["to"].as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

const COMPONENT: &str = "(defcomponent names [] {:pet (r/pet \"pet\" {}) :id (r/id \"id\" {:byte_length 4})})\n";

// ------------------------------------------------------------------ ledger

#[test]
fn first_build_records_addresses_and_moves_nothing() {
    let v1 = ok("(r/pet \"a\" {})", None);
    assert!(v1.lock.contains("\"random_pet.a\""));
    assert!(moved(&v1).is_empty());
    // A second build with that lock is a fixpoint.
    let again = ok("(r/pet \"a\" {})", Some(&v1.lock));
    assert_eq!(again.lock, v1.lock);
    assert_eq!(again.json, v1.json);
}

#[test]
fn declared_rename_becomes_a_moved_block() {
    let v1 = ok("(r/pet \"a\" {})", None);
    let v2 = ok("(r/pet ^{:was \"a\"} \"b\" {})", Some(&v1.lock));
    assert_eq!(v2.new_moves, vec![pair("random_pet.a", "random_pet.b")]);
    assert_eq!(moved(&v2), vec![pair("random_pet.a", "random_pet.b")]);
    // The annotation can stay or go; the move is kept by the ledger either way.
    let v3 = ok("(r/pet \"b\" {})", Some(&v2.lock));
    assert!(v3.new_moves.is_empty());
    assert_eq!(moved(&v3), vec![pair("random_pet.a", "random_pet.b")]);
    let v3b = ok("(r/pet ^{:was \"a\"} \"b\" {})", Some(&v2.lock));
    assert_eq!(v3b.json, v3.json);
}

#[test]
fn renaming_a_component_instance_moves_everything_inside() {
    let v1 = ok(&format!("{}(names \"alpha\")", COMPONENT), None);
    let v2 = ok(&format!("{}(names ^{{:was \"alpha\"}} \"beta\")", COMPONENT), Some(&v1.lock));
    assert_eq!(
        moved(&v2),
        vec![pair("random_id.alpha__id", "random_id.beta__id"), pair("random_pet.alpha__pet", "random_pet.beta__pet")]
    );
}

#[test]
fn renames_at_two_levels_made_at_different_times() {
    let comp = |inner: &str| format!("(defcomponent c [] (r/pet {} {{}}))\n", inner);
    let v1 = ok(&format!("{}(c \"outer\")", comp("\"x\"")), None);
    // First the instance is renamed...
    let v2 = ok(&format!("{}(c ^{{:was \"outer\"}} \"shell\")", comp("\"x\"")), Some(&v1.lock));
    assert_eq!(v2.new_moves, vec![pair("random_pet.outer__x", "random_pet.shell__x")]);
    // ...later the resource inside it, with the old annotation still in place.
    let v3 = ok(&format!("{}(c ^{{:was \"outer\"}} \"shell\")", comp("^{:was \"x\"} \"y\"")), Some(&v2.lock));
    assert_eq!(v3.new_moves, vec![pair("random_pet.shell__x", "random_pet.shell__y")]);
    assert_eq!(moved(&v3).len(), 2);
}

#[test]
fn undeclared_rename_stops_the_build() {
    let v1 = ok(&format!("{}(names \"alpha\")\n(r/integer \"keep\" {{:min 1 :max 2}})", COMPONENT), None);
    let e = b(&format!("{}(names \"beta\")\n(r/integer \"keep\" {{:min 1 :max 2}})", COMPONENT), Some(&v1.lock))
        .err()
        .expect("must fail");
    assert!(e.contains("error[K0500]: 2 addresses"), "{}", e);
    assert!(e.contains("random_pet.alpha__pet  ->  random_pet.beta__pet"), "{}", e);
    assert!(e.contains("--replace"), "{}", e);
}

#[test]
fn proposals_pair_by_argument_similarity() {
    let v1_src = format!("{}(r/pet \"a\" {{:length 2}})\n(r/pet \"b\" {{:length 9}})", NS);
    let v1 = build(&v1_src, "t.kiln", &BuildOpts::default()).unwrap();
    let v2_src = format!("{}(r/pet \"x\" {{:length 9}})\n(r/pet \"y\" {{:length 2}})", NS);
    let e = build(
        &v2_src,
        "t.kiln",
        &BuildOpts { lock: Some(&v1.lock), old_output: Some(&v1.json), ..BuildOpts::default() },
    )
    .err()
    .unwrap();
    assert!(e.contains("random_pet.a  ->  random_pet.y   (100% alike)"), "{}", e);
    assert!(e.contains("random_pet.b  ->  random_pet.x   (100% alike)"), "{}", e);
}

#[test]
fn replace_accepts_delete_and_create() {
    let v1 = ok("(r/pet \"a\" {})", None);
    let src = format!("{}(r/pet \"b\" {{}})", NS);
    let v2 = build(&src, "t.kiln", &BuildOpts { lock: Some(&v1.lock), accept_replace: true, ..BuildOpts::default() }).unwrap();
    assert!(moved(&v2).is_empty());
    assert!(v2.lock.contains("random_pet.b") && !v2.lock.contains("random_pet.a"));
}

#[test]
fn plain_additions_and_deletions_pass() {
    let v1 = ok("(r/pet \"a\" {})\n(r/id \"i\" {:byte_length 2})", None);
    let v2 = ok("(r/pet \"a\" {})\n(r/integer \"n\" {:min 1 :max 2})", Some(&v1.lock));
    assert!(moved(&v2).is_empty());
}

#[test]
fn kiln_mv_is_consumed_by_the_next_build() {
    let v1 = ok("(r/pet \"a\" {})", None);
    let mut ledger = kiln::ledger::Ledger::parse(&v1.lock).unwrap();
    ledger.pending.push(pair("random_pet.a", "random_pet.b"));
    let v2 = ok("(r/pet \"b\" {})", Some(&ledger.render()));
    assert_eq!(moved(&v2), vec![pair("random_pet.a", "random_pet.b")]);
    assert!(!v2.lock.contains("pending"));
    // A pending move that matches nothing is an error, not a silent no-op.
    let e = b("(r/pet \"a\" {})", Some(&ledger.render())).err().unwrap();
    assert!(e.contains("K0501"), "{}", e);
}

#[test]
fn renaming_back_does_not_create_a_cycle() {
    let v1 = ok("(r/pet \"a\" {})", None);
    let v2 = ok("(r/pet ^{:was \"a\"} \"b\" {})", Some(&v1.lock));
    let v3 = ok("(r/pet ^{:was \"b\"} \"a\" {})", Some(&v2.lock));
    assert_eq!(moved(&v3), vec![pair("random_pet.b", "random_pet.a")]);
}

#[test]
fn output_does_not_depend_on_the_invocation_path() {
    let src = format!("{}(r/pet \"a\" {{}})", NS);
    let a = build(&src, "main.kiln", &BuildOpts::default()).unwrap();
    let b = build(&src, "/some/where/else/main.kiln", &BuildOpts::default()).unwrap();
    assert_eq!(a.json, b.json);
}

// ------------------------------------------------------------------ import

fn read_dir(dir: &str) -> Vec<(String, String)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().map_or(false, |x| x == "tf"))
        .collect();
    files.sort();
    files.iter().map(|p| (p.to_string_lossy().to_string(), std::fs::read_to_string(p).unwrap())).collect()
}

fn import_str(hcl: &str) -> Result<String, String> {
    kiln::import::import(&[("main.tf".into(), hcl.into())], "test").map(|i| i.source)
}

fn import_json(hcl: &str) -> J {
    let src = import_str(hcl).unwrap_or_else(|e| panic!("{}", e));
    let out = kiln::compile(&src, "main.kiln").unwrap_or_else(|e| panic!("{}\n---\n{}", e, src));
    serde_json::from_str(&out).unwrap()
}

#[test]
fn imported_gke_equals_the_hand_written_kiln() {
    let imported = kiln::import::import(&read_dir("examples/gke/original"), "examples/gke/original").unwrap();
    let got: J = serde_json::from_str(&kiln::compile(&imported.source, "main.kiln").unwrap()).unwrap();
    let want: J = serde_json::from_str(&std::fs::read_to_string("examples/gke/out/main.tf.json").unwrap()).unwrap();
    assert_eq!(got, want);
}

#[test]
fn offline_fixture_imports_and_keeps_every_address() {
    let imported =
        kiln::import::import(&read_dir("examples/import-offline/original"), "examples/import-offline/original").unwrap();
    let b = build(&imported.source, "main.kiln", &BuildOpts::default()).unwrap_or_else(|e| panic!("{}", e));
    let ledger = kiln::ledger::Ledger::parse(&b.lock).unwrap();
    let got: Vec<&str> = ledger.addresses.iter().map(String::as_str).collect();
    assert_eq!(got, vec!["local_file.summary", "random_id.per_size", "random_integer.extra", "random_pet.base"]);
    let j: J = serde_json::from_str(&b.json).unwrap();
    assert_eq!(j["resource"]["random_pet"]["base"]["keepers"]["total"], 10);
    assert_eq!(j["output"]["small_id"]["value"], "${random_id.per_size[\"small\"].hex}");
    assert_eq!(j["resource"]["local_file"]["summary"]["depends_on"][0], "random_integer.extra");
    assert_eq!(j["resource"]["random_integer"]["extra"]["lifecycle"]["ignore_changes"][0], "keepers");
    let content = j["resource"]["local_file"]["summary"]["content"].as_str().unwrap();
    assert!(content.starts_with("name:  kiln-${random_pet.base.id}\nsizes: "), "{}", content);
    assert!(content.ends_with("cost:  $${not_interpolated}\n"), "{}", content);
}

#[test]
fn declarations_are_reordered_so_nothing_is_used_before_its_definition() {
    let j = import_json(
        r#"
        output "o" { value = local.b }
        locals { b = "${local.a}-${random_pet.p.id}" }
        resource "random_pet" "p" { prefix = local.a }
        locals { a = var.v }
        variable "v" { default = "x" }
        "#,
    );
    assert_eq!(j["output"]["o"]["value"], "x-${random_pet.p.id}");
    assert_eq!(j["resource"]["random_pet"]["p"]["prefix"], "x");
}

#[test]
fn repeated_blocks_operators_and_conditionals() {
    let j = import_json(
        r#"
        variable "n" { default = 3 }
        resource "random_pet" "p" {
          length = var.n * 2 - 1
          keepers = {
            big  = var.n >= 3 && var.n != 4
            pick = var.n > 2 ? "many" : "few"
            neg  = -var.n
          }
        }
        resource "google_compute_firewall" "f" {
          name    = "f"
          network = "default"
          allow {
            protocol = "tcp"
            ports    = ["80", "443"]
          }
          allow {
            protocol = "icmp"
          }
        }
        "#,
    );
    let p = &j["resource"]["random_pet"]["p"];
    assert_eq!(p["length"], 5);
    assert_eq!(p["keepers"]["big"], "${(true && true)}");
    assert_eq!(p["keepers"]["pick"], "${(true ? \"many\" : \"few\")}");
    assert_eq!(p["keepers"]["neg"], -3);
    let allow = j["resource"]["google_compute_firewall"]["f"]["allow"].as_array().unwrap();
    assert_eq!(allow.len(), 2);
    assert_eq!(allow[0]["ports"][1], "443");
}

#[test]
fn names_that_clash_with_kiln_are_renamed() {
    let src = import_str(
        r#"
        locals { name = "a" }
        locals { count = 2 }
        resource "random_pet" "name" { prefix = local.name }
        output "o" { value = "${local.count}-${random_pet.name.id}" }
        "#,
    )
    .unwrap();
    assert!(src.contains("(def local-name \"a\")"), "{}", src);
    assert!(src.contains("(def local-count 2)"), "{}", src);
    assert!(src.contains("(def random_pet-name"), "{}", src);
    let j: J = serde_json::from_str(&kiln::compile(&src, "main.kiln").unwrap()).unwrap();
    assert_eq!(j["output"]["o"]["value"], "2-${random_pet.name.id}");
}

#[test]
fn unsupported_constructs_are_listed_not_guessed() {
    let e = import_str(
        r#"
        variable "required" {}
        resource "random_pet" "p" {
          dynamic "x" {
            for_each = []
            content {}
          }
        }
        check "health" {}
        "#,
    )
    .unwrap_err();
    assert!(e.contains("error[K0600]: 3 constructs"), "{}", e);
    assert!(e.contains("var.required: a variable without a default"), "{}", e);
    assert!(e.contains("random_pet.p: a dynamic block"), "{}", e);
    assert!(e.contains("a `check` block"), "{}", e);
}
