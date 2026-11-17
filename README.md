# Kiln

A Lisp that compiles to Terraform JSON. Terraform or OpenTofu stays the engine
(providers, plan, apply, state); Kiln replaces HCL and Terragrunt as the thing
you write. The generated `main.tf.json` has no dependency on Kiln.

Design: [DESIGN.md](DESIGN.md). Milestones and status: [ROADMAP.md](ROADMAP.md).

## Status: M2, M3 under way

M0 (reader, evaluator, macros, emitter), M1 (address ledger, `moved` blocks,
`kiln import`) and M2 (checking against provider schemas) are done. Of M3, secrecy
tracking and structural recursion are done; see [ROADMAP.md](ROADMAP.md).

| Done | Not yet |
| --- | --- |
| Resources, data sources, modules, providers, outputs, backend | Static types inferred from the source (M3) |
| Functions, `let`, `for`, destructuring, components | Static `Deferred` / `Secret` checking (M3); staging is checked dynamically |
| Macros with quasiquote and auto-gensym | Hygiene beyond gensym (M5) |
| Deferred values that lift through `str`, arithmetic, comparison, `if`, `get` | Stacks, contexts, cross-stack references (M4) |
| Deterministic output, `--check` | Pruning `moved` blocks once every state has applied them (M4) |
| `kiln.lock`: renames become `moved` blocks; undeclared renames stop the build | Typing module inputs and outputs |
| `kiln import`: HCL to Kiln with addresses preserved | |
| Declarations checked against cached provider schemas | |

## Use

```sh
cargo build --release
kiln schema examples/gke/main.kiln           # cache provider schemas (runs tofu)
kiln build examples/gke/main.kiln            # writes out/main.tf.json and kiln.lock
kiln build examples/gke/main.kiln --check --strict   # CI
kiln expand examples/gke/main.kiln           # program with macros expanded
kiln import path/to/terraform -o path/to/stack
kiln mv stack/main.kiln google_x.old google_x.new

./check.sh               # tests, schemas, tofu init/validate, import round trip
PLAN=1 ./check.sh        # also: generated and imported plans equal the HCL plans
tools/move-test.sh       # applies `random` resources locally to prove state moves
```

Commit `main.kiln`, `kiln.lock`, `out/main.tf.json` and `.kiln/schemas/` together.

## Checking against provider schemas

Every resource, data source and provider block is checked against its
provider's schema before anything is written:

| Code | Problem |
| --- | --- |
| K0700 | unknown resource type or data source, with a spelling suggestion |
| K0701 | unknown argument, at any depth, with a spelling suggestion |
| K0702 | missing required argument or block |
| K0703 | setting an attribute the provider computes |
| K0704 | wrong value type: `"1460"` for a number, a string for a set |
| K0705 | a nested block given as something other than a map |
| K0706 | too many or too few nested blocks |
| K0707 | reading an attribute that does not exist, e.g. `(:self_lnk vpc)`, also inside nested blocks |
| K0708 | reading an attribute from a list or set of blocks without picking one |
| K0711 | the cached schema's version does not satisfy the stack's constraint |
| K0712 | no cached schema for a provider (a warning; an error with `--strict`) |
| K0750 | a deprecated argument (a warning) |
| K0320 | a secret flowing into an argument that is not sensitive, or into `count` / `for_each` |
| K0321 | an output that exposes a secret without `:sensitive true` |
| K0322 | `(secret x)` on a value written in the source |

All problems are reported together, each at the declaration that caused it.
A value that is a Terraform expression passes: its type is known only to the
engine. That gap, and errors in code paths that did not run, close with M3.

Schemas live in `.kiln/schemas/<namespace>/<provider>.json`: one provider at
one version, descriptions stripped, one line per resource type. The nearest
`.kiln` directory at or above a stack applies. `kiln schema <file.kiln>` fetches
them with the engine (`$TOFU`, default `tofu`) for the providers in the stack's
`ns`; `kiln schema --from schema.json --lock .terraform.lock.hcl` converts output
you already have. The cache is committed, so builds need neither the engine
nor the network. One version per provider per repository, for now.

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
- **No forward references; only structural recursion.** A definition sees only what
  precedes it, and a function may call itself only on a strictly smaller part of one
  of its parameters: `(rest xs)`, a destructured part, `(:children node)`, an element.
  Every build terminates. Counting down (`(f (- n 1))`) is not structural; use `range`.
- **Secrets stay secret.** Reading a sensitive attribute marks the value secret, through
  any computation. It may flow only into sensitive arguments and sensitive outputs.
  `(tf/nonsensitive x)` declassifies on purpose.
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
src/schema.rs     schema cache, conversion, version constraints
src/check.rs      checking declarations against schemas, secret flows
src/recursion.rs  the structural recursion check
src/prelude.kiln  library macros
examples/gke/             source, lock, committed output, hand-written HCL original
examples/import-offline/  HCL fixture for the importer, credential-free providers
tests/fixtures/move/      three versions of one stack for tools/move-test.sh
.kiln/schemas/            cached provider schemas (Google 6.50.0)
```

## License

MIT. See [LICENSE](LICENSE).
