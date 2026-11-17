# Kiln roadmap

Milestones are ordered so that the adoption-critical parts (output shape, address safety, import) are proven before the type system is built. Each has a gate: a concrete result that must hold before the next one starts. The design behind them is in [DESIGN.md](DESIGN.md).

| Milestone | Status | Gate |
| --- | --- | --- |
| M0 Core | Done | Met: the hand-written GKE example plans identically to its HCL original |
| M1 Address safety | Done | Met on the repository's fixtures; a real client repository is still to try |
| M2 Schema checking | Done | Met: every mistake in a seeded corpus of 20 is caught before Terraform runs |
| M3 Types, staging, secrecy | In progress | |
| M4 Stacks and contexts | | |
| M5 Hardening | | |

## M0 Core

Reader, evaluator, macro expander, and an untyped emitter for one stack. Output is deterministic and committed.

**Gate:** a hand-written GKE example plans identically to its HCL original. Met on OpenTofu 1.13.9.

## M1 Address safety

The address ledger (`kiln.lock`), `moved` blocks from `^{:was ...}` and `kiln mv`, a stop on undeclared renames (K0500), and `kiln import` for a useful subset of HCL.

**Gate:** an existing repository imports to a zero-diff plan and survives a component rename. Met on the two fixtures in `examples/`: both import to plans identical to the HCL, and `tools/move-test.sh` shows a renamed component instance and resource planning as moves only. Not yet tried on a real client repository, which is where the import subset will find its edges.

## M2 Schema checking

Every resource, data source and provider block is checked against cached provider schemas after evaluation: arguments, required fields, read-only attributes, value types, nested blocks and their counts, attribute reads. Schemas are cached in `.kiln/schemas/` and committed. `kiln schema` fetches them with the engine.

This replaced the original M2 ("types"). Checking the evaluated graph gives users schema errors early. Full inference, which also catches errors in code paths that did not run, moved to M3, where staging needs it anyway.

**Gate:** every argument error in a seeded test corpus is caught before Terraform runs. Met in `tests/m2.rs`. Against OpenTofu 1.13.9, `kiln schema` fetched the real `random` and `local` schemas, and the imported fixtures pass a strict check against all three cached providers.

## M3 Types, staging, secrecy

- Hindley-Milner inference with row-polymorphic records over functions and components, before evaluation.
- Types for resources come from the schema cache M2 introduced.
- `Deferred T` and `Secret T` in the type system, replacing today's dynamic staging checks.
- The explicit deferred forms: `count-deferred` and `for-each-deferred`.
- Structural recursion, which the type checker can verify.
- Typed module inputs and outputs.

**Gate:** the corpus of known `for_each` and `count` plan-time failures becomes compile errors, including in code paths that do not run.

## M4 Stacks and contexts

`defstack`, `defcontext`, typed cross-stack `ref` through `terraform_remote_state`, `out/graph.json`, `kiln run`, and one CI adapter (GitLab). Also: pruning `moved` blocks once every state has applied them.

**Gate:** a Terragrunt repository with three environments is replaced and its directory tree deleted.

## M5 Hardening

Macro hygiene beyond gensym, the language server and formatter, Datalog policy rules, graph transforms, `kiln test`, `kiln diff`, `kiln explain`.

**Gate:** a second team adopts Kiln without help from the authors.

## Known limits carried forward

- One cached schema version per provider per repository.
- Values that are Terraform expressions are not type-checked until M3.
- `kiln import` does not handle `dynamic` blocks, variables without defaults, provisioners, `moved` / `import` / `check` blocks, or template directives.
- Comments are not carried over by `kiln import`.
