# Kiln

A typed Lisp that compiles to Terraform JSON. Terraform or OpenTofu stays the
engine; Kiln replaces HCL and Terragrunt. The compiler is Rust (`src/`), one crate,
two dependencies (`hcl-rs`, `serde_json`).

Read before changing anything: `DESIGN.md` (why), `ROADMAP.md` (milestones and
gates), `docs/HANDOFF.md` (where work stands), `ERRORS.md` (every diagnostic code).

## Safety: this machine has live GCP credentials

- **Never run `tofu apply`, `terraform apply`, `destroy`, or `import` against
  anything.** `check.sh` only inits, validates and (with `PLAN=1`) plans; keep it
  that way.
- Do not run `PLAN=1 ./check.sh` or any `plan` unless the user asks: plans call
  the GCP APIs with the user's credentials.
- The one script that applies is `tools/move-test.sh`. It applies only `random`
  provider resources into a local state file under `.kiln-check/move/`. Do not add
  any other provider to it.
- Example stacks name projects like `acme-dev`. They are fixtures, not real
  infrastructure; never point them at real projects.

## Build and test

```
cargo build --release          # ./target/release/kiln
cargo test                     # all suites; must pass before any commit
./check.sh                     # engine checks: build, test, schemas, init + validate every example and import
tools/move-test.sh             # state-move test (applies `random` only, see above)
```

`check.sh` writes `.kiln-check/report.txt`; read it instead of asking for output.

Regenerate `ERRORS.md` after touching `src/codes.rs`:
`KILN_BLESS=1 cargo test --test codes`. The codes test fails if a code is used in
`src/` but not registered, registered but unused, or `ERRORS.md` is stale.

After changing an example or anything that affects output:
`./target/release/kiln build examples/<name>/main.kiln --strict`, then commit
`out/main.tf.json` and `kiln.lock`. `tests/golden.rs` builds every
`examples/*/main.kiln` strictly and fails on stale output, any warning, or any
type clash.

## Conventions

- **Output is deterministic and committed.** Same source, same bytes. Never let
  map order, timestamps or absolute paths leak into `out/` or `kiln.lock`.
- **Builds are hermetic.** Provider schemas (`.kiln/schemas/`) and module
  interfaces (`.kiln/modules/`) are cached and committed; `kiln build` never needs
  the engine or the network. `kiln schema <file.kiln>` refreshes them.
- **Every diagnostic has a code** registered in `src/codes.rs` with a title and an
  explanation. Errors point at the source position that caused them and, where
  possible, suggest the fix (`help:`).
- **Checks run twice**: during evaluation (`eval.rs` + `check.rs`, on what a build
  produces) and during inference (`infer.rs`, on every form, including code that
  never runs). A new check usually needs both, and tests for both: one evaluated,
  one inside `(defn never [] ...)`. When both report the same code at the same
  position, the evaluated one wins.
- **Inference is gradual for shapes, sound for stage, secrecy and effect.** A
  shape check must never reject valid code: when unsure, give up quietly (`Dyn`).
  `tests/m3.rs` has an `IDIOMS` list and a test that the examples and imports have
  no type errors; extend them when adding a check.
- **Tests are named as sentences** about behaviour
  (`sensitive_module_outputs_are_secrets`) and live in the milestone's file
  (`tests/m4.rs` for M4).
- **No new dependencies** without asking.
- Keep the Rust plain. The maintainer reads Rust but is not a Rust specialist:
  prefer explicit code to clever generics or macros, and explain non-obvious Rust
  in the commit message.
- Docs are part of a change: update `README.md`, `ROADMAP.md` and the milestone's
  `docs/M<n>.md` status table when a step lands.

## Git

- Commit only when asked. Message format used so far:
  `M3 step 7: typed modules; M3 done`.
- Do not commit `.terraform/`, `.terraform.lock.hcl` under `examples/`,
  `.kiln-check/`, `.kiln/work/`, state files or `plan.bin` (`.gitignore` covers
  them).
- The user's email address is private; do not put it in files or commit metadata.

## Layout

```
src/reader.rs     text -> values
src/eval.rs       evaluator, macro expander, declarations, count/for_each forms, modules
src/builtins.rs   built-in functions, lifting over deferred values
src/tf.rs         Terraform expression rendering
src/emit.rs       main.tf.json
src/ledger.rs     kiln.lock, moved blocks, rename guard
src/import.rs     HCL -> Kiln
src/schema.rs     schema and module-interface caches, version constraints
src/check.rs      checking evaluated declarations against schemas, secret flows
src/infer.rs      type inference: gradual shapes; stage, effect and secrecy graph
src/recursion.rs  structural recursion check
src/codes.rs      diagnostic codes, source of ERRORS.md
src/main.rs       CLI: build, schema, import, mv, expand, types, explain
src/prelude.kiln  library macros
```
