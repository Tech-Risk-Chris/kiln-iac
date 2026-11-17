use kiln::{compile, eval_last, expand};

fn ev(src: &str) -> String {
    let out = eval_last(src).unwrap_or_else(|e| panic!("{}", e));
    let r = kiln::types(src, "t.kiln", None).unwrap();
    assert!(r.clashes.is_empty(), "false type error in valid code:\n{}\n{}", src, r.clashes.join("\n"));
    assert!(r.errors.is_empty(), "false stage error in valid code:\n{}\n{}", src, r.errors.join("\n"));
    out
}

fn err(src: &str) -> String {
    match compile(src, "t.kiln") {
        Ok(out) => panic!("expected an error, got:\n{}", out),
        Err(e) => e,
    }
}

const NS: &str = "(ns t (:provider [google :as g :version \"~> 6.0\"]))\n";

fn stack(body: &str) -> serde_json::Value {
    let src = format!("{}{}", NS, body);
    let b = kiln::build(&src, "t.kiln", &kiln::BuildOpts::default()).unwrap_or_else(|e| panic!("{}", e));
    assert!(b.type_clashes.is_empty(), "false type error in valid code:\n{}\n{}", src, b.type_clashes.join("\n"));
    let out = b.json;
    serde_json::from_str(&out).unwrap()
}

// ------------------------------------------------------------------ language

#[test]
fn reader_and_literals() {
    assert_eq!(ev("[1 -2 3.5 \"a\\nb\" :k nil true]"), "[1 -2 3.5 \"a\\nb\" :k nil true]");
    assert_eq!(ev("{:a 1, :b [2 3]} ; comment"), "{:a 1 :b [2 3]}");
    assert_eq!(ev("#_(ignored form) 42"), "42");
    assert_eq!(ev("'(a b ~c)"), "(a b (unquote c))");
}

#[test]
fn arithmetic_and_comparison() {
    assert_eq!(ev("(+ 1 2 3)"), "6");
    assert_eq!(ev("(/ 7 2)"), "3.5");
    assert_eq!(ev("(/ 8 2)"), "4");
    assert_eq!(ev("(- 5)"), "-5");
    assert_eq!(ev("[(< 1 2) (= [1 2] [1 2]) (not= :a :b) (max 3 9 4) (min 3 9 4)]"), "[true true true 9 3]");
}

#[test]
fn functions_and_destructuring() {
    assert_eq!(ev("(defn f [a {:keys [b c] :or {c 10}}] (+ a b c)) (f 1 {:b 2})"), "13");
    assert_eq!(ev("(let [[a b & more] [1 2 3 4]] [a b more])"), "[1 2 (3 4)]");
    assert_eq!(ev("(defn f [x & {:keys [y]}] [x y]) (f 1 :y 2)"), "[1 2]");
    assert_eq!(ev("((fn [x] (* x x)) 7)"), "49");
    assert_eq!(ev("(:a {:a 1})"), "1");
    assert_eq!(ev("(:missing {:a 1} :dflt)"), ":dflt");
}

#[test]
fn collections() {
    assert_eq!(ev("(for [x [1 2 3] :when (> x 1) y [:a :b]] [x y])"), "[[2 :a] [2 :b] [3 :a] [3 :b]]");
    assert_eq!(ev("(for [[k v] {:a 1 :b 2}] (str (name k) \"=\" v))"), "[\"a=1\" \"b=2\"]");
    assert_eq!(ev("(reduce + 0 (map inc [1 2 3]))"), "9");
    assert_eq!(ev("(merge {:a 1 :b 2} {:b 3} nil)"), "{:a 1 :b 3}");
    assert_eq!(ev("(merge-deep {:a {:x 1 :y 2}} {:a {:y 3}})"), "{:a {:x 1 :y 3}}");
    assert_eq!(ev("(conj [1 2] 3)"), "[1 2 3]");
    assert_eq!(ev("(get-in {:a [{:b 7}]} [:a 0 :b])"), "7");
    assert_eq!(ev("(filter (fn [x] (> x 1)) [1 2 3])"), "[2 3]");
    assert_eq!(ev("(slug \"roles/pubsub.publisher\")"), "\"roles_pubsub_publisher\"");
    assert_eq!(ev("(join \", \" [\"a\" 1 :k])"), "\"a, 1, :k\"");
}

