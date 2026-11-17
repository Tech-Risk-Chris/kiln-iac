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
        let r = types(&src);
        if !r.clashes.is_empty() || !r.errors.is_empty() {
            bad.push(format!("{}\n{}{}", snippet, r.clashes.join("\n"), r.errors.join("\n")));
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
        let r = types(&src);
        assert!(r.clashes.is_empty(), "{}: false type errors:\n{}", name, r.clashes.join("\n"));
        assert!(r.errors.is_empty(), "{}: false stage errors:\n{}", name, r.errors.join("\n"));
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

// ------------------------------------------------------------------ stages (step 4): the M3 gate

fn stage_errors(body: &str) -> Vec<String> {
    let src = format!("{}{}", G, body);
    types(&src).errors
}

const CLUSTER: &str = "(def c (g/container_cluster \"c\" {:name \"c\" :location \"l\"}))\n";

/// The gate: plan-time `count` and `for_each` failures, and their relatives,
/// become compile errors, even in code that no build runs.
const GATE: &[(&str, &str, &str)] = &[
    // existence decided by a value known only after apply, inside a helper that is never called
    (
        r#"(defn maybe-net [flag] (when flag (g/compute_network "n" {:name "n"})))
           (defn never [] (maybe-net (:enable_autopilot c)))"#,
        "K0312",
        "google_container_cluster.c.enable_autopilot",
    ),
    // the same through a function that declares
    (
        r#"(defn mk [] (g/compute_network "n" {:name "n"}))
           (defn never [] (when (:enable_autopilot c) (mk)))"#,
        "K0312",
        "enable_autopilot",
    ),
    // iterating a deferred list, in a branch that is not taken
    (r#"(def x (if false (for [z (:node_locations c)] z) []))"#, "K0313", "node_locations"),
    // iterating it through a helper (for_each over a computed set)
    (
        r#"(defn nets [zones] (for [z zones] (g/compute_network z {:name z})))
           (defn never [] (nets (:node_locations c)))"#,
        "K0313",
        "node_locations",
    ),
    // a function returning a deferred value, iterated elsewhere
    (r#"(defn locs [] (:node_locations c)) (defn never [] (map identity (locs)))"#, "K0313", "node_locations"),
    // count of a deferred collection
    (r#"(defn never [] (count (:node_locations c)))"#, "K0313", "node_locations"),
    // a filter that decides existence
    (r#"(defn never [] (for [z ["a" "b"] :when (= z (:location c))] (g/compute_network z {:name z})))"#, "K0312", "location"),
    // a name known only after apply
    (r#"(defn never [] (g/compute_network (:name c) {:name "x"}))"#, "K0314", "google_container_cluster.c.name"),
    (r#"(defcomponent net [n] (g/compute_network "v" {:name n})) (defn never [] (net (:name c) "x"))"#, "K0314", "name"),
    // short-circuit and compile-time text functions
    (r#"(defn never [] (and (:enable_autopilot c) 1))"#, "K0315", "enable_autopilot"),
    (r#"(defn never [] (slug (:name c)))"#, "K0316", "name"),
    // Terraform functions are evaluated by the engine
    (r#"(defn never [] (for [p (tf/split "," "a,b")] p))"#, "K0313", "the Terraform function `split`"),
];

#[test]
fn the_m3_gate_corpus_is_caught_without_running_it() {
    let mut missed = vec![];
    for (snippet, code, text) in GATE {
        let errs = stage_errors(&format!("{}{}", CLUSTER, snippet));
        let hit = errs.iter().any(|e| e.contains(&format!("error[{}]", code)) && e.contains(text));
        if !hit {
            missed.push(format!("{}\n  want {} mentioning {:?}, got:\n{}", snippet, code, text, errs.join("\n")));
        }
        // and builds now refuse it
        let src = format!("{}{}{}", G, CLUSTER, snippet);
        let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
        if build(&src, "t.kiln", &BuildOpts { schemas: Some(schemas), ..BuildOpts::default() }).is_ok() {
            missed.push(format!("build accepted: {}", snippet));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn stage_errors_point_at_the_sink_and_name_the_source() {
    let src = format!(
        "{}{}{}",
        G,
        CLUSTER,
        "(defn maybe-net [flag]\n  (when flag (g/compute_network \"n\" {:name \"n\"})))\n(defn never [] (maybe-net (:enable_autopilot c)))\n"
    );
    let r = types(&src);
    let e = r.errors.join("\n");
    assert!(e.contains("t.kiln:4:3"), "the error is at the `when`:\n{}", e);
    assert!(e.contains("(line 5, column 27)"), "and names where the value came from:\n{}", e);
}

/// Values known after apply are fine wherever they do not shape the graph.
const VALID_STAGES: &[&str] = &[
    // value position: a Terraform conditional
    r#"(def label (if (:enable_autopilot c) "auto" "standard"))"#,
    // helpers used with deferred values as plain values
    r#"(defn label [x] (str "x-" x)) (def a (g/compute_network "n" {:name "n" :description (label (:name c))}))"#,
    // polymorphism keeps call sites apart: static here, deferred there
    r#"(defn pick [m] (:a m))
       (def zs (for [z (pick {:a ["a" "b"]})] z))
       (output "o" (pick {:a (:node_locations c)}))"#,
    // iterating static data that contains deferred values
    r#"(def items [{:n "a" :v (:name c)} {:n "b" :v "x"}])
       (def nets (for [i items] (g/compute_network (:n i) {:name (:n i) :description (:v i)})))"#,
    // conditions known at compile time decide existence
    r#"(def env "dev") (def n (when (= env "prod") (g/compute_network "n" {:name "n"})))"#,
    // deferred values in strings, numbers and Terraform functions
    r#"(def mtu (+ 1 (:initial_node_count c))) (def up (tf/upper (:name c)))"#,
];

#[test]
fn deferred_values_that_do_not_shape_the_graph_are_accepted() {
    let mut bad = vec![];
    for snippet in VALID_STAGES {
        let errs = stage_errors(&format!("{}{}", CLUSTER, snippet));
        if !errs.is_empty() {
            bad.push(format!("{}\n{}", snippet, errs.join("\n")));
        }
    }
    assert!(bad.is_empty(), "false stage errors:\n\n{}", bad.join("\n\n"));
}

#[test]
fn executed_errors_keep_their_exact_message_and_are_not_reported_twice() {
    let src = format!("{}{}(when (:enable_autopilot c)\n  (g/compute_network \"n\" {{:name \"n\"}}))\n", G, CLUSTER);
    let e = build(&src, "t.kiln", &BuildOpts::default()).err().unwrap();
    assert_eq!(e.matches("error[K0312]").count(), 1, "{}", e);
    assert!(e.contains("google_container_cluster.c.enable_autopilot"), "{}", e);
}

// ------------------------------------------------------------------ step 6: count-deferred, for-each-deferred

fn json(body: &str) -> serde_json::Value {
    let b = with_google(&format!("{}{}", CLUSTER, body)).unwrap_or_else(|e| panic!("{}", e));
    assert!(b.type_clashes.is_empty(), "{}", b.type_clashes.join("\n"));
    serde_json::from_str(&b.json).unwrap()
}

#[test]
fn count_deferred_emits_count_and_reads_with_one() {
    let j = json(
        r#"(def fw (count-deferred (:enable_autopilot c)
                     (g/compute_firewall "fw" {:name "fw" :network "default" :allow {:protocol "tcp"}})))
           (output "id" (:id fw))
           (g/compute_network "uses" {:name "u" :description (:self_link fw)})"#,
    );
    let fw = &j["resource"]["google_compute_firewall"]["fw"];
    assert_eq!(fw["count"], "${(google_container_cluster.c.enable_autopilot ? 1 : 0)}");
    assert_eq!(j["output"]["id"]["value"], "${one(google_compute_firewall.fw[*].id)}");
    assert_eq!(j["resource"]["google_compute_network"]["uses"]["description"], "${one(google_compute_firewall.fw[*].self_link)}");
}

#[test]
fn nested_count_deferred_conditions_are_combined() {
    let j = json(
        r#"(count-deferred (:enable_autopilot c)
             (count-deferred (= (:location c) "europe-west3")
               (g/compute_network "n" {:name "n"})))"#,
    );
    assert_eq!(
        j["resource"]["google_compute_network"]["n"]["count"],
        "${((google_container_cluster.c.enable_autopilot && (google_container_cluster.c.location == \"europe-west3\")) ? 1 : 0)}"
    );
}

#[test]
fn for_each_deferred_binds_each_and_keys_siblings() {
    let j = json(
        r#"(def subs
             (for-each-deferred [zone (:node_locations c)]
               (let [net (g/compute_network "z" {:name (str "net-" zone)})]
                 (g/compute_subnetwork "z" {:name (str "sub-" zone) :network (:id net) :ip_cidr_range "10.0.0.0/24"}))))
           (output "ids" (:id subs))"#,
    );
    let sub = &j["resource"]["google_compute_subnetwork"]["z"];
    assert_eq!(sub["for_each"], "${google_container_cluster.c.node_locations}");
    assert_eq!(sub["name"], "sub-${each.value}");
    assert_eq!(sub["network"], "${google_compute_network.z[each.key].id}");
    assert_eq!(j["output"]["ids"]["value"], "${{for key, r in google_compute_subnetwork.z : key => r.id}}");
}

#[test]
fn for_each_deferred_over_a_map_binds_key_and_value() {
    let j = json(
        r#"(for-each-deferred [[k v] {:a "x" :b "y"}]
             (g/compute_network "n" {:name (str k "-" v)}))
           (for-each-deferred [z ["a" "b"]] (g/compute_network "m" {:name z}))"#,
    );
    let n = &j["resource"]["google_compute_network"];
    assert_eq!(n["n"]["for_each"], serde_json::json!({"a": "x", "b": "y"}));
    assert_eq!(n["n"]["name"], "${each.key}-${each.value}");
    assert_eq!(n["m"]["for_each"], "${toset([\"a\", \"b\"])}");
}

#[test]
fn components_inside_the_forms_are_instanced_too() {
    let j = json(
        r#"(defcomponent pair [] (g/compute_network "a" {:name "a"}) (g/compute_network "b" {:name "b"}))
           (count-deferred (:enable_autopilot c) (pair "p"))"#,
    );
    for n in ["p__a", "p__b"] {
        assert!(j["resource"]["google_compute_network"][n]["count"].is_string(), "{}", n);
    }
}

#[test]
fn the_forms_need_no_compile_time_condition_but_names_still_do() {
    // no K0312/K0313: that is the point of the forms
    let errs = stage_errors(&format!(
        "{}{}",
        CLUSTER,
        r#"(defn never [] (count-deferred (:enable_autopilot c) (g/compute_network "n" {:name "n"})))
           (defn never2 [] (for-each-deferred [z (:node_locations c)] (g/compute_network "m" {:name z})))"#
    ));
    assert!(errs.is_empty(), "{}", errs.join("\n"));
    // but each.value is not a name
    let errs = stage_errors(&format!(
        "{}{}",
        CLUSTER,
        r#"(defn never [] (for-each-deferred [z (:node_locations c)] (g/compute_network z {:name z})))"#
    ));
    assert!(errs.iter().any(|e| e.contains("K0314") && e.contains("each.value")), "{}", errs.join("\n"));
}

#[test]
fn nesting_and_conflicts_are_refused() {
    let e = err(&format!("{}{}", CLUSTER, r#"(for-each-deferred [a ["x"]] (for-each-deferred [b ["y"]] (g/compute_network "n" {:name "n"})))"#));
    assert!(e.contains("K0318"), "{}", e);
    let e = err(&format!("{}{}", CLUSTER, r#"(for-each-deferred [a ["x"]] (count-deferred true (g/compute_network "n" {:name "n"})))"#));
    assert!(e.contains("K0318"), "{}", e);
    let e = err(&format!("{}{}", CLUSTER, r#"(count-deferred (:enable_autopilot c) (g/compute_network "n" {:name "n" :count 2}))"#));
    assert!(e.contains("K0317"), "{}", e);
}

#[test]
fn a_secret_cannot_decide_count() {
    let e = err(&format!("{}{}", SETUP, r#"(count-deferred (tf/length key) (g/compute_network "n" {:name "n"}))"#));
    assert!(e.contains("K0320") && e.contains("`count` depends on the secret"), "{}", e);
}

// ------------------------------------------------------------------ step 5: schema and secrets before evaluation

/// Schema mistakes inside a function nobody calls: found anyway.
const UNRUN_SCHEMA: &[(&str, &str, &str)] = &[
    (r#"(g/compute_network "n" {:name "n" :descripton "x"})"#, "K0701", "did you mean `description`?"),
    (r#"(g/compute_network "n" {:auto_create_subnetworks false})"#, "K0702", "missing required argument `name`"),
    (r#"(g/compute_network "n" {:name "n" :self_link "x"})"#, "K0703", "`self_link` is read-only"),
    (r#"(g/compute_network "n" {:name "n" :mtu "1460"})"#, "K0704", "`mtu` must be number"),
    (r#"(g/compute_network "n" {:name "n" :auto_create_subnetworks "true"})"#, "K0704", "must be bool"),
    (r#"(g/container_node_pool "p" {:cluster "c" :node_config {:machine_typ "e2"}})"#, "K0701", "`node_config.machine_typ`"),
    (r#"(g/container_node_pool "p" {:cluster "c" :node_config {:oauth_scopes "x"}})"#, "K0704", "`node_config.oauth_scopes` must be set(string)"),
    (r#"(g/container_node_pool "p" {:cluster "c" :autoscaling 5})"#, "K0705", "is a nested block"),
    (r#"(g/compute_firewall "f" {:name "f" :network "n" :allow {:ports ["80"]}})"#, "K0702", "`allow.protocol`"),
    (r#"(data g/client_config "me" {:nope 1})"#, "K0701", "unknown argument `nope`"),
];

#[test]
fn schema_mistakes_are_found_in_code_that_never_runs() {
    let mut missed = vec![];
    for (snippet, code, text) in UNRUN_SCHEMA {
        let errs = stage_errors(&format!("(defn never [] {})", snippet));
        if !errs.iter().any(|e| e.contains(&format!("error[{}]", code)) && e.contains(text)) {
            missed.push(format!("{}\n  want {} with {:?}, got:\n{}", snippet, code, text, errs.join("\n")));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn values_known_after_apply_pass_the_static_type_check() {
    // a string attribute into a number argument: the engine converts; the evaluator lets it pass too
    let errs = stage_errors(&format!("{}{}", CLUSTER, r#"(defn never [] (g/compute_network "n" {:name "n" :mtu (:name c)}))"#));
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

#[test]
fn incomplete_maps_are_not_judged_for_completeness() {
    // a map built from a parameter: fields present are checked, missing ones are not guessed
    let errs = stage_errors(r#"(defn mk [extra] (g/compute_network "n" (merge {:mtu 1460} extra)))"#);
    assert!(errs.is_empty(), "{}", errs.join("\n"));
    let errs = stage_errors(r#"(defn mk [cfg] (g/compute_network "n" {:name (:name cfg) :mtu (:mtu cfg)}))"#);
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

#[test]
fn evaluated_mistakes_are_reported_once_with_the_address() {
    let src = format!("{}(g/compute_network \"n\" {{:name \"n\" :descripton \"x\"}})\n", G);
    let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
    let e = build(&src, "t.kiln", &BuildOpts { schemas: Some(schemas), ..BuildOpts::default() }).err().unwrap();
    assert_eq!(e.matches("error[K0701]").count(), 1, "{}", e);
    assert!(e.contains("google_compute_network.n: unknown argument"), "{}", e);
}

/// Secrets leaking in code that never runs.
const UNRUN_SECRETS: &[(&str, &str, &str)] = &[
    (r#"(defn never [] (g/compute_network "n" {:name "n" :description (str "t=" (:access_token me))}))"#, "K0320", "`description` is not sensitive"),
    (r#"(defn never [] (g/container_node_pool "p" {:cluster "c" :node_config {:labels {:k key}}}))"#, "K0320", "`node_config.labels` is not sensitive"),
    (r#"(defn never [] (output "k" key))"#, "K0321", "output `k` exposes a secret"),
    (r#"(defn wrap [x] (tf/upper x)) (defn never [] (g/compute_network "n" {:name "n" :description (wrap key)}))"#, "K0320", "client_key"),
    (r#"(defn never [] (count-deferred (tf/length key) (g/compute_network "n" {:name "n"})))"#, "K0320", "`count` depends on a secret"),
    (r#"(defn never [] (g/compute_network "n" {:name "n" :description (secret (tf/expr "var.t"))}))"#, "K0320", "marked with `secret`"),
];

#[test]
fn secrets_are_tracked_in_code_that_never_runs() {
    let mut missed = vec![];
    for (snippet, code, text) in UNRUN_SECRETS {
        let errs = stage_errors(&format!("{}{}", SETUP, snippet));
        if !errs.iter().any(|e| e.contains(&format!("error[{}]", code)) && e.contains(text)) {
            missed.push(format!("{}\n  want {} with {:?}, got:\n{}", snippet, code, text, errs.join("\n")));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn secrets_may_go_where_they_belong_statically_too() {
    let errs = stage_errors(&format!(
        "{}{}",
        SETUP,
        r#"(defn ok [] (g/secret_manager_secret_version "v" {:secret "s" :secret_data (str "t=" (:access_token me))}))
           (defn ok2 [] (output "k" key {:sensitive true}))
           (defn ok3 [] (g/compute_network "n" {:name "n" :description (tf/nonsensitive key)}))
           (defn ok4 [] (g/compute_network "n" {:name "n" :description (:cluster_ca_certificate (get (:master_auth c) 0))}))"#
    ));
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

// ------------------------------------------------------------------ step 7: typed modules

/// A stack next to the `bucket` module of examples/modules, whose interface is
/// cached in .kiln/modules.
const MOD_FILE: &str = "examples/modules/t.kiln";

fn mod_build(body: &str, strict: bool) -> Result<kiln::Build, String> {
    let src = format!("{}{}\n", G, body);
    let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
    build(&src, MOD_FILE, &BuildOpts { schemas: Some(schemas), strict, ..BuildOpts::default() })
}

fn mod_errors(body: &str) -> Vec<String> {
    let src = format!("{}{}\n", G, body);
    let schemas = Rc::new(SchemaSet::from_dir(".kiln/schemas".into()));
    kiln::types(&src, MOD_FILE, Some(schemas)).unwrap_or_else(|e| panic!("{}", e)).errors
}

#[test]
fn a_module_interface_is_read_from_its_variables_and_outputs() {
    let (inputs, outputs) = kiln::schema::module_interface_from_dir(std::path::Path::new("examples/modules/bucket")).unwrap();
    let attr = |b: &serde_json::Value, n: &str| b["attributes"][n].clone();
    assert_eq!(attr(&inputs, "name")["required"], true);
    assert_eq!(attr(&inputs, "name")["type"], "string");
    assert_eq!(attr(&inputs, "location")["optional"], true);
    assert_eq!(attr(&inputs, "labels")["type"], serde_json::json!(["map", "string"]));
    assert_eq!(
        attr(&inputs, "versioning")["type"],
        serde_json::json!(["object", {"enabled": "bool", "keep_last": "number"}, ["keep_last"]])
    );
    assert_eq!(attr(&inputs, "kms_key")["sensitive"], true);
    assert_eq!(attr(&outputs, "url")["computed"], true);
    assert_eq!(attr(&outputs, "kms_key")["sensitive"], true);
    assert!(outputs["attributes"].get("nope").is_none());
}

#[test]
fn the_cached_interface_is_what_kiln_schema_writes() {
    let (inputs, outputs) = kiln::schema::module_interface_from_dir(std::path::Path::new("examples/modules/bucket")).unwrap();
    let want = kiln::schema::render_module_cache("local:examples/modules/bucket", "", &inputs, &outputs);
    let got = std::fs::read_to_string(".kiln/modules/local_examples_modules_bucket.json").unwrap();
    assert_eq!(got, want, "run `kiln schema examples/modules/main.kiln`");
}

/// Module calls that are wrong, both evaluated and in code that never runs.
const BAD_CALLS: &[(&str, &str, &str)] = &[
    (r#"(module "b" {:source "./bucket" :name "x" :locaton "EU"})"#, "K0701", "did you mean `location`?"),
    (r#"(module "b" {:source "./bucket" :location "EU"})"#, "K0702", "missing required argument `name`"),
    (r#"(module "b" {:source "./bucket" :name "x" :versioning "on"})"#, "K0704", "`versioning` must be"),
    (r#"(module "b" {:source "./bucket" :name "x" :labels ["a"]})"#, "K0704", "`labels` must be"),
    (r#"(module "b" {:source "./bucket" :name "x" :versioning {:enabled "yes"}})"#, "K0704", "`versioning.enabled` must be"),
];

#[test]
fn module_inputs_are_checked() {
    let mut missed = vec![];
    for (snippet, code, text) in BAD_CALLS {
        match mod_build(snippet, false) {
            Ok(_) => missed.push(format!("{}: built", snippet)),
            Err(e) if e.contains(&format!("error[{}]", code)) && e.contains(text) && e.contains("module.b") => {}
            Err(e) => missed.push(format!("{}\n  want {} with {:?}, got:\n{}", snippet, code, text, e)),
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn module_inputs_are_checked_in_code_that_never_runs() {
    let mut missed = vec![];
    for (snippet, code, text) in BAD_CALLS {
        let snippet = snippet.replace(r#"(module "b""#, "(module n");
        let errs = mod_errors(&format!("(defn never [n] {})", snippet));
        if !errs.iter().any(|e| e.contains(&format!("error[{}]", code)) && e.contains(text)) {
            missed.push(format!("{}\n  want {} with {:?}, got:\n{}", snippet, code, text, errs.join("\n")));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n\n"));
}

#[test]
fn module_outputs_are_checked() {
    let e = mod_build(r#"(def b (module "b" {:source "./bucket" :name "x"})) (output "u" (:uri b))"#, false).err().unwrap();
    assert!(e.contains("error[K0707]") && e.contains("did you mean `url`?"), "{}", e);
    let errs = mod_errors(r#"(defn never [] (:uri (module "b" {:source "./bucket" :name "x"})))"#);
    assert!(errs.iter().any(|e| e.contains("K0707") && e.contains("uri")), "{}", errs.join("\n"));
}

#[test]
fn sensitive_module_outputs_are_secrets() {
    let body = r#"(def b (module "b" {:source "./bucket" :name "x"}))"#;
    let e = mod_build(&format!(r#"{} (output "k" (:kms_key b))"#, body), false).err().unwrap();
    assert!(e.contains("error[K0321]") && e.contains("module.b.kms_key"), "{}", e);
    mod_build(&format!(r#"{} (output "k" (:kms_key b) {{:sensitive true}})"#, body), false).unwrap_or_else(|e| panic!("{}", e));
    let errs = mod_errors(&format!(r#"{} (defn never [] (output "k" (:kms_key b)))"#, body));
    assert!(errs.iter().any(|e| e.contains("K0321")), "{}", errs.join("\n"));
}

#[test]
fn a_module_without_a_cached_interface_warns_or_fails_when_strict() {
    let body = r#"(module "x" {:source "./nowhere" :anything 1})"#;
    let b = mod_build(body, false).unwrap_or_else(|e| panic!("{}", e));
    assert!(b.warnings.iter().any(|w| w.contains("K0714") && w.contains("./nowhere")), "{:?}", b.warnings);
    let e = mod_build(body, true).err().unwrap();
    assert!(e.contains("error[K0714]"), "{}", e);
}

#[test]
fn correct_module_calls_raise_nothing() {
    let src = std::fs::read_to_string("examples/modules/main.kiln").unwrap();
    let b = mod_build(&src.replace("(ns examples.modules\n  (:provider [google :as g :version \"~> 6.0\"]))", ""), true)
        .unwrap_or_else(|e| panic!("{}", e));
    assert!(b.warnings.is_empty() && b.type_clashes.is_empty(), "{:?} {:?}", b.warnings, b.type_clashes);
    // module values built from a parameter: present fields checked, absent ones not guessed
    let errs = mod_errors(r#"(defn mk [n extra] (module n (merge {:source "./bucket"} extra)))"#);
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

#[test]
fn local_module_sources_are_written_relative_to_the_output() {
    use kiln::schema::rebase_source;
    use std::path::Path;
    assert_eq!(rebase_source("./bucket", Path::new("examples/modules"), None), "../bucket");
    assert_eq!(rebase_source("../shared/net", Path::new("stacks/dev"), None), "../../shared/net");
    assert_eq!(rebase_source("./bucket", Path::new("stacks/dev"), Some(Path::new("stacks/dev/gen/tf"))), "../../bucket");
    assert_eq!(rebase_source("./gen/m", Path::new("s"), Some(Path::new("s/gen"))), "./m");
    let b = mod_build(r#"(module "b" {:source "./bucket" :name "x"})"#, false).unwrap();
    assert!(b.json.contains("\"source\": \"../bucket\""), "{}", b.json);
}

#[test]
fn kiln_schema_sees_module_sources_as_written() {
    let src = std::fs::read_to_string("examples/modules/main.kiln").unwrap();
    assert_eq!(kiln::modules_of(&src, "examples/modules/main.kiln"), vec![("./bucket".to_string(), None)]);
}

#[test]
fn a_string_key_on_a_resource_picks_an_instance() {
    let errs = stage_errors(
        r#"(def sizes {"small" 4 "large" 8})
           (def nets (for-each-deferred [s (tf/keys sizes)] (g/compute_network "net" {:name s})))
           (def ok (:self_link (get nets "small")))
           (defn bad [] (:self_lnk (get nets "small")))"#,
    );
    assert_eq!(errs.len(), 1, "{}", errs.join("\n"));
    assert!(errs[0].contains("K0707") && errs[0].contains("self_lnk"), "{}", errs[0]);
}
