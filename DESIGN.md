# Kiln: a typed Lisp that compiles to Terraform

Nov 16, 2026 · Robert C. Kahlert

> Copied from the design doc of the same name. Where the implementation has diverged, this file is out of date until updated: M2 became checking against provider schemas, with static type inference moved to M3 together with staging and secrecy; M0 to M2 have no recursion at all; staging is checked during evaluation, not by types. The README has the current status.

## Thesis

Kiln is a pure, statically typed Lisp whose only output is Terraform JSON (`*.tf.json`). Terraform or OpenTofu remains the engine: it owns providers, plan, apply and state. Kiln owns everything HCL is bad at: abstraction, types, environments, stack boundaries and refactoring.

Three decisions carry the design:

1. **A compiler, not a runtime.** The output is plain Terraform with no Kiln dependency. You can stop using Kiln on any day and keep the generated files.
2. **Two phases, visible in the types.** Values known at compile time get the full language. Values known only after apply get a restricted expression language that compiles to Terraform expressions. The type checker enforces the border.
3. **Addresses are a managed artifact.** The compiler records every resource address it has emitted and generates `moved` blocks when a refactoring changes one. Abstraction stops being a state-surgery risk.

The cautionary precedent is CDKTF, which HashiCorp sunset and archived on 10 December 2025, citing no product-market fit at scale ([repository notice](https://github.com/hashicorp/terraform-cdk)). It put a general-purpose runtime, pre-generated bindings in five languages and a synth step between the author and Terraform. Kiln avoids all three: one small language, bindings derived from the provider schema at compile time, and output a Terraform user can read.

## Diagnosis

HCL's problems are structural, not cosmetic: it is a data format that acquired half a language, and Terragrunt is a text preprocessor bolted on to compensate.

| Pain | Root cause | Kiln's answer |
| --- | --- | --- |
| `count = var.x ? 1 : 0`, `dynamic` blocks, `for_each` over `toset()` | Control flow encoded as resource meta-arguments | Ordinary `if`, `for`, `when` at compile time |
| Modules need a `variable` and an `output` for every value crossing the boundary | The only unit of abstraction is a directory | Functions; a component is a function that returns resources |
| No user-defined functions; logic lives in `locals` spaghetti | Expression language without abstraction | First-class functions, `let`, pattern destructuring |
| Type errors surface at plan, or at apply | Provider schemas are checked late; object types are opt-in | Types derived from provider schemas, checked before any Terraform call |
| "Invalid for\_each argument: value depends on resource attributes" | Phase distinction exists but is invisible in the language | `Deferred` types; the error becomes a compile error with a source location |
| Renaming or re-nesting a module destroys and recreates resources | Address is a side effect of file layout | Address ledger with generated `moved` blocks |
| Terragrunt `include`, `find_in_parent_folders`, `generate` | Configuration inheritance done by path convention and text templating | Contexts: typed, named, explicitly inherited values |
| Terragrunt `dependency` with hand-maintained `mock_outputs` | Cross-state references are untyped strings | Typed stack references; the compiler wires both ends and the run order |

## Borrowed ideas

Each feature is taken from a language that already proved it; nothing here is novel except the combination.

| Source | Idea taken | What it does in Kiln |
| --- | --- | --- |
| Clojure | S-expressions with map, vector and keyword literals | Syntax. Infrastructure is mostly nested maps; Terraform JSON is already a tree |
| Racket / Scheme | Hygienic macros, `syntax-parse`-style error reporting | New binding forms and team DSLs without capture bugs |
| ML, PureScript | Hindley-Milner inference with row-polymorphic records | Resource arguments are records; functions over "anything with a `:name` and `:location`" type-check without annotations |
| F# | Type providers | Resource types come from `terraform providers schema -json` at compile time; no checked-in generated bindings |
| MetaML, Zig `comptime` | Staged computation | The compile-time / apply-time split |
| Haskell | Applicative vs. monad | A `Deferred` value can be mapped and combined, but never inspected to decide structure |
| Dhall, Nix | Purity, hermetic evaluation, hash-pinned imports | Same source and lockfile give byte-identical output; no ambient environment variables or network |
| Rust | Affine-style tracking, `cargo`, editions, compiler diagnostics | `Secret` taint tracking, one toolchain binary, errors that show the fix |
| Datalog | Rules over a fact base | Policy as queries over the compiled resource graph |
| Cyc | Microtheories with `genlMt` inheritance | Contexts: an environment is a set of assertions visible from its specialisations |

Deliberately not borrowed: CUE's unification (elegant, but merge-by-unification makes "where does this value come from" hard to answer), Dhall's full totality discipline (v1 takes only structural recursion, which gives termination without the rest), and general-purpose host languages as in Pulumi or CDKTF (too much power in the wrong phase).

## The language by example

A resource is a function call that returns a typed handle; a reusable unit is a function that returns handles. That is the whole abstraction story.

```clojure
(ns platform.gke
  (:provider [google :as g :version "~> 6.0"]))

;; Plain function. No variable blocks, no outputs.
(defn labels [ctx extra]
  (merge {:env (:env ctx) :managed_by "kiln"} extra))

;; Component: the instance name becomes an address segment.
(defcomponent cluster
  [ctx {:keys [region network pools]}]
  (let [c (g/container_cluster "this"
            {:name     (str (:prefix ctx) "-" (:env ctx))
             :location region
             :network  (:id network)
             :remove_default_node_pool true
             :initial_node_count 1
             :resource_labels (labels ctx {})})]
    {:cluster  c
     :endpoint (:endpoint c)            ; Deferred String
     :pools
     (for [p pools]
       (g/container_node_pool (:name p)
         {:cluster  (:name c)
          :location region
          :autoscaling {:min_node_count (:min p)
                        :max_node_count (:max p)}
          :node_config {:machine_type (:machine p)
                        :spot (get p :spot false)}}))}))

;; Use site
(cluster "primary" ctx
  {:region  "europe-west3"
   :network vpc
   :pools   [{:name "default" :machine "e2-standard-4" :min 1 :max 5}
             {:name "batch"   :machine "c3-highcpu-8"  :min 0 :max 20 :spot true}]})
```

This emits `google_container_cluster.primary__this` and two node pools, `primary__default` and `primary__batch`, as three flat resources.

The rules the example relies on:

- **Constructors come from the schema.** `g/container_cluster` exists because the pinned provider declares it. A misspelt argument, a missing required one, or a string where a number belongs is a compile error.
- **Provider names are kept verbatim.** `:remove_default_node_pool`, not `:remove-default-node-pool`. Being able to grep the provider documentation beats Lisp naming taste.
- **Reading an attribute creates the dependency.** `(:name c)` compiles to `${google_container_cluster.primary__this.name}`. Explicit `depends_on` is needed only for dependencies Terraform could not see either.
- **Nested blocks are nested data.** The schema tells the compiler whether `:autoscaling` is a block or an attribute. `dynamic` has no counterpart because `for` and `when` work inside any map.
- **Compile-time loops unroll.** Each pool is its own resource addressed by its name, so the plan reads as a list of named things and removing the first element does not renumber the rest.
- **Existing Terraform modules remain usable.** `(module "gke" {:source "..." :version "..."} inputs)` type-checks `inputs` against the module's declared variables and exposes its outputs as `Deferred` fields.
- **There is an escape hatch.** `(tf/expr "..." :as String)` embeds a raw Terraform expression at a type you assert; `(tf/raw {...})` embeds raw JSON. Both are greppable and lintable.

## Staging: known vs. deferred

Every value is either known at compile time (`T`) or known only after apply (`Deferred T`), and the type checker decides what you may do with each. This is the single most important idea in the design.

- **Where `Deferred` comes from.** The provider schema marks attributes as computed. Reading one yields `Deferred T`. Cross-stack references and data sources also yield `Deferred`.
- **What you can do with it.** A fixed set of liftable operations (`str`, arithmetic, comparison, `get`, value-position `if`, and Terraform built-ins such as `tf/cidrsubnet`) accept `Deferred` arguments and return `Deferred` results. They compile to Terraform expressions. With static arguments the same operations constant-fold.
- **What you cannot do with it.** Decide whether a resource exists, iterate to create resources, or form a logical name. Those shape the graph, and the graph must be known at compile time.
- **The explicit exits.** `(count-deferred cond ...)` and `(for-each-deferred [x xs] ...)` emit real `count` and `for_each`. They are spelled out so that every plan-time-unknown structure in a codebase can be found with grep.

In Haskell terms `Deferred` is an applicative functor with no monadic bind: you can combine effects, you cannot let one effect's result choose the next. Terraform has always had this restriction; HCL just reports it late.

```text
error[K0312]: resource existence depends on a value known only after apply
  --> platform/dns.kiln:14:9
   |
14 |   (when (:private_endpoint c)
   |         ^^^^^^^^^^^^^^^^^^^^^ Deferred Bool, computed by
   |                               google_container_cluster.primary__this
   = help: branch on a compile-time value such as (:private? cfg),
           or use (count-deferred ...) to emit a Terraform count
```

`Secret T` is a second, orthogonal marker, seeded from `sensitive` in the schema. A secret may flow only into sensitive arguments and sensitive outputs; `(str "prefix-" secret)` is itself a secret. Putting a database password into a label is a type error.

## Stable addresses

A resource's Terraform address is its type plus the chain of component instance names plus its logical name, and the compiler treats changes to that address as migrations. Without this, every abstraction feature above would be a liability: flattening functions into resources means any refactoring can rename something in state.

- **Derivation.** `(cluster "primary" ...)` containing `(g/container_cluster "this" ...)` gives `google_container_cluster.primary__this`. Plain `defn` adds no segment, so extracting a helper function never changes an address.
- **Ledger.** `kiln.lock` records every address emitted per stack. Each build diffs against it.
- **Declared renames.** `(g/container_cluster ^{:was "main"} "this" ...)` or `kiln mv old new` produces a `moved` block. The ledger keeps emitting it until every context reports the move applied, then prunes it.
- **Undeclared renames fail the build.** If one address of a type disappears and another of the same type appears in the same build, the compiler stops and proposes the `moved` pairing, ranked by argument similarity. It proposes; it never assumes.
- **Cross-stack moves.** `moved` cannot cross state files. Kiln generates the `terraform state mv` script and the matching ledger update, and refuses to build the two stacks in a half-moved state.

Pure deletions pass through untouched; the plan shows them as usual.

## Stacks and contexts

A stack is one state file; a context is one environment; both are first-class values, and the directory tree stops carrying meaning. This replaces Terragrunt entirely.

```clojure
(defcontext base
  {:prefix "acme" :region "europe-west3"
   :pools  [{:name "default" :machine "e2-standard-4" :min 1 :max 3}]})

(defcontext dev  :extends base {:env "dev"  :project "acme-dev"})
(defcontext prod :extends base
  {:env "prod" :project "acme-prod"
   :pools (conj (:pools base)
            {:name "batch" :machine "c3-highcpu-8" :min 0 :max 20 :spot true})})

(defstack network [ctx]
  {:vpc (g/compute_network "main"
          {:name (str (:prefix ctx) "-" (:env ctx))
           :auto_create_subnetworks false})})

(defstack gke [ctx]
  (let [net (ref network)]            ; {:vpc Deferred ComputeNetwork}
    (gke/cluster "primary" ctx
      {:region (:region ctx) :network (:vpc net) :pools (:pools ctx)})))

(deploy [network gke] :in [dev prod]
  :backend (fn [stack ctx]
             (gcs-backend {:bucket (str (:project ctx) "-tfstate")
                           :prefix (name stack)})))
```

The build writes `out/dev/network/main.tf.json`, `out/dev/gke/main.tf.json`, the same pair for `prod`, and `out/graph.json`.

- **`(ref network)` is typed and two-sided.** The compiler adds exactly the outputs that `gke` reads to `network`, adds a `terraform_remote_state` data source to `gke`, and records the edge. No `mock_outputs`: the type of `network`'s return value is the mock.
- **Contexts inherit explicitly.** A child overrides whole keys. Deep merging happens only where the source calls `merge-deep`. `kiln why prod :pools` prints the chain of definitions that produced a value.
- **Contexts are checked against stacks.** Each stack infers the row type it needs from `ctx`. Adding a `staging` context without `:project` fails with the name of the stack that requires it.
- **Run order is data.** `out/graph.json` is the stack DAG per context. `kiln run plan|apply` walks it locally; thin adapters render it as a GitLab child pipeline or a Makefile.

The context mechanism is microtheories in miniature: a context is a set of assertions, `:extends` is `genlMt`, and a stack is evaluated in exactly one context. Where a value needs to vary on two axes (environment and region), contexts take multiple parents and conflicts must be resolved explicitly at the child.

## Macros

Macros exist for new binding forms and team vocabulary; functions do everything else. The test of the mechanism is that `defcomponent`, `defstack`, `defcontext` and `deploy` are themselves library macros, not compiler built-ins.

```clojure
(defmacro defservice [name & {:keys [roles namespace]}]
  `(defcomponent ~name [ctx]
     (let [sa (g/service_account "sa"
                {:account_id ~(str name) :project (:project ctx)})]
       {:sa sa
        :bindings (for [r ~roles]
                    (g/project_iam_member (slug r)
                      {:project (:project ctx) :role r
                       :member  (str "serviceAccount:" (:email sa))}))
        :workload-identity (workload-identity sa ~namespace ~(str name))})))

(defservice billing-api
  :roles     ["roles/pubsub.publisher" "roles/cloudsql.client"]
  :namespace "billing")
```

Four constraints keep macros from becoming the new `generate` block:

1. **Hygienic by default.** Introduced bindings cannot capture or be captured; breaking hygiene needs an explicit `unhygienic` form.
2. **Expansion precedes type checking.** A macro cannot emit ill-typed Terraform. Errors point at the use site and carry the expansion trace.
3. **No generated names in addresses.** A logical name must derive from the macro's arguments. A gensym reaching an address is a compile error, because it would not survive the next build.
4. **Expansion is inspectable.** `kiln expand file.kiln` prints the macro-free program; the language server shows it inline.

Cross-cutting changes ("every resource that supports labels gets these labels") are not macros. Resources are data after evaluation, so those are ordinary functions from resource graph to resource graph, registered as transforms.

## Compiler and toolchain

One static binary, `kiln`, runs an eight-step pipeline and ships the tools that make generated Terraform debuggable.

1. **Read** source into syntax objects that keep file, line and column.
2. **Expand** macros.
3. **Resolve providers** from `kiln.lock`; fetch each schema once with `terraform providers schema -json` (or `tofu`) and cache it by hash.
4. **Type-check**: inference with row polymorphism, then staging and secrecy.
5. **Evaluate**: pure, strict, terminating by construction, once per stack and context, giving a resource graph.
6. **Transform and check**: run registered graph transforms, then policy rules.
7. **Reconcile addresses** against the ledger and generate `moved` blocks.
8. **Emit** sorted, byte-stable `main.tf.json`, `graph.json` and a source map.

| Command | Purpose |
| --- | --- |
| `kiln build` | Run the pipeline; `--check` fails if committed output is stale |
| `kiln import ./legacy` | Decompile existing HCL into Kiln and seed the ledger so the first plan is a zero diff |
| `kiln explain <address>` | Map a Terraform address back to its source location and call chain |
| `kiln diff <rev> <rev>` | Semantic diff of generated Terraform between two commits, for merge requests |
| `kiln why <ctx> <key>` | Provenance of a context value |
| `kiln expand` / `kiln mv` / `kiln graph` | Macro expansion, declared renames, stack DAG |
| `kiln run plan\|apply` | Walk the DAG, call the engine, rewrite its diagnostics to source positions |
| `kiln test` | Evaluate stacks and assert on the graph; milliseconds, no cloud credentials |

Policy is Datalog over the same graph the emitter sees, so it runs before any plan exists:

```clojure
(defrule no-public-buckets
  :deny "buckets must not grant allUsers"
  [(resource ?m :google_storage_bucket_iam_member)
   (attr ?m :member "allUsers")])
```

Implementation language: Rust. The hard parts are the type checker and the language server, neither of which gains from Go's Terraform libraries, since the only inputs from that ecosystem are a JSON schema and module variable declarations.

## Non-goals and risks

Kiln never touches state, never speaks the provider protocol, and never runs at apply time. It also has no I/O beyond reading declared files: no network, no ambient environment variables, no shelling out.

| Risk | Why it matters | Mitigation |
| --- | --- | --- |
| S-expressions deter operations teams | The largest adoption risk; syntax is judged in the first five minutes | Formatter and language server from the first usable release; `kiln import` so nobody starts from a blank file; the reader is one module, so a second surface syntax stays possible |
| Leaky abstraction at apply | Provider errors name generated addresses and JSON paths | Source maps, `kiln explain`, diagnostic rewriting in `kiln run` |
| Type errors nobody can read | Rows plus staging plus secrecy is a lot of inference | Treat diagnostics as a feature with its own test suite; require annotations at `defcomponent` boundaries |
| Schema holes | Attributes typed `dynamic` (for example `kubernetes_manifest`) defeat the checker | An explicit `Any` type that must be annotated at the use site |
| Terraform and OpenTofu diverge | Features exist in one engine only | Target the shared JSON subset; engine-specific forms live in namespaces that name the engine |
| The project is abandoned | CDKTF users were left with a frozen dependency | Output is standalone Terraform; eject by deleting the source |
| Flattening hides structure | No `-target=module.x`; long resource names | Address prefixes are the component path, so `kiln run --only primary` can compute the target set |

## Roadmap

Six milestones, ordered so that the adoption-critical parts (output shape, address safety, import) are proven before the type system is built.

1. **M0, core.** Reader, evaluator, a basic macro expander, untyped emission for one stack. Gate: a hand-written GKE example plans identically to its HCL original.
2. **M1, address safety.** Ledger, `moved` generation, `kiln import` for a useful HCL subset. Gate: an existing client repository imports to a zero-diff plan and survives a component rename.
3. **M2, types.** Schema-derived constructors, inference with rows, module typing. Gate: every argument error in a seeded test corpus is caught before Terraform runs.
4. **M3, staging and secrecy.** `Deferred`, `Secret`, the explicit deferred forms. Gate: the corpus of known `for_each` and `count` plan-time failures becomes compile errors.
5. **M4, stacks and contexts.** `defstack`, `defcontext`, `ref`, `graph.json`, `kiln run`, one CI adapter. Gate: a Terragrunt repository with three environments is replaced and its directory tree deleted.
6. **M5, hardening.** Macro hygiene, language server, policy rules, transforms, `kiln test`, `kiln diff`. Gate: a second team adopts it without help from the authors.

## Decisions for v1

All seven open questions from the first draft were settled on 16 November 2026.

| Question | Decision | Consequence |
| --- | --- | --- |
| Flatten components or emit modules | Flatten | Generated code has no `module` blocks except for imported third-party modules; `kiln run --only <component>` computes target sets from address prefixes |
| Cross-stack references | `terraform_remote_state` | Upstream stacks must be applied before a downstream plan; orchestrator-fed variables are deferred to a later version |
| Syntax | S-expressions only | No second surface syntax in v1; the reader stays a separate module so one can be added |
| Minimum engine | Terraform 1.7 or the equivalent OpenTofu release | `moved`, `import` and `removed` blocks are all available to the emitter |
| Generated output | Deterministic and committed | Byte-identical output is guaranteed within a Kiln major version; a major upgrade may change the output and lands as one regeneration commit. CI runs `kiln build --check` |
| Recursion | Structural recursion only | Evaluation terminates by construction; no fuel limit. General recursion can be revisited if a real program needs it |
| Name | Kiln | A collision check on package registries and trademarks is still to do |
