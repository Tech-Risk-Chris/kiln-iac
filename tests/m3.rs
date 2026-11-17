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
