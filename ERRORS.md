# Kiln diagnostic codes

Generated from `src/codes.rs`; do not edit by hand. `kiln explain <code>` prints one entry.

| Code | Meaning |
| --- | --- |
| [K0001](#k0001) | syntax error |
| [K0100](#k0100) | unbound name |
| [K0101](#k0101) | not callable |
| [K0102](#k0102) | wrong number of arguments |
| [K0103](#k0103) | wrong kind of value |
| [K0104](#k0104) | definition outside the top level |
| [K0105](#k0105) | arithmetic or range error |
| [K0201](#k0201) | invalid name |
| [K0202](#k0202) | declared twice |
| [K0203](#k0203) | resource used as a value |
| [K0312](#k0312) | existence decided after apply |
| [K0313](#k0313) | iterating a value known after apply |
| [K0314](#k0314) | name known after apply |
| [K0315](#k0315) | and/or on a value known after apply |
| [K0316](#k0316) | compile-time text function on a value known after apply |
| [K0317](#k0317) | count or for_each set inside count-deferred or for-each-deferred |
| [K0318](#k0318) | for_each nested or combined with count |
| [K0320](#k0320) | secret flows into a plain argument |
| [K0321](#k0321) | output exposes a secret |
| [K0322](#k0322) | secret written in the source |
| [K0400](#k0400) | runaway expansion |
| [K0401](#k0401) | recursion that does not shrink |
| [K0402](#k0402) | recursion without a base case |
| [K0403](#k0403) | recursive function used as a value |
| [K0500](#k0500) | undeclared rename |
| [K0501](#k0501) | kiln mv does not match the build |
| [K0502](#k0502) | move across resource types |
| [K0600](#k0600) | outside the import subset |
| [K0601](#k0601) | cyclic declarations |
| [K0700](#k0700) | unknown resource type |
| [K0701](#k0701) | unknown argument |
| [K0702](#k0702) | missing required argument |
| [K0703](#k0703) | read-only attribute set |
| [K0704](#k0704) | wrong value type |
| [K0705](#k0705) | nested block given as a value |
| [K0706](#k0706) | wrong number of nested blocks |
| [K0707](#k0707) | unknown attribute read |
| [K0708](#k0708) | reading from a collection of blocks |
| [K0711](#k0711) | cached schema has the wrong version |
| [K0712](#k0712) | no cached schema |
| [K0713](#k0713) | unreadable cached schema |
| [K0714](#k0714) | no cached module interface |
| [K0715](#k0715) | cached module interface has the wrong version |
| [K0750](#k0750) | deprecated argument |
| [K0900](#k0900) | fail |

## K0001

**Syntax error.** The source could not be read: an unclosed or unexpected delimiter, an unterminated string, an unknown escape, an invalid number, or a map literal with an odd number of forms.

## K0100

**Unbound name.** A symbol is not defined at this point, or a namespaced symbol uses a namespace the `ns` form does not declare. A definition is visible only after it: Kiln has no forward references.

## K0101

**Not callable.** The head of a call is a value that is not a function, keyword, map or resource constructor.

## K0102

**Wrong number of arguments.** A function, special form or built-in was called with too few or too many arguments, or keyword arguments not in pairs.

## K0103

**Wrong kind of value.** A value of the wrong kind was given: a string where a map is needed, a pattern that is not a symbol, vector or map, an invalid `ns` clause, and similar.

## K0104

**Definition outside the top level.** `def`, `defn`, `defmacro` and `ns` are allowed only at the top level of a file. Use `let` for local bindings and `fn` for local functions.

## K0105

**Arithmetic or range error.** Integer overflow, division by zero, an index out of range, or a `range` longer than 100000 elements.

## K0201

**Invalid name.** A resource, output, module or component instance name must be a string that starts with a letter or underscore and contains only letters, digits, underscores and hyphens. `(slug x)` turns arbitrary text into one.

## K0202

**Declared twice.** Two declarations have the same address, or a stack declares two backends. Give each declaration a distinct name, or put them in components with different instance names.

## K0203

**Resource used as a value.** A resource handle was used where a value is expected. Read one of its attributes instead, for example `(:id x)`.

## K0312

**Existence decided after apply.** Whether a resource exists depends on a value known only after apply: the condition of an `if` or `when` whose branches declare, a `:when` filter in `for`, or a `filter` predicate. Branch on compile-time data, or use `(count-deferred cond ...)` to let Terraform decide.

## K0313

**Iterating a value known after apply.** A collection known only after apply is iterated or inspected at compile time: `for`, `map`, `count`, `first`, `merge` and similar. Iterate compile-time data, or use `(for-each-deferred [x coll] ...)` to let Terraform iterate.

## K0314

**Name known after apply.** A resource, output, module or component name depends on a value known only after apply, such as an attribute or `each.value`. Addresses are fixed at compile time; derive names from compile-time data.

## K0315

**And/or on a value known after apply.** `and` and `or` decide at compile time and cannot short-circuit on a value known only after apply. Write the condition as a Terraform expression: `(tf/and a b)`, `(tf/or a b)`.

## K0316

**Compile-time text function on a value known after apply.** `name`, `keyword`, `slug`, `lower`, `upper` and the separator of `join` need a compile-time string. Use the Terraform function instead, for example `(tf/lower x)`.

## K0317

**Count or for_each set inside count-deferred or for-each-deferred.** A declaration inside `count-deferred` or `for-each-deferred` sets `:count` or `:for_each` itself. Let the enclosing form decide, or move the declaration out of it.

## K0318

**For_each nested or combined with count.** `for-each-deferred` inside either form, or `count-deferred` inside `for-each-deferred`. Terraform gives a resource at most one `for_each`, and not together with `count`; build the combined collection with a Terraform expression instead.

## K0320

**Secret flows into a plain argument.** A value derived from a sensitive attribute flows into an argument the provider does not mark sensitive, or into `count` or `for_each`, which Terraform rejects. If exposing it is intended, say so with `(tf/nonsensitive x)`.

## K0321

**Output exposes a secret.** An output's value derives from a sensitive attribute. Mark the output sensitive: `(output "name" value {:sensitive true})`.

## K0322

**Secret written in the source.** `(secret x)` was applied to a value written in the source. A secret in the source is in the repository; read it from a secret store instead.

## K0400

**Runaway expansion.** Macro expansion did not terminate, or evaluation nested too deeply, usually because a macro expands into a call of itself.

## K0401

**Recursion that does not shrink.** A function calls itself without passing a strictly smaller part of the same parameter in every recursive call. Kiln allows only structural recursion, so every build terminates: recur on `(rest xs)`, a destructured part, `(:children node)`, or an element. Counting down is not structural; use `range`.

## K0402

**Recursion without a base case.** At run time, a recursive call did not make its shrinking argument smaller, for example `(f (rest xs))` on an empty `xs`. Stop the recursion when the argument is empty.

## K0403

**Recursive function used as a value.** A function that calls itself may only call itself directly, or be passed to `map`, `mapcat` or `filter`.

## K0500

**Undeclared rename.** Addresses disappeared while new addresses of the same type appeared. Terraform would destroy and recreate them. Declare renames with `^{:was "old"} "new"` or `kiln mv`, or rebuild with `--replace` if they really are deletions and creations.

## K0501

**Kiln mv does not match the build.** A move recorded with `kiln mv` applies when its source address disappears and its target appears in the same build. Delete the entry from `pending` in kiln.lock if it is no longer wanted.

## K0502

**Move across resource types.** A move would change the resource type. Terraform cannot move state across types.

## K0600

**Outside the import subset.** `kiln import` found constructs it does not translate: `dynamic` blocks, variables without a default, provisioners, `moved`, `import` and `check` blocks, template directives. They are listed, not guessed at.

## K0601

**Cyclic declarations.** The imported declarations refer to each other in a cycle, so they cannot be ordered for a language without forward references.

## K0700

**Unknown resource type.** The provider's cached schema has no such resource type or data source. The message suggests the closest name.

## K0701

**Unknown argument.** The schema has no argument or nested block of this name, at any depth, or a module has no variable of this name. The message suggests the closest name.

## K0702

**Missing required argument.** A required argument or nested block is missing, or a module variable without a default is not given.

## K0703

**Read-only attribute set.** The provider computes this attribute; it cannot be set. Read it from the resource instead.

## K0704

**Wrong value type.** A value does not have the type the schema requires, such as the string "1460" for a number or a string for a set. Values known only after apply pass: their type is known only to the engine.

## K0705

**Nested block given as a value.** A nested block must be a map, or for list and set blocks a vector of maps. Terraform cannot compute a block from an expression.

## K0706

**Wrong number of nested blocks.** A nested block appears more or fewer times than the schema allows.

## K0707

**Unknown attribute read.** Reading an attribute that the resource, or a nested block of it, does not have, or an output that a module does not declare. The message suggests the closest name.

## K0708

**Reading from a collection of blocks.** An attribute was read from a list or set of blocks. Pick one first: `(get x 0)` for a list, `(tf/one x)` for a set.

## K0711

**Cached schema has the wrong version.** The cached schema's version does not satisfy the stack's version constraint, so it cannot be used to check. Refresh it with `kiln schema <file.kiln>`.

## K0712

**No cached schema.** A provider has no cached schema, so its declarations are not checked. A warning, or an error with `--strict`. Run `kiln schema <file.kiln>` and commit what it writes under .kiln/schemas/.

## K0713

**Unreadable cached schema.** A file under .kiln/schemas/ is not a valid schema cache. Regenerate it with `kiln schema`.

## K0714

**No cached module interface.** A module has no cached interface, so its inputs and outputs are not checked. A warning, or an error with `--strict`. Run `kiln schema <file.kiln>` and commit what it writes under .kiln/modules/; local modules are read directly, remote ones are installed with the engine.

## K0715

**Cached module interface has the wrong version.** The cached interface of a module does not satisfy the version constraint in the module call, so it is not used to check. Refresh it with `kiln schema <file.kiln>`.

## K0750

**Deprecated argument.** A warning: the provider marks this argument deprecated.

## K0900

**Fail.** Raised by `(fail ...)` in the program itself; the message is the program's.
