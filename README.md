# Kiln

A Lisp that compiles to Terraform JSON. Terraform or OpenTofu stays the engine
(providers, plan, apply, state); Kiln replaces HCL and Terragrunt as the thing
you write. The generated `main.tf.json` has no dependency on Kiln.

Design document: *Kiln: a typed Lisp that compiles to Terraform*.

## Status: M1

M0 (reader, evaluator, macros, untyped emitter) and M1 (address ledger, `moved`
blocks, `kiln import`) are done.

| Done | Not yet |
| --- | --- |
| Resources, data sources, modules, providers, outputs, backend | Types from provider schemas (M2) |
| Functions, `let`, `for`, destructuring, components | Static `Deferred` / `Secret` checking (M3); staging is checked dynamically |
| Macros with quasiquote and auto-gensym | Hygiene beyond gensym (M5) |
| Deferred values that lift through `str`, arithmetic, comparison, `if`, `get` | Stacks, contexts, cross-stack references (M4) |
| Deterministic output, `--check` | Pruning `moved` blocks once every state has applied them (M4) |
| `kiln.lock`: renames become `moved` blocks; undeclared renames stop the build | |
| `kiln import`: HCL to Kiln with addresses preserved | |

## Use

```sh
cargo build --release
kiln build examples/gke/main.kiln            # writes out/main.tf.json and kiln.lock
kiln build examples/gke/main.kiln --check    # CI: fail if either is stale
kiln expand examples/gke/main.kiln           # program with macros expanded
kiln import path/to/terraform -o path/to/stack
kiln mv stack/main.kiln google_x.old google_x.new

./check.sh               # tests, tofu init/validate, import round trip
PLAN=1 ./check.sh        # also: generated and imported plans equal the HCL plans
tools/move-test.sh       # applies `random` resources locally to prove state moves
```

Commit `main.kiln`, `kiln.lock` and `out/main.tf.json` together.

## Renaming without destroying

Kiln flattens components into resources, so a refactoring can change an address,
and Terraform reads a changed address as destroy-and-create. `kiln.lock` records
every address a stack has emitted, and each build diffs against it.

```clojure
(cluster ^{:was "primary"} "main" ctx {...})        ; a component instance
(g/compute_network ^{:was "vpc"} "network" {...})   ; a single resource
```

- A name annotated with `:was` yields a `moved` block for every address under it.
  The annotation can be deleted once built; the ledger keeps the move.
- `kiln mv <file> <from> <to>` declares the same thing without touching the source.
- If an address disappears while another of the same type appears and nothing
  declares the rename, the build stops (K0500) and proposes pairings ranked by how
  alike the arguments are. `--replace` says they really are a deletion and a creation.
- Plain additions and deletions pass through; the plan shows them as usual.

## Importing HCL

`kiln import <dir> -o <dir>` reads every `.tf` file, writes `main.kiln`, and builds
it. Addresses are unchanged, so the first plan against existing state is a no-op.

- Declarations are reordered: HCL is unordered, Kiln has no forward references.
- Locals, and variables with a default, become compile-time definitions.
- Expressions with a Kiln counterpart are translated (`"${a}-x"` to `(str a "-x")`,
  references to `(:attr resource)`, operators, conditionals, function calls).
  The rest (`for` expressions, splats, `each.value`, `path.module`) are carried
  over as `(tf/expr "...")`, with compile-time values spliced in.
- Outside the subset, and reported rather than guessed at: `dynamic` blocks,
  variables without a default, provisioners, `moved` / `import` / `check` blocks,
  template directives. Comments are not carried over.

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
- **Keyword and string keys are interchangeable for lookup**: `(:a m)` finds `"a"`.
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
src/ledger.rs     kiln.lock: address diffing, moved blocks, rename guard
src/import.rs     HCL -> Kiln
src/prelude.kiln  library macros
examples/gke/             source, lock, committed output, hand-written HCL original
examples/import-offline/  HCL fixture for the importer, credential-free providers
tests/fixtures/move/      three versions of one stack for tools/move-test.sh
```
