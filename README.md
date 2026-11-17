# Kiln

A Lisp that compiles to Terraform JSON. Terraform or OpenTofu stays the engine
(providers, plan, apply, state); Kiln replaces HCL and Terragrunt as the thing you
write. The generated `main.tf.json` has no dependency on Kiln: stop using Kiln on any
day and keep the files.

- [DESIGN.md](DESIGN.md): the language Kiln is heading for, and why.
- [ROADMAP.md](ROADMAP.md): milestones, their gates, and where things stand.
- [ERRORS.md](ERRORS.md): every diagnostic code; `kiln explain K0312` prints one.

## Status

M0 to M2 are done: the language and its evaluator, address safety (`kiln.lock`,
`moved` blocks, `kiln import`), and checking against provider schemas. M3 (types,
staging, secrecy) has met its gate and has one step left, typed modules. Stacks and
contexts, the replacement for Terragrunt, are M4. Details in [ROADMAP.md](ROADMAP.md).

## Use

```sh
cargo build --release
kiln schema examples/gke/main.kiln        # cache the providers' schemas (runs tofu)
kiln build  examples/gke/main.kiln        # writes out/main.tf.json and kiln.lock
kiln build  examples/gke/main.kiln --check --strict    # in CI
kiln types  examples/gke/main.kiln        # inferred types and type errors
kiln expand examples/gke/main.kiln        # the program with macros expanded
kiln explain K0312                        # what a diagnostic means
kiln import path/to/terraform -o path/to/stack
kiln mv stack/main.kiln google_x.old google_x.new

./check.sh               # tests, schemas, tofu init/validate, import round trip
PLAN=1 ./check.sh        # also: generated and imported plans equal the HCL plans
tools/move-test.sh       # applies `random` resources locally to prove state moves
```

Commit `main.kiln`, `kiln.lock`, `out/main.tf.json` and `.kiln/schemas/` together.

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
- **Reading an attribute of a resource gives a value known only after apply**: a
  Terraform expression. `str`, arithmetic, comparisons, `not`, `get` and value-position
  `if` accept such values and compile to Terraform expressions. `(tf/name args...)`
  calls any Terraform function; `(tf/expr "...")` embeds a raw expression.
- **Such a value cannot shape the graph.** Branching on one to declare a resource,
  iterating over one, or naming a resource with one is a compile error, found in every
  function and branch, including code that no build runs.
- **When only the engine knows.** `(count-deferred cond body...)` and
  `(for-each-deferred [x coll] body...)` give every declaration in the body a real
  `count` or `for_each`, with `x` as `each.value`. References to those resources come
  out as `one(x[*].a)`, `x[each.key].a` inside the block, or a keyed map outside.
  See `examples/deferred`.
- **Secrets stay secret.** Reading a sensitive attribute marks the value secret,
  through any computation. It may flow only into sensitive arguments and sensitive
  outputs. `(tf/nonsensitive x)` declassifies on purpose; `(secret x)` marks a value.
- **No forward references; only structural recursion.** A definition sees only what
  precedes it, and a function may call itself only on a strictly smaller part of one
  of its parameters: `(rest xs)`, a destructured part, `(:children node)`, an element.
  Every build terminates. Counting down (`(f (- n 1))`) is not structural; use `range`.
- **Keyword and string keys are interchangeable for lookup**: `(:a m)` finds `"a"`.
- **Special forms:** `def defn defmacro ns` (top level only), `fn let if do for and or
  quote`, `data provider output backend module scope count-deferred for-each-deferred`.
  `when`, `when-not`, `cond` and `defcomponent` are macros in `src/prelude.kiln`.

## What the compiler checks

Before anything is written, and with every problem reported together at the place
that caused it:

- **Provider schemas.** Every resource, data source and provider block against its
  provider's schema: resource types, argument names at any depth (with spelling
  suggestions), required and read-only arguments, value types, nested blocks and how
  many of them. Also attribute reads, down into nested blocks.
- **Stages.** No value known only after apply decides whether a resource exists, what
  is iterated, or what anything is called.
- **Secrets.** No sensitive value reaches an argument, output, `count` or `for_each`
  that is not sensitive.
