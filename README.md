# Kiln

A Lisp that compiles to Terraform JSON. Terraform or OpenTofu stays the engine
(providers, plan, apply, state); Kiln replaces HCL and Terragrunt as the thing
you write. The generated `main.tf.json` has no dependency on Kiln.

Design document: *Kiln: a typed Lisp that compiles to Terraform*.

## Status: M0

Reader, evaluator, macro expander and an untyped emitter for one stack.

| In M0 | Not yet |
| --- | --- |
| Resources, data sources, modules, providers, outputs, backend | Types from provider schemas (M2) |
| Functions, `let`, `for`, destructuring, components | Static `Deferred` / `Secret` checking (M3); M0 checks staging dynamically |
| Macros with quasiquote and auto-gensym | Hygiene beyond gensym (M5) |
| Deferred values that lift through `str`, arithmetic, comparison, `if`, `get` | Address ledger and `moved` blocks (M1) |
| Deterministic output, `--check` | Stacks, contexts, cross-stack references (M4) |

## Use

```sh
cargo build --release
./target/release/kiln build examples/gke/main.kiln     # writes examples/gke/out/main.tf.json
./target/release/kiln build examples/gke/main.kiln --check
./target/release/kiln expand examples/gke/main.kiln    # program with macros expanded
./check.sh                                             # tests + tofu init/validate on every example
```

## The language in one screen

```clojure
(ns platform.gke
  (:provider [google :as g :version "~> 6.0"]))     ; g/x constructs google_x

(defn labels [ctx extra]                             ; a function
  (merge {:env (:env ctx)} extra))

(defcomponent cluster [ctx {:keys [region pools]}]   ; a function with an address scope
  (let [c (g/container_cluster "this" {:name "demo" :location region})]
    {:cluster c
     :pools (for [p pools]                           ; unrolled at compile time
              (g/container_node_pool (:name p)
                {:cluster (:name c)                  ; reference => dependency
                 :location region}))}))

(cluster "primary" ctx {...})    ; google_container_cluster.primary__this, ...
```

- **Names are the provider's.** `:remove_default_node_pool`, `g/container_cluster`.
- **Addresses** are the component instance names plus the logical name, joined by `__`.
- **Nil is omission.** A `nil` argument is left out, so `(when cond value)` works inside a map.
- **Reading an attribute of a resource** gives a deferred value: a Terraform expression.
  `str`, `+ - * /`, comparisons, `not`, `get` and value-position `if` accept deferred
  values and compile to Terraform expressions. `(tf/name args...)` calls any Terraform
  function; `(tf/expr "...")` embeds a raw expression.
- **A deferred value cannot shape the graph.** Branching on one to declare a resource,
  iterating over one, or naming a resource with one is a compile error (K0312 to K0314).
- **No recursion, no forward references.** A definition sees only what precedes it, so
  evaluation always terminates. Iterate with `for`, `map`, `reduce`.
- **Special forms:** `def defn defmacro ns` (top level only), `fn let if do for and or quote`,
  `data provider output backend module scope`. `when`, `when-not`, `cond` and
  `defcomponent` are macros in `src/prelude.kiln`.

## Layout

```
src/reader.rs     text -> values (code is data)
src/eval.rs       evaluator, macro expander, declarations
src/builtins.rs   built-in functions and lifting over deferred values
src/tf.rs         Terraform expression rendering, value -> Terraform JSON
src/emit.rs       main.tf.json
src/prelude.kiln  library macros
examples/gke/     source, committed output, hand-written HCL original
```
