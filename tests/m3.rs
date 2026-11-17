//! M3, first steps: secrecy tracking and structural recursion.

use kiln::schema::SchemaSet;
use kiln::{build, eval_last, BuildOpts};
use std::rc::Rc;

fn with_google(body: &str) -> Result<kiln::Build, String> {
    let src = format!("(ns t (:provider [google :as g :version \"~> 6.0\"]))\n{}\n", body);
    let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
    build(&src, "t.kiln", &BuildOpts { schemas: Some(schemas), ..BuildOpts::default() })
}

fn err(body: &str) -> String {
    match with_google(body) {
        Ok(_) => panic!("expected an error for:\n{}", body),
        Err(e) => e,
    }
}

const SETUP: &str = r#"
(def me (data g/client_config "me" {}))
(def c (g/container_cluster "c" {:name "c" :location "europe-west3"}))
(def key (:client_key (get (:master_auth c) 0)))
"#;

// ------------------------------------------------------------------ secrecy

#[test]
fn secrets_may_flow_into_sensitive_arguments_and_sensitive_outputs() {
    let b = with_google(&format!(
        "{}{}",
        SETUP,
        r#"(g/secret_manager_secret_version "v" {:secret "s" :secret_data (str "token=" (:access_token me))})
           (output "key" key {:sensitive true})
           (output "ca" (:cluster_ca_certificate (get (:master_auth c) 0)))
           (output "declassified" (tf/nonsensitive (:access_token me)))"#
    ))
    .unwrap_or_else(|e| panic!("{}", e));
    // The markers leave no trace in the output.
    assert!(b.json.contains("\"secret_data\": \"token=${data.google_client_config.me.access_token}\""), "{}", b.json);
}