- **Recursion.** Every recursive function shrinks a parameter, so every build stops.
- **Addresses.** No resource is silently renamed into a destroy and create.

Each of these runs twice. Evaluation checks what a build actually produces.
Type inference, which runs on every top-level form before it is evaluated, checks
every function and branch, so a mistake in a helper nobody calls yet, or in the
`prod` branch while you build `dev`, is found as well. Inference is gradual: where it
cannot be precise it gives up quietly rather than complain about valid code. Shape
mismatches it finds are shown by `kiln types` but not enforced yet.

Every code is listed in [ERRORS.md](ERRORS.md), which is generated from
`src/codes.rs`; `kiln explain <code>` prints an entry.

## Provider schemas

Schemas live in `.kiln/schemas/<namespace>/<provider>.json`: one provider at one
version, descriptions stripped, one line per resource type. The nearest `.kiln`
directory at or above a stack applies. `kiln schema <file.kiln>` fetches them with the
engine (`$TOFU`, default `tofu`) for the providers in the stack's `ns`;
`kiln schema --from schema.json --lock .terraform.lock.hcl` converts output you
already have. The cache is committed, so builds need neither the engine nor the
network. A provider without a cached schema is a warning, or an error with
`--strict`. A cached schema whose version does not satisfy the stack's constraint is
not used. One version per provider per repository, for now.

## Renaming without destroying

Kiln flattens components into resources, so a refactoring can change an address, and
Terraform reads a changed address as destroy and create. `kiln.lock` records every
address a stack has emitted, and each build diffs against it.

```clojure
(cluster ^{:was "primary"} "main" ctx {...})        ; a component instance
(g/compute_network ^{:was "vpc"} "network" {...})   ; a single resource
```

- A name annotated with `:was` yields a `moved` block for every address under it.
  The annotation can be deleted once built; the ledger keeps the move.
- `kiln mv <file> <from> <to>` declares the same thing without touching the source.
- If an address disappears while another of the same type appears and nothing
  declares the rename, the build stops and proposes pairings ranked by how alike the
  arguments are. `--replace` says they really are a deletion and a creation.
- Plain additions and deletions pass through; the plan shows them as usual.

## Importing HCL

`kiln import <dir> -o <dir>` reads every `.tf` file, writes `main.kiln`, and builds it.
Addresses are unchanged, so the first plan against existing state is a no-op.

- Declarations are reordered: HCL is unordered, Kiln has no forward references.
- Locals, and variables with a default, become compile-time definitions.
- Expressions with a Kiln counterpart are translated (`"${a}-x"` to `(str a "-x")`,
  references to `(:attr resource)`, operators, conditionals, function calls). The
  rest (`for` expressions, splats, `each.value`, `path.module`) are carried over as
  `(tf/expr "...")`, with compile-time values spliced in.
- Outside the subset, and reported rather than guessed at: `dynamic` blocks,
  variables without a default, provisioners, `moved` / `import` / `check` blocks,
  template directives. Comments are not carried over.

## Layout

```
src/reader.rs     text -> values (code is data)
src/eval.rs       evaluator, macro expander, declarations, count/for_each forms
src/builtins.rs   built-in functions and lifting over deferred values
src/tf.rs         Terraform expression rendering, value -> Terraform JSON
src/emit.rs       main.tf.json
src/ledger.rs     kiln.lock: address diffing, moved blocks, rename guard
src/import.rs     HCL -> Kiln
src/schema.rs     schema cache, conversion, version constraints
src/check.rs      checking evaluated declarations against schemas, secret flows
src/infer.rs      type inference: gradual shapes; stage, effect and secrecy graph
src/recursion.rs  the structural recursion check
src/codes.rs      every diagnostic code, the source of ERRORS.md
src/prelude.kiln  library macros
examples/gke/             source, lock, committed output, hand-written HCL original
examples/deferred/        count-deferred and for-each-deferred
examples/import-offline/  HCL fixture for the importer, credential-free providers
tests/fixtures/move/      three versions of one stack for tools/move-test.sh
.kiln/schemas/            cached provider schemas
docs/M3.md                the M3 plan and its status
```

## License

MIT. See [LICENSE](LICENSE).
