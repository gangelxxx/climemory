//! Source parsing for code find. Tree-sitter definitions and references feed
//! the derived code index; no memory authority or specification states are involved.
//! Text search remains available without the code-index feature.

mod extract;
mod languages;

pub use extract::{parse, path_is_test};
// `LangDef`/`LANGUAGES` keep their historical `crate::code::…` path; nothing
// outside this module references them yet, which a bin crate lints as unused.
#[allow(unused_imports)]
pub use languages::{
    is_source_file, is_text_grep_file, lang_for_path, LangDef, LANGUAGES, TEXT_GREP_EXTENSIONS,
};

use crate::util::digest;

/// A symbol DEFINITION found in a file: its name, the tag's syntax kind
/// (`function`, `struct`, `class`, `type`, ...), the 1-based start line, the
/// 1-based end line of the whole definition node (so references can be owned by
/// the DEEPEST ENCLOSING callable — lexical ownership — instead of the nearest
/// preceding definition; line order alone is not an ownership rule), the trimmed
/// first line of source as a human-facing signature, and whether it sits in test
/// code (so listings can hide it by default — test symbols are noise when you're
/// learning a module's real API).
#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub end_line: usize,
    pub signature: String,
    pub is_test: bool,
}

/// A symbol REFERENCE (a call / use site): the referenced name and the 1-based
/// line it occurs on. Resolution to a concrete definition happens later, by name.
/// `kind` is the tree-sitter syntax-type name of the reference capture (e.g.
/// call / implementation) — the same source the definition kind comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub name: String,
    pub line: usize,
    pub kind: String,
}

/// The result of parsing one source file: its definitions and references.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeParse {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

/// Content-addressed id for a symbol definition. Stable across rebuilds (depends
/// only on where/what the symbol is, not on insertion order), so re-`map`ping an
/// unchanged file reproduces identical ids — the same property chunk ids rely on.
pub fn symbol_id(path: &str, kind: &str, name: &str, line: usize) -> String {
    digest(format!("{path}\0{kind}\0{name}\0{line}"))
}

/// Ubiquitous stdlib / built-in method names (iterator combinators, Option/Result
/// helpers, conversions, common collection ops) that appear as `uses` references
/// everywhere. Because `uses` resolves by name with no scope analysis, a project
/// symbol that happens to be named `map`/`get`/`new` would otherwise swallow every
/// `.map()`/`.get()`/`::new()` call as a false dependency. We refuse to resolve
/// these names to a project symbol — they stay external — so `--calls`/`--uses`
/// surface real, in-project relationships, not language noise. (They're still
/// visible under `--calls --external` as unresolved names.)
pub fn is_stdlib_combinator(name: &str) -> bool {
    matches!(
        name,
        // iterator / option / result combinators
        "map" | "filter" | "filter_map" | "flat_map" | "flatten" | "fold" | "reduce"
            | "for_each" | "collect" | "zip" | "chain" | "take" | "skip" | "rev"
            | "enumerate" | "find" | "any" | "all" | "count" | "sum" | "product"
            | "min" | "max" | "sort" | "sort_by" | "position" | "last" | "next"
            | "and_then" | "or_else" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            // unwrapping / conversion
            | "unwrap" | "expect" | "clone" | "into" | "from" | "to_string" | "to_owned"
            | "as_str" | "as_ref" | "as_deref" | "as_mut" | "as_bytes" | "to_vec"
            | "parse" | "try_into" | "try_from" | "borrow"
            // common collection / string ops
            | "push" | "pop" | "insert" | "remove" | "get" | "get_mut" | "len" | "is_empty"
            | "contains" | "iter" | "iter_mut" | "into_iter" | "keys" | "values" | "entry"
            | "split" | "join" | "trim" | "replace" | "starts_with" | "ends_with"
            | "to_lowercase" | "to_uppercase" | "chars" | "lines" | "format"
            // Option/Result constructors that are everywhere
            | "Some" | "Ok" | "Err" | "None"
    )
}