#[test]
fn a_secret_in_a_plain_argument_is_refused() {
    let e = err(&format!("{}{}", SETUP, r#"(g/compute_network "n" {:name "n" :description (str "token " (:access_token me))})"#));
    assert!(e.contains("error[K0320]") && e.contains("`description` is not sensitive"), "{}", e);
    assert!(e.contains("data.google_client_config.me.access_token") && e.contains("tf/nonsensitive"), "{}", e);
}

#[test]
fn secrets_are_found_inside_nested_blocks_and_maps() {
    // client_key is sensitive inside the master_auth block; labels is a map in a nested block.
    let e = err(&format!("{}{}", SETUP, r#"(g/container_node_pool "p" {:cluster "c" :node_config {:labels {:k key}}})"#));
    assert!(e.contains("`node_config.labels` is not sensitive"), "{}", e);
    assert!(e.contains("google_container_cluster.c.master_auth[0].client_key"), "{}", e);
}

#[test]
fn secrets_survive_functions_and_conditionals() {
    let e = err(&format!(
        "{}{}",
        SETUP,
        r#"(defn wrap [x] (tf/upper (str "[" x "]")))
           (g/compute_network "n" {:name "n" :description (if (= 1 1) (wrap key) "none")})"#
    ));
    assert!(e.contains("K0320"), "{}", e);
}

#[test]
fn an_output_that_exposes_a_secret_must_be_sensitive() {
    let e = err(&format!("{}{}", SETUP, r#"(output "key" (str "k:" key))"#));
    assert!(e.contains("error[K0321]: output `key` exposes the secret"), "{}", e);
    assert!(e.contains(":sensitive true"), "{}", e);
}

#[test]
fn secrets_cannot_decide_count_or_for_each() {
    let e = err(&format!("{}{}", SETUP, r#"(g/compute_network "n" {:name "n" :count (tf/length key)})"#));
    assert!(e.contains("`count` depends on the secret"), "{}", e);
}

#[test]
fn values_can_be_marked_secret() {
    let e = err(r#"(g/compute_network "n" {:name "n" :description (secret (tf/expr "var.token"))})"#);
    assert!(e.contains("the secret `var.token`"), "{}", e);
    let e = err(r#"(g/compute_network "n" {:name "n" :description (secret "hunter2")})"#);
    assert!(e.contains("K0322"), "{}", e);
}

#[test]
fn reads_from_nested_blocks_are_checked() {
    let e = err(&format!("{}{}", SETUP, r#"(output "x" (:client_kye (get (:master_auth c) 0)))"#));
    assert!(e.contains("error[K0707]") && e.contains("google_container_cluster.c.master_auth[0]"), "{}", e);
    assert!(e.contains("did you mean `client_key`?"), "{}", e);
    let e = err(&format!("{}{}", SETUP, r#"(output "x" (:client_key (:master_auth c)))"#));
    assert!(e.contains("error[K0708]") && e.contains("(get x 0)"), "{}", e);
}

// ------------------------------------------------------------------ recursion

#[test]
fn structural_recursion_over_a_list() {
    assert_eq!(eval_last("(defn total [xs] (if (empty? xs) 0 (+ (first xs) (total (rest xs))))) (total [1 2 3 4])").unwrap(), "10");
    assert_eq!(
        eval_last("(defn pairs [[a b & more]] (if (nil? a) [] (concat [[a b]] (pairs more)))) (pairs [1 2 3 4])").unwrap(),
        "[[1 2] [3 4]]"
    );
}

#[test]
fn structural_recursion_over_a_tree() {
    let tree = "{:name \"root\" :children [{:name \"a\" :children [{:name \"a1\" :children []}]} {:name \"b\" :children []}]}";
    let names = format!(
        "(defn names [node] (concat [(:name node)] (mapcat names (:children node)))) (names {})",
        tree
    );
    assert_eq!(eval_last(&names).unwrap(), "[\"root\" \"a\" \"a1\" \"b\"]");
    let depth = format!(
        "(defn depth [node] (inc (reduce max 0 (for [c (:children node)] (depth c))))) (depth {})",
        tree
    );
    assert_eq!(eval_last(&depth).unwrap(), "3");
}

#[test]
fn recursive_components_scope_addresses_by_path() {
    let b = with_google(
        r#"(defcomponent folder [node]
             (g/folder "f" {:display_name (:name node) :parent "organizations/1"})
             (for [c (:children node)] (folder (:name c) c)))
           (folder "root" {:name "root" :children [{:name "eng" :children [{:name "infra" :children []}]}]})"#,
    )
    .unwrap_or_else(|e| panic!("{}", e));
    for addr in ["root__f", "root__eng__f", "root__eng__infra__f"] {
        assert!(b.json.contains(&format!("\"{}\"", addr)), "{} missing:\n{}", addr, b.json);
    }
}

#[test]
fn non_structural_recursion_is_rejected_at_the_definition() {
    let e = eval_last("(defn f [xs] (f xs))").unwrap_err();
    assert!(e.contains("K0401") && e.contains("does not pass a smaller part"), "{}", e);
    // Shrinking a different parameter in different calls does not terminate in general.
    let e = eval_last("(defn g [a b] (if (empty? a) (g b (rest a)) (g (rest a) b)))").unwrap_err();
    assert!(e.contains("K0401"), "{}", e);
    let e = eval_last("(defn h [xs] (let [k h] (k xs)))").unwrap_err();
    assert!(e.contains("K0403"), "{}", e);
}

#[test]
fn recursion_without_a_base_case_stops_at_run_time() {
    // (rest []) is [] again: the static check passes, the size check stops it.
    let e = eval_last("(defn f [xs] (f (rest xs))) (f [1 2])").unwrap_err();
    assert!(e.contains("error[K0402]") && e.contains("size 1 -> 1"), "{}", e);
}

#[test]
fn a_local_name_that_shadows_the_function_is_not_recursion() {
    assert_eq!(eval_last("(defn f [xs] (let [f count] (f xs))) (f [1 2 3])").unwrap(), "3");
}

// ------------------------------------------------------------------ inference (step 3)

fn types(src: &str) -> kiln::TypeReport {
    let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
    kiln::types(src, "t.kiln", Some(schemas)).unwrap_or_else(|e| panic!("{}", e))
}

fn ty_of(src: &str, name: &str) -> String {
    let r = types(src);
    r.defs.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()).unwrap_or_else(|| panic!("no {} in {:?}", name, r.defs))
}

fn clashes(src: &str) -> Vec<String> {
    types(src).clashes
}

const G: &str = "(ns t (:provider [google :as g :version \"~> 6.0\"]))\n";

/// Idiomatic code that must type-check without a single clash.
const IDIOMS: &[&str] = &[
    // maps whose keys differ in one vector; optional keys read with a default
    r#"(def pools [{:name "a" :min 1} {:name "b" :min 0 :spot true}])
       (def spots (for [p pools] (get p :spot false)))"#,
    // nil as absence, when inside a map, merge
    r#"(defn labels [ctx extra] (merge {:env (:env ctx) :managed_by "kiln"} extra))
       (def l (labels {:env "dev"} {:team "x"}))
       (def m {:a 1 :b (when false "x")})"#,
    // heterogeneous vectors and pairs from a map
    r#"(def pair ["a" 1])
       (def kv (for [[k v] {:a 1 :b 2}] (str (name k) "=" v)))"#,
    // a helper used at two different types
    r#"(defn twice [x] [x x])
       (def a (twice 1))
       (def b (twice "s"))"#,
    // keyword arguments and variadic functions
    r#"(defn f [x & {:keys [y]}] [x y])
       (def r (f 1 :y 2))
       (defn g [& xs] (count xs))
       (def n (g 1 2 3))"#,
    // higher-order functions
    r#"(def total (reduce + 0 (map inc [1 2 3])))
       (def names (map (fn [p] (:name p)) [{:name "a"} {:name "b"}]))
       (def big (filter (fn [x] (> x 1)) [1 2 3]))"#,
    // recursion
    r#"(defn names [node] (concat [(:name node)] (mapcat names (:children node))))
       (def all (names {:name "r" :children [{:name "a" :children []}]}))"#,
    // macros and components
    r#"(defmacro unless [c & body] `(if ~c nil (do ~@body)))
       (def u (unless false 1))
       (defcomponent net [ctx] (g/compute_network "vpc" {:name (:env ctx)}))
       (def n (net "a" {:env "dev"}))
       (def id (:id n))"#,
    // resource attributes into strings, deferred arithmetic, Terraform functions
    r#"(def vpc (g/compute_network "vpc" {:name "v"}))
       (def s (g/compute_subnetwork "s" {:name (str (:name vpc) "-s") :network (:id vpc)
                                         :region "r" :ip_cidr_range (tf/cidrsubnet "10.0.0.0/16" 8 1)}))
       (def mtu (+ (:mtu vpc) 1))
       (output "link" (:self_link vpc))"#,
    // nested blocks read through their schema
    r#"(def c (g/container_cluster "c" {:name "c" :location "l"}))
       (def ca (:cluster_ca_certificate (get (:master_auth c) 0)))"#,
];

#[test]
fn idiomatic_code_has_no_type_errors() {
    let mut bad = vec![];
    for snippet in IDIOMS {
        let src = format!("{}{}", G, snippet);
        let c = clashes(&src);
        if !c.is_empty() {
            bad.push(format!("{}\n{}", snippet, c.join("\n")));
        }
    }
    assert!(bad.is_empty(), "false type errors:\n\n{}", bad.join("\n\n"));
}

#[test]
fn examples_fixtures_and_imports_have_no_type_errors() {
    let mut sources = vec![("examples/gke/main.kiln".to_string(), std::fs::read_to_string("examples/gke/main.kiln").unwrap())];
    for v in ["v1", "v2", "v3"] {
        let p = format!("tests/fixtures/move/{}.kiln", v);
        sources.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
    }
    for dir in ["examples/gke/original", "examples/import-offline/original"] {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().map_or(false, |x| x == "tf"))
            .collect();
        files.sort();
        let files: Vec<(String, String)> =
            files.iter().map(|p| (p.display().to_string(), std::fs::read_to_string(p).unwrap())).collect();
        sources.push((format!("import of {}", dir), kiln::import::import(&files, dir).unwrap().source));
    }
    for (name, src) in sources {
        let c = clashes(&src);
        assert!(c.is_empty(), "{}: false type errors:\n{}", name, c.join("\n"));
    }
}

#[test]
fn inferred_types_are_readable() {
    let src = format!(
        "{}{}",
        G,
        r#"(defn labels [ctx extra] (merge {:env (:env ctx)} extra))
           (defn port [svc] (+ (:base svc) 80))
           (def vpc (g/compute_network "vpc" {:name "v"}))
           (def info {:name (:name vpc) :link (:self_link vpc)})"#
    );
    assert_eq!(ty_of(&src, "port"), "(Fn [{:base? Num, & r0}] Num)");
    assert_eq!(ty_of(&src, "vpc"), "google_compute_network");
    assert_eq!(ty_of(&src, "info"), "{:link Str, :name Str}");
    assert!(ty_of(&src, "labels").starts_with("(Fn [{:env? a, & r1} "), "{}", ty_of(&src, "labels"));
}

/// Mistakes the checker finds, here all in code that no build would run.
#[test]
fn clashes_are_found_in_code_that_never_runs() {
    let cases: &[(&str, &str)] = &[
        (r#"(defn f [x] (+ x 1)) (def y (if false (f "a") 0))"#, "argument 1 of `f`"),
        (r#"(defn f [a b] a) (defn never [] (f 1))"#, "`f` takes 2 arguments, got 1"),
        (r#"(defn never [] (:k 5))"#, "cannot look up :k in Num"),
        (r#"(defn never [] (for [x 5] x))"#, "Num is not a collection"),
        (r#"(defn never [] (+ 1 "a"))"#, "argument 2 of `+`"),
        (r#"(def vpc (g/compute_network "vpc" {:name "v"})) (defn never [] (+ (:name vpc) 1))"#, "argument 1 of `+`: Str"),
        (r#"(def cfg {:port 80}) (defn never [] (str (count (:port cfg))))"#, "Num is not a collection"),
        (r#"(defn never [] (5 1))"#, "Num is not a function"),
    ];
    for (snippet, want) in cases {
        let src = format!("{}{}", G, snippet);
        let c = clashes(&src);
        assert!(c.iter().any(|m| m.contains(want)), "{}\nwanted a clash containing {:?}, got:\n{}", snippet, want, c.join("\n"));
        // and none of these stops the build yet
        let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
        let built = build(&src, "t.kiln", &BuildOpts { schemas: Some(schemas), ..BuildOpts::default() });
        assert!(built.is_ok(), "{}: {}", snippet, built.err().unwrap());
    }
}