#[test]
fn prelude_macros() {
    assert_eq!(ev("(when (> 2 1) :a :b)"), ":b");
    assert_eq!(ev("(when-not true :a)"), "nil");
    assert_eq!(ev("(cond false 1 nil 2 :else 3)"), "3");
    assert_eq!(ev("(and 1 2 nil 4)"), "nil");
    assert_eq!(ev("(or nil false 3)"), "3");
}

#[test]
fn user_macros_and_gensym() {
    assert_eq!(ev("(defmacro unless [c & body] `(if ~c nil (do ~@body))) (unless false 1 2)"), "2");
    assert_eq!(ev("(defmacro twice [e] `(let [x# ~e] [x# x#])) (let [x 1] (twice (+ x 1)))"), "[2 2]");
    let ex = expand("(defmacro twice [e] `(let [x# ~e] [x# x#]))\n(twice 5)", "t.kiln").unwrap();
    assert!(ex.contains("(let [x__"), "{}", ex);
}

#[test]
fn only_structural_recursion_and_no_forward_references() {
    // Counting down is not structural: n - 1 is not a part of n.
    let e = eval_last("(defn f [n] (if (= n 0) 0 (f (- n 1)))) (f 3)").unwrap_err();
    assert!(e.contains("K0401"), "{}", e);
    let e = eval_last("(defn a [] (b)) (defn b [] 1) (a)").unwrap_err();
    assert!(e.contains("unbound symbol `b`"), "{}", e);
}

#[test]
fn runaway_macro_expansion_is_stopped() {
    let e = eval_last("(defmacro m [] `(m)) (m)").unwrap_err();
    assert!(e.contains("K0400"), "{}", e);
}

#[test]
fn def_only_at_top_level() {
    let e = eval_last("(let [x 1] (def y x))").unwrap_err();
    assert!(e.contains("K0104"), "{}", e);
}

// ------------------------------------------------------------------ Terraform

#[test]
fn resources_references_and_templates() {
    let j = stack(
        r#"(def vpc (g/compute_network "main" {:name "n" :auto_create_subnetworks false}))
           (g/compute_subnetwork "a" {:name (str "sub-" (:name vpc) "-x")
                                      :network (:id vpc)
                                      :ip_cidr_range (tf/cidrsubnet "10.0.0.0/16" 4 1)
                                      :region "europe-west3"})"#,
    );
    let s = &j["resource"]["google_compute_subnetwork"]["a"];
    assert_eq!(s["network"], "${google_compute_network.main.id}");
    assert_eq!(s["name"], "sub-${google_compute_network.main.name}-x");
    assert_eq!(s["ip_cidr_range"], "${cidrsubnet(\"10.0.0.0/16\", 4, 1)}");
    assert_eq!(j["terraform"]["required_providers"]["google"]["version"], "~> 6.0");
}

