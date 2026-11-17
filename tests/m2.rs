//! M2: declarations are checked against provider schemas before Terraform runs.
//! Uses the committed Google schema in .kiln/schemas/.

use kiln::schema::{self, SchemaSet};
use kiln::{build, Build, BuildOpts};
use std::rc::Rc;

fn google() -> Rc<SchemaSet> {
    Rc::new(SchemaSet::from_dir(".kiln/schemas".into()))
}

fn with_ns(ns: &str, body: &str) -> Result<Build, String> {
    let src = format!("{}\n{}\n", ns, body);
    build(&src, "t.kiln", &BuildOpts { schemas: Some(google()), ..BuildOpts::default() })
}

const G: &str = "(ns t (:provider [google :as g :version \"~> 6.0\"]))";

fn ok(body: &str) -> Build {
    with_ns(G, body).unwrap_or_else(|e| panic!("{}", e))
}

fn err(body: &str) -> String {
    match with_ns(G, body) {
        Ok(_) => panic!("expected an error for:\n{}", body),
        Err(e) => e,
    }
}

const NET: &str = "(def vpc (g/compute_network \"vpc\" {:name \"vpc\" :auto_create_subnetworks false}))\n";

/// The seeded corpus: each entry is a mistake, the code it must produce, and
/// text the message must contain. Every one is caught before Terraform runs.
const CORPUS: &[(&str, &str, &str)] = &[
    // arguments
    (r#"(g/compute_network "n" {:name "n" :descripton "x"})"#, "K0701", "did you mean `description`?"),
    (r#"(g/compute_network "n" {:auto_create_subnetworks false})"#, "K0702", "missing required argument `name`"),
    (r#"(g/compute_network "n" {:name "n" :self_link "x"})"#, "K0703", "`self_link` is read-only"),
    // primitive types
    (r#"(g/compute_network "n" {:name "n" :mtu "1460"})"#, "K0704", "write 1460 without quotes"),
    (r#"(g/compute_network "n" {:name "n" :auto_create_subnetworks "true"})"#, "K0704", "must be a boolean"),
    (r#"(g/compute_network "n" {:name ["n"]})"#, "K0704", "`name` must be a string, got a vector"),
    // collection types
    (
        r#"(g/container_node_pool "p" {:cluster "c" :node_config {:oauth_scopes "x"}})"#,
        "K0704",
        "`node_config.oauth_scopes` must be a set(string)",
    ),
    (
        r#"(g/container_cluster "c" {:name "c" :resource_labels {:team ["a"]}})"#,
        "K0704",
        "`resource_labels[\"team\"]` must be a string, got a vector",
    ),
    // nested blocks
    (
        r#"(g/container_node_pool "p" {:cluster "c" :node_config {:machine_typ "e2"}})"#,
        "K0701",
        "did you mean `machine_type`?",
    ),
    (
        r#"(g/container_node_pool "p" {:cluster "c" :autoscaling [{:min_node_count 1} {:max_node_count 2}]})"#,
        "K0706",
        "`autoscaling` allows at most 1 block, got 2",
    ),
    (r#"(g/container_node_pool "p" {:cluster "c" :autoscaling 5})"#, "K0705", "is a nested block"),
    (
        r#"(g/container_node_pool "p" {:cluster "c" :autoscaling (tf/expr "var.x")})"#,
        "K0705",
        "cannot compute a block from an expression",
    ),
    (
        r#"(g/compute_firewall "f" {:name "f" :network "n" :allow {:ports ["80"]}})"#,
        "K0702",
        "missing required argument `allow.protocol`",
    ),
    // types and attributes
    (r#"(g/compute_netwrok "n" {:name "n"})"#, "K0700", "did you mean `google_compute_network`?"),
    (r#"(data g/client_confg "me" {})"#, "K0700", "has no data source `google_client_confg`"),
    (r#"(output "x" (:self_lnk vpc))"#, "K0707", "did you mean `self_link`?"),
    // providers and meta-arguments
    (r#"(provider g {:project "p" :regoin "europe-west3"})"#, "K0701", "did you mean `region`?"),
    (
        r#"(g/compute_network "n" {:name "n" :lifecycle {:ignore_change ["mtu"]}})"#,
        "K0701",
        "did you mean `ignore_changes`?",
    ),
    (r#"(g/compute_network "n" {:name "n" :count "2"})"#, "K0704", "`count` must be a whole number"),
    (
        r#"(g/compute_network "n" {:name "n" :count 1 :for_each {:a 1}})"#,
        "K0704",
        "`count` and `for_each` cannot be used together",
    ),
];

#[test]
fn every_mistake_in_the_corpus_is_caught() {
    let mut missed = vec![];
    for (snippet, code, text) in CORPUS {
        match with_ns(G, &format!("{}{}", NET, snippet)) {
            Ok(_) => missed.push(format!("not caught: {}", snippet)),
            Err(e) => {
                if !e.contains(&format!("error[{}]", code)) || !e.contains(text) {
                    missed.push(format!("wrong report for {}\n  want {} / {}\n  got:\n{}", snippet, code, text, e));
                }
            }
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn errors_point_at_the_declaration() {
    let e = err("(def a 1)\n\n(g/compute_network \"n\" {:mtu \"x\"})");
    assert!(e.contains("t.kiln:4:1"), "{}", e);
    // Inside a component, the declaration in the component's body.
    let e = err("(defcomponent c []\n  (g/compute_network \"n\" {}))\n(c \"one\")");
    assert!(e.contains("google_compute_network.one__n: missing required argument `name`"), "{}", e);
    assert!(e.contains("t.kiln:3:3"), "{}", e);
}

#[test]
fn all_problems_are_reported_together() {
    let e = err(
        "(g/compute_network \"a\" {})\n(g/compute_network \"b\" {:name \"b\" :mtu \"x\"})\n(output \"o\" (:nope (g/compute_network \"c\" {:name \"c\"})))",
    );
    assert!(e.contains("K0702") && e.contains("K0704") && e.contains("K0707"), "{}", e);
    assert!(e.contains("3 problems found; nothing was written"), "{}", e);
}

#[test]
fn valid_stacks_pass() {
    // Expressions, numbers where strings go, blocks as a map or a vector of maps.
    let b = ok(&format!(
        "{}{}",
        NET,
        r#"(g/compute_subnetwork "s" {:name (str (:name vpc) "-s") :network (:id vpc) :region "europe-west3"
                                    :ip_cidr_range (tf/cidrsubnet "10.0.0.0/16" 8 1)})
           (g/compute_firewall "f" {:name "f" :network (:name vpc)
                                    :allow [{:protocol "tcp" :ports [80 "443"]} {:protocol "icmp"}]})
           (g/container_node_pool "p" {:cluster "c" :node_count (tf/expr "var.n")
                                       :node_config {:labels {:tier 1}}})
           (output "link" (:self_link vpc))"#
    ));
    assert!(b.warnings.is_empty(), "{}", b.warnings.join("\n"));
}

#[test]
fn the_gke_example_passes_strictly() {
    let src = std::fs::read_to_string("examples/gke/main.kiln").unwrap();
    let b = build(&src, "main.kiln", &BuildOpts { schemas: Some(google()), strict: true, ..BuildOpts::default() })
        .unwrap_or_else(|e| panic!("{}", e));
    assert!(b.warnings.is_empty());
}

#[test]
fn deprecated_arguments_warn_but_build() {
    let b = ok(&format!(
        "{}{}",
        NET,
        r#"(g/compute_subnetwork "s" {:name "s" :network (:id vpc) :region "r" :ip_cidr_range "10.0.0.0/24" :enable_flow_logs true})"#
    ));
    assert_eq!(b.warnings.len(), 1);
    assert!(b.warnings[0].contains("warning[K0750]") && b.warnings[0].contains("enable_flow_logs"), "{}", b.warnings[0]);
}

#[test]
fn providers_without_a_schema_warn_or_fail_when_strict() {
    let ns = "(ns t (:provider [random :as r :version \"~> 3.6\"]))";
    let b = with_ns(ns, "(r/pet \"p\" {:lenght 2})").unwrap();
    assert!(b.warnings.iter().any(|w| w.contains("warning[K0712]") && w.contains("hashicorp/random")), "{:?}", b.warnings);
    let src = format!("{}\n(r/pet \"p\" {{}})\n", ns);
    let e = build(&src, "t.kiln", &BuildOpts { schemas: Some(google()), strict: true, ..BuildOpts::default() })
        .err()
        .unwrap();
    assert!(e.contains("error[K0712]") && e.contains("kiln schema"), "{}", e);
}

#[test]
fn a_cached_schema_of_the_wrong_version_is_refused() {
    let e = with_ns("(ns t (:provider [google :as g :version \"~> 5.0\"]))", "(g/compute_network \"n\" {})")
        .err()
        .unwrap();
    assert!(e.contains("error[K0711]") && e.contains("6.50.0") && e.contains("~> 5.0"), "{}", e);
    // and nothing is checked against the wrong schema
    assert!(!e.contains("K0702"), "{}", e);
}

#[test]
fn without_schemas_nothing_is_checked() {
    let src = format!("{}\n(g/compute_network \"n\" {{:nonsense 1}})\n", G);
    assert!(kiln::compile(&src, "t.kiln").is_ok());
}

// ------------------------------------------------------------------ cache

const TOFU_SCHEMA: &str = r#"{
  "format_version": "1.0",
  "provider_schemas": {
    "registry.opentofu.org/hashicorp/random": {
      "provider": {"version": 0, "block": {"description_kind": "plain"}},
      "resource_schemas": {
        "random_pet": {"version": 0, "block": {
          "attributes": {
            "id": {"type": "string", "description": "The random pet name.", "description_kind": "plain", "computed": true},
            "length": {"type": "number", "description": "words", "description_kind": "plain", "optional": true, "computed": true},
            "keepers": {"type": ["map", "string"], "description_kind": "plain", "optional": true}
          },
          "description": "A pet.", "description_kind": "plain"}}
      },
      "data_source_schemas": {}
    }
  }
}"#;

const LOCK: &str = r#"
provider "registry.opentofu.org/hashicorp/random" {
  version     = "3.6.3"
  constraints = "~> 3.6"
  hashes = ["h1:abc="]
}
"#;

#[test]
fn engine_schema_converts_to_a_compact_cache() {
    let versions = schema::lock_versions(LOCK).unwrap();
    assert_eq!(versions["hashicorp/random"], "3.6.3");
    let out = schema::convert(TOFU_SCHEMA, &versions).unwrap();
    assert_eq!(out.len(), 1);
    let c = &out[0];
    assert_eq!((c.source.as_str(), c.version.as_str(), c.resources), ("hashicorp/random", "3.6.3", 1));
    assert!(!c.text.contains("description"), "{}", c.text);
    assert!(c.text.contains("    \"random_pet\": {"), "one line per type:\n{}", c.text);
    // The cache drives checks like any other.
    let set = Rc::new(SchemaSet::from_texts(&[&c.text]).unwrap());
    let src = "(ns t (:provider [random :as r :version \"~> 3.6\"]))\n(r/pet \"p\" {:lenght 2 :id \"x\"})\n";
    let e = build(src, "t.kiln", &BuildOpts { schemas: Some(set), ..BuildOpts::default() }).err().unwrap();
    assert!(e.contains("did you mean `length`?") && e.contains("`id` is read-only"), "{}", e);
}

#[test]
fn missing_lock_version_is_an_error() {
    assert!(schema::convert(TOFU_SCHEMA, &Default::default()).is_err());
}

#[test]
fn cache_paths_and_sources() {
    assert_eq!(schema::normalize_source("registry.terraform.io/hashicorp/google"), "hashicorp/google");
    assert_eq!(schema::normalize_source("example.com/acme/thing"), "example.com/acme/thing");
    let p = schema::cache_path(std::path::Path::new(".kiln/schemas"), "registry.opentofu.org/integrations/github");
    assert_eq!(p, std::path::PathBuf::from(".kiln/schemas/integrations/github.json"));
}

#[test]
fn version_constraints() {
    let cases = [
        ("6.50.0", "~> 6.0", true),
        ("7.0.0", "~> 6.0", false),
        ("6.1.5", "~> 6.1.2", true),
        ("6.2.0", "~> 6.1.2", false),
        ("6.1.1", "~> 6.1.2", false),
        ("6.9.0", "~> 6", true),
        ("1.5.0", ">= 1.2, < 2.0", true),
        ("2.0.0", ">= 1.2, < 2.0", false),
        ("3.6.3", "3.6.3", true),
        ("3.6.3", "!= 3.6.3", false),
        ("3.6.3", "", true),
    ];
    for (v, c, want) in cases {
        assert_eq!(schema::satisfies(v, c).unwrap(), want, "{} against {:?}", v, c);
    }
    assert!(schema::satisfies("6.0.0", "~> six").is_err());
}

#[test]
fn templates_are_recognised() {
    use kiln::check::is_template;
    assert!(is_template("${a.b}"));
    assert!(is_template("x-${a}"));
    assert!(is_template("%{ if a }x%{ endif }"));
    assert!(!is_template("$${literal}"));
    assert!(!is_template("cost is $5"));
}
