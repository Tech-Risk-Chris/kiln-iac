//! Every diagnostic code Kiln reports, with what it means and what to do.
//!
//! This is the single source of truth: `kiln explain <code>` prints an entry,
//! and ERRORS.md is generated from this table (a test keeps it in sync).

pub struct Code {
    pub code: &'static str,
    pub title: &'static str,
    pub explain: &'static str,
}

pub const CODES: &[Code] = &[
    // ---------------------------------------------------------------- reading
    Code {
        code: "K0001",
        title: "syntax error",
        explain: "The source could not be read: an unclosed or unexpected delimiter, an unterminated string, an unknown escape, an invalid number, or a map literal with an odd number of forms.",
    },
    // ---------------------------------------------------------------- evaluation
    Code {
        code: "K0100",
        title: "unbound name",
        explain: "A symbol is not defined at this point, or a namespaced symbol uses a namespace the `ns` form does not declare. A definition is visible only after it: Kiln has no forward references.",
    },
    Code { code: "K0101", title: "not callable", explain: "The head of a call is a value that is not a function, keyword, map or resource constructor." },
    Code {
        code: "K0102",
        title: "wrong number of arguments",
        explain: "A function, special form or built-in was called with too few or too many arguments, or keyword arguments not in pairs.",
    },
    Code {
        code: "K0103",
        title: "wrong kind of value",
        explain: "A value of the wrong kind was given: a string where a map is needed, a pattern that is not a symbol, vector or map, an invalid `ns` clause, and similar.",
    },
    Code {
        code: "K0104",
        title: "definition outside the top level",
        explain: "`def`, `defn`, `defmacro` and `ns` are allowed only at the top level of a file. Use `let` for local bindings and `fn` for local functions.",
    },
    Code { code: "K0105", title: "arithmetic or range error", explain: "Integer overflow, division by zero, an index out of range, or a `range` longer than 100000 elements." },
    // ---------------------------------------------------------------- declarations
    Code {
        code: "K0201",
        title: "invalid name",
        explain: "A resource, output, module or component instance name must be a string that starts with a letter or underscore and contains only letters, digits, underscores and hyphens. `(slug x)` turns arbitrary text into one.",
    },
    Code {
        code: "K0202",
        title: "declared twice",
        explain: "Two declarations have the same address, or a stack declares two backends. Give each declaration a distinct name, or put them in components with different instance names.",
    },
    Code {
        code: "K0203",
        title: "resource used as a value",
        explain: "A resource handle was used where a value is expected. Read one of its attributes instead, for example `(:id x)`.",
    },
    // ---------------------------------------------------------------- stages and secrets
    Code {
        code: "K0312",
        title: "existence decided after apply",
        explain: "Whether a resource exists depends on a value known only after apply: the condition of an `if` or `when` whose branches declare, a `:when` filter in `for`, or a `filter` predicate. Branch on compile-time data, or use `(count-deferred cond ...)` to let Terraform decide.",
    },
    Code {
        code: "K0313",
        title: "iterating a value known after apply",
        explain: "A collection known only after apply is iterated or inspected at compile time: `for`, `map`, `count`, `first`, `merge` and similar. Iterate compile-time data, or use `(for-each-deferred [x coll] ...)` to let Terraform iterate.",
    },
    Code {
        code: "K0314",
        title: "name known after apply",
        explain: "A resource, output, module or component name depends on a value known only after apply, such as an attribute or `each.value`. Addresses are fixed at compile time; derive names from compile-time data.",
    },
    Code {
        code: "K0315",
        title: "and/or on a value known after apply",
        explain: "`and` and `or` decide at compile time and cannot short-circuit on a value known only after apply. Write the condition as a Terraform expression: `(tf/and a b)`, `(tf/or a b)`.",
    },
    Code {
        code: "K0316",
        title: "compile-time text function on a value known after apply",
        explain: "`name`, `keyword`, `slug`, `lower`, `upper` and the separator of `join` need a compile-time string. Use the Terraform function instead, for example `(tf/lower x)`.",
    },
    Code {
        code: "K0317",
        title: "count or for_each set inside count-deferred or for-each-deferred",
        explain: "A declaration inside `count-deferred` or `for-each-deferred` sets `:count` or `:for_each` itself. Let the enclosing form decide, or move the declaration out of it.",
    },
    Code {
        code: "K0318",
        title: "for_each nested or combined with count",
        explain: "`for-each-deferred` inside either form, or `count-deferred` inside `for-each-deferred`. Terraform gives a resource at most one `for_each`, and not together with `count`; build the combined collection with a Terraform expression instead.",
    },
    Code {
        code: "K0320",
        title: "secret flows into a plain argument",
        explain: "A value derived from a sensitive attribute flows into an argument the provider does not mark sensitive, or into `count` or `for_each`, which Terraform rejects. If exposing it is intended, say so with `(tf/nonsensitive x)`.",
    },
    Code {
        code: "K0321",
        title: "output exposes a secret",
        explain: "An output's value derives from a sensitive attribute. Mark the output sensitive: `(output \"name\" value {:sensitive true})`.",
    },
    Code {
        code: "K0322",
        title: "secret written in the source",
        explain: "`(secret x)` was applied to a value written in the source. A secret in the source is in the repository; read it from a secret store instead.",
    },
    // ---------------------------------------------------------------- macros and recursion
    Code {
        code: "K0400",
        title: "runaway expansion",
        explain: "Macro expansion did not terminate, or evaluation nested too deeply, usually because a macro expands into a call of itself.",
    },
    Code {
        code: "K0401",
        title: "recursion that does not shrink",
        explain: "A function calls itself without passing a strictly smaller part of the same parameter in every recursive call. Kiln allows only structural recursion, so every build terminates: recur on `(rest xs)`, a destructured part, `(:children node)`, or an element. Counting down is not structural; use `range`.",
    },
    Code {
        code: "K0402",
        title: "recursion without a base case",
        explain: "At run time, a recursive call did not make its shrinking argument smaller, for example `(f (rest xs))` on an empty `xs`. Stop the recursion when the argument is empty.",
    },
    Code {
        code: "K0403",
        title: "recursive function used as a value",
        explain: "A function that calls itself may only call itself directly, or be passed to `map`, `mapcat` or `filter`.",
    },
    // ---------------------------------------------------------------- addresses
    Code {
        code: "K0500",
        title: "undeclared rename",
        explain: "Addresses disappeared while new addresses of the same type appeared. Terraform would destroy and recreate them. Declare renames with `^{:was \"old\"} \"new\"` or `kiln mv`, or rebuild with `--replace` if they really are deletions and creations.",
    },
    Code {
        code: "K0501",
        title: "kiln mv does not match the build",
        explain: "A move recorded with `kiln mv` applies when its source address disappears and its target appears in the same build. Delete the entry from `pending` in kiln.lock if it is no longer wanted.",
    },
    Code { code: "K0502", title: "move across resource types", explain: "A move would change the resource type. Terraform cannot move state across types." },
    // ---------------------------------------------------------------- import
    Code {
        code: "K0600",
        title: "outside the import subset",
        explain: "`kiln import` found constructs it does not translate: `dynamic` blocks, variables without a default, provisioners, `moved`, `import` and `check` blocks, template directives. They are listed, not guessed at.",
    },
    Code { code: "K0601", title: "cyclic declarations", explain: "The imported declarations refer to each other in a cycle, so they cannot be ordered for a language without forward references." },
    // ---------------------------------------------------------------- schemas
    Code { code: "K0700", title: "unknown resource type", explain: "The provider's cached schema has no such resource type or data source. The message suggests the closest name." },
    Code { code: "K0701", title: "unknown argument", explain: "The schema has no argument or nested block of this name, at any depth, or a module has no variable of this name. The message suggests the closest name." },
    Code { code: "K0702", title: "missing required argument", explain: "A required argument or nested block is missing, or a module variable without a default is not given." },
    Code { code: "K0703", title: "read-only attribute set", explain: "The provider computes this attribute; it cannot be set. Read it from the resource instead." },
    Code {
        code: "K0704",
        title: "wrong value type",
        explain: "A value does not have the type the schema requires, such as the string \"1460\" for a number or a string for a set. Values known only after apply pass: their type is known only to the engine.",
    },
    Code { code: "K0705", title: "nested block given as a value", explain: "A nested block must be a map, or for list and set blocks a vector of maps. Terraform cannot compute a block from an expression." },
    Code { code: "K0706", title: "wrong number of nested blocks", explain: "A nested block appears more or fewer times than the schema allows." },
    Code { code: "K0707", title: "unknown attribute read", explain: "Reading an attribute that the resource, or a nested block of it, does not have, or an output that a module does not declare. The message suggests the closest name." },
    Code { code: "K0708", title: "reading from a collection of blocks", explain: "An attribute was read from a list or set of blocks. Pick one first: `(get x 0)` for a list, `(tf/one x)` for a set." },
    Code {
        code: "K0711",
        title: "cached schema has the wrong version",
        explain: "The cached schema's version does not satisfy the stack's version constraint, so it cannot be used to check. Refresh it with `kiln schema <file.kiln>`.",
    },
    Code {
        code: "K0712",
        title: "no cached schema",
        explain: "A provider has no cached schema, so its declarations are not checked. A warning, or an error with `--strict`. Run `kiln schema <file.kiln>` and commit what it writes under .kiln/schemas/.",
    },
    Code { code: "K0713", title: "unreadable cached schema", explain: "A file under .kiln/schemas/ is not a valid schema cache. Regenerate it with `kiln schema`." },
    Code {
        code: "K0714",
        title: "no cached module interface",
        explain: "A module has no cached interface, so its inputs and outputs are not checked. A warning, or an error with `--strict`. Run `kiln schema <file.kiln>` and commit what it writes under .kiln/modules/; local modules are read directly, remote ones are installed with the engine.",
    },
    Code {
        code: "K0715",
        title: "cached module interface has the wrong version",
        explain: "The cached interface of a module does not satisfy the version constraint in the module call, so it is not used to check. Refresh it with `kiln schema <file.kiln>`.",
    },
    Code { code: "K0750", title: "deprecated argument", explain: "A warning: the provider marks this argument deprecated." },
    // ---------------------------------------------------------------- user
    Code { code: "K0900", title: "fail", explain: "Raised by `(fail ...)` in the program itself; the message is the program's." },
];

pub fn find(code: &str) -> Option<&'static Code> {
    let c = code.trim().to_ascii_uppercase();
    CODES.iter().find(|x| x.code == c)
}

/// ERRORS.md, generated from the table above.
pub fn markdown() -> String {
    let mut s = String::from(
        "# Kiln diagnostic codes\n\nGenerated from `src/codes.rs`; do not edit by hand. `kiln explain <code>` prints one entry.\n\n| Code | Meaning |\n| --- | --- |\n",
    );
    for c in CODES {
        s.push_str(&format!("| [{}](#{}) | {} |\n", c.code, c.code.to_ascii_lowercase(), c.title));
    }
    for c in CODES {
        s.push_str(&format!("\n## {}\n\n**{}.** {}\n", c.code, capitalise(c.title), c.explain));
    }
    s
}

fn capitalise(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}