#[test]
fn literal_dollar_brace_is_escaped() {
    let j = stack(r#"(g/compute_network "main" {:name "n" :description "cost is ${amount} or %{x}"})"#);
    assert_eq!(
        j["resource"]["google_compute_network"]["main"]["description"],
        "cost is $${amount} or %%{x}"
    );
}

#[test]
fn nil_arguments_are_omitted() {
    let j = stack(r#"(g/compute_network "main" {:name "n" :description (when false "x") :mtu nil})"#);
    let r = j["resource"]["google_compute_network"]["main"].as_object().unwrap();
    assert_eq!(r.keys().collect::<Vec<_>>(), vec!["name"]);
}

#[test]
fn components_scope_addresses() {
    let j = stack(
        r#"(defcomponent inner [] (g/compute_network "vpc" {:name "n"}))
           (defcomponent outer [] (inner "b"))
           (outer "a")
           (outer "z")"#,
    );
    let nets = j["resource"]["google_compute_network"].as_object().unwrap();
    assert_eq!(nets.keys().collect::<Vec<_>>(), vec!["a__b__vpc", "z__b__vpc"]);
}

#[test]
fn deferred_values_lift_through_operators() {
    let j = stack(
        r#"(def c (g/container_cluster "c" {:name "c" :location "x"}))
           (output "a" (+ (:node_count c) 1))
           (output "b" (if (= (:location c) "x") "yes" "no"))
           (output "d" (:cluster_ca_certificate (get (:master_auth c) 0)))
           (output "e" (not (:enable_autopilot c)))"#,
    );
    assert_eq!(j["output"]["a"]["value"], "${(google_container_cluster.c.node_count + 1)}");
    assert_eq!(j["output"]["b"]["value"], "${((google_container_cluster.c.location == \"x\") ? \"yes\" : \"no\")}");
    assert_eq!(j["output"]["d"]["value"], "${google_container_cluster.c.master_auth[0].cluster_ca_certificate}");
    assert_eq!(j["output"]["e"]["value"], "${!(google_container_cluster.c.enable_autopilot)}");
}

#[test]
fn data_sources_modules_backend_depends_on() {
    let j = stack(
        r#"(backend "gcs" {:bucket "state" :prefix "t"})
           (def me (data g/client_config "me" {}))
           (def m (module "net" {:source "terraform-google-modules/network/google" :version "~> 9.0"}
                               {:project_id (:project me) :network_name "n"}))
           (def vpc (g/compute_network "main" {:name (:network_name m)}))
           (g/compute_router "r" {:name "r" :network (:id vpc) :depends_on [vpc m]})"#,
    );
    assert_eq!(j["terraform"]["backend"]["gcs"]["bucket"], "state");
    assert_eq!(j["module"]["net"]["project_id"], "${data.google_client_config.me.project}");
    assert_eq!(j["resource"]["google_compute_network"]["main"]["name"], "${module.net.network_name}");
    assert_eq!(
        j["resource"]["google_compute_router"]["r"]["depends_on"],
        serde_json::json!(["google_compute_network.main", "module.net"])
    );
}

#[test]
fn output_is_deterministic() {
    let src = std::fs::read_to_string("examples/gke/main.kiln").unwrap();
    let a = compile(&src, "examples/gke/main.kiln").unwrap();
    let b = compile(&src, "examples/gke/main.kiln").unwrap();
    assert_eq!(a, b);
}

// ------------------------------------------------------------------ errors

#[test]
fn deferred_condition_cannot_decide_existence() {
    let src = format!(
        "{}(def c (g/container_cluster \"c\" {{:name \"c\"}}))\n(when (:enable_autopilot c)\n  (g/compute_network \"n\" {{:name \"n\"}}))\n",
        NS
    );
    let e = compile(&src, "t.kiln").unwrap_err();
    assert!(e.contains("error[K0312]"), "{}", e);
    assert!(e.contains("google_container_cluster.c.enable_autopilot"), "{}", e);
    assert!(e.contains("t.kiln:3:1"), "{}", e);
}

#[test]
fn deferred_collection_cannot_be_iterated() {
    let e = err(&format!(
        "{}(def c (g/container_cluster \"c\" {{:name \"c\"}}))\n(for [z (:node_locations c)] (g/compute_network z {{}}))",
        NS
    ));
    assert!(e.contains("K0313"), "{}", e);
}

#[test]
fn deferred_name_is_rejected() {
    let e = err(&format!(
        "{}(def c (g/container_cluster \"c\" {{:name \"c\"}}))\n(g/compute_network (:name c) {{}})",
        NS
    ));
    assert!(e.contains("K0314"), "{}", e);
}

#[test]
fn duplicate_and_invalid_names() {
    let e = err(&format!("{}(g/compute_network \"a\" {{}})\n(g/compute_network \"a\" {{}})", NS));
    assert!(e.contains("K0202") && e.contains("google_compute_network.a") && e.contains("t.kiln:3:1"), "{}", e);
    let e = err(&format!("{}(g/compute_network \"roles/x\" {{}})", NS));
    assert!(e.contains("K0201") && e.contains("slug"), "{}", e);
}

#[test]
fn handle_is_not_a_value() {
    let e = err(&format!(
        "{}(def v (g/compute_network \"a\" {{}}))\n(g/compute_subnetwork \"s\" {{:network v}})",
        NS
    ));
    assert!(e.contains("K0203") && e.contains("(:id x)"), "{}", e);
}

#[test]
fn syntax_errors_have_positions() {
    let e = compile("(def x\n  [1 2", "t.kiln").unwrap_err();
    assert!(e.contains("K0001") && e.contains("t.kiln:2:3"), "{}", e);
}
