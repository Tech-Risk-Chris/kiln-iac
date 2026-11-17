# Handoff, 17 November 2026

Kiln was built M0 to M3 in one long Claude chat session, with files written in a
cloud workspace and copied into this repository. Work continues in Claude Code,
directly in the repository. This note is what that session knew and the repository
did not yet say. Standing rules are in `CLAUDE.md`.

## Where things stand

- **M0 to M3 are done** (see `ROADMAP.md`). Last commit: M3 step 7, typed modules.
- **Tests:** 103 across `tests/lang.rs`, `m1.rs`, `m2.rs`, `m3.rs`, `golden.rs`,
  `codes.rs`. All pass; `./check.sh` should report 0 failures.
- **Examples:** `gke` (with its HCL original, plans identically), `deferred`
  (`count-deferred`, `for-each-deferred`), `modules` (a local HCL module with
  checked inputs and outputs), `import-offline` (an HCL fixture for the importer).
- **Engine:** OpenTofu 1.13.9 at `/opt/homebrew/bin/tofu`. Kiln targets
  Terraform/OpenTofu 1.7 semantics.

## Decisions already made (do not reopen without the user)

From the design phase:

- Components flatten into the parent stack for v1; an address is the scope
  segments joined by `__`.
- Cross-stack references go through remote state at first.
- S-expressions only; no alternative surface syntax yet.
- Generated output is deterministic and committed, stable across minor versions.
- Structural recursion before general recursion.

From M3 (17 November):

- **A sound core with a gradual penumbra.** Stage (known now vs. known after
  apply), secrecy and effect are sound: a violation is an error, also in code that
  never runs. Shapes are gradual: inference gives up quietly rather than reject
  valid code. Shape clashes are shown by `kiln types` but not enforced.
- Module interfaces are cached as schema blocks (`kiln-module/1` in
  `.kiln/modules/`), so the resource checks apply to them unchanged. Module outputs
  are typed `dynamic`, since HCL outputs declare no type.
- A local module `:source` is written relative to the stack file; the compiler
  rewrites it relative to the output directory when emitting.

## How the pieces fit (the parts that take longest to rediscover)

- **Deferred values.** A value known only after apply is `Value::Deferred(TfExpr)`.
  Operators and built-ins lift over it, producing Terraform expressions. Wrappers:
  `Secret` (taint), `Shaped` (carries a schema for nested reads), and
  `Instance(addr, Count | EachKey | All)` for reading instances of resources with
  `count` or `for_each`.
- **Inference** (`infer.rs`) builds one constraint graph per top-level form:
  sources (attributes, secrets), sinks (names, `count`, `for_each`,
  non-sensitive arguments), and edges through every function. `settle()` reports
  stage errors once per cause and secret errors for every leak. Each function's
  type scheme carries its part of the graph, so a leak through a helper is found at
  the call.
- **Schema checks** live in `check.rs` (evaluated) and `Infer::check_args` /
  `check_type` (static). Both read the same cached schema JSON.
- **Duplicates:** when evaluation and inference report the same code at the same
  position, the evaluated error wins.

## Gotchas met so far

- A cached schema whose version does not satisfy the stack's constraint must never
  be used, in any code path (`peek_schema` once forgot this).
- `tests/golden.rs` builds strictly: a new example needs its provider schemas and
  module interfaces cached and committed, or it fails with K0712 or K0714.
- Some checks only fire when a provider's schema is cached. `check.sh` fetches
  missing schemas (for example `random`), so it can find false positives that
  `cargo test` alone, run without them, does not.
- `ERRORS.md` is generated; edit `src/codes.rs` and bless.

## Next: M4, stacks and contexts

Scope and gate are in `ROADMAP.md`; the design is "Stacks and contexts" in
`DESIGN.md`. In short: `defcontext` with explicit `:extends`, `defstack`, typed
`(ref stack)` compiled to `terraform_remote_state` plus the outputs the reader
needs, `deploy` writing `out/<context>/<stack>/main.tf.json` and `out/graph.json`,
`kiln run plan|apply` walking the graph, and a GitLab child-pipeline adapter.
Gate: a Terragrunt repository with three environments is replaced and its
directory tree deleted.

Open questions to settle with the user before building:

1. **Which Terragrunt repository is the gate?** Ideally a real (or realistic,
   anonymised) client layout; the gate is only as good as the corpus.
2. **Context rows.** M3 inference can already infer the row a stack reads from
   `ctx`; decide whether a missing key is an error per context at build time
   (the design says yes) and how multiple parents resolve conflicts.
3. **`kiln run apply`.** It is the first command that applies. Decide its
   guardrails (confirmation, plan-file only, refusing without an explicit
   context) before writing it, and keep it out of `check.sh` and the tests.
4. **Layout change.** One stack per file today, `out/` beside it. M4 makes the
   output tree per context and per stack; existing examples and `kiln.lock`
   placement must keep working or migrate deliberately.

A good first step: write `docs/M4.md` as a plan with numbered steps and a status
table, in the style of `docs/M3.md`, and agree it with the user before code.
