use std::path::Path;

/// The registry name of the language a path maps to (by extension), or `None` if
/// we don't index that extension. Kept as a pure string lookup so the dispatch is
/// testable without the `code` feature compiled in.
pub fn lang_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.exts.contains(&ext.as_str()))
        .map(|l| l.name)
}

/// True if we have a grammar for this path's extension.
pub fn is_source_file(path: &Path) -> bool {
    lang_for_path(path).is_some()
}

/// UTF-8 text config/doc extensions `cm code grep` line-greps in addition to
/// recognized source files. Deliberately NOT in `LANGUAGES`: the semantic
/// index resolves a grammar per admitted file (`lang_for_path(...).expect`)
/// and would panic on a grammar-less row, so these stay grep-only.
pub const TEXT_GREP_EXTENSIONS: &[&str] = &["json", "jsonl", "md", "yaml", "yml", "toml", "lock"];

/// True if this path's extension is a grep-admitted text config/doc extension.
pub fn is_text_grep_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    TEXT_GREP_EXTENSIONS.contains(&ext.to_lowercase().as_str())
}

/// One language in the registry: its canonical name, the file extensions it owns,
/// and (under the `code` feature) the grammar + a function building its tagging
/// query. The query is a function (not a `&str`) so a language can APPEND a
/// supplemental query to the grammar's upstream `TAGS_QUERY` — see TypeScript,
/// whose shipped `tags.scm` only covers declaration forms (`.d.ts`) and misses
/// plain `function`/`class`/calls.
pub struct LangDef {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    #[cfg(feature = "code-index")]
    pub language: fn() -> tree_sitter::Language,
    #[cfg(feature = "code-index")]
    pub tags_query: fn() -> String,
}

/// Supplemental tags for TypeScript: the upstream `tags.scm` only tags signatures
/// and abstract/interface declarations, so a normal `.ts` file (concrete functions,
/// classes, calls) yields nothing. We add the concrete forms — these node types all
/// exist in the TS grammar, they're just absent from its query.
#[cfg(feature = "code-index")]
const TS_TAGS_EXTRA: &str = r#"
(function_declaration name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(method_definition name: (property_identifier) @name) @definition.method
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (member_expression property: (property_identifier) @name)) @reference.call
"#;

/// Supplemental tags for Rust: the upstream `tags.scm` only matches plain
/// identifiers and field expressions in call position, so a scoped/qualified
/// call (`crate::x::foo(..)`, `Type::assoc(..)`) produces NO reference at all
/// and cross-module dependency edges silently go missing (the umbrella
/// code-find-symbol-navigation Phase-1 dogfood finding). Capture the scoped
/// call's last segment as the referenced name. It also tags no const/static
/// items, so a `#SYMBOL` evidence anchor could not cite a module-level
/// constant (registry item 505: src/agents.rs SECTION_TEMPLATE failed
/// section-set with "no symbol named SECTION_TEMPLATE") — tag them as
/// `@definition.constant`.
#[cfg(feature = "code-index")]
const RUST_TAGS_EXTRA: &str = r#"
(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (identifier) @name)) @reference.call
(call_expression function: (generic_function function: (scoped_identifier name: (identifier) @name))) @reference.call
(call_expression function: (generic_function function: (field_expression field: (field_identifier) @name))) @reference.call
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
"#;

/// The supported languages. A path's extension picks the row; the row's grammar
/// and `tags.scm` drive extraction. Extensions are lowercase and must stay unique
/// across rows (first match wins, but we keep them disjoint).
///
/// Markup-only grammars (css/html) and ones that ship no `tags.scm` (bash) are
/// intentionally excluded — they have no meaningful symbol graph.
pub const LANGUAGES: &[LangDef] = &[
    LangDef {
        name: "rust",
        exts: &["rs"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_rust::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || format!("{}{}", tree_sitter_rust::TAGS_QUERY, RUST_TAGS_EXTRA),
    },
    LangDef {
        name: "python",
        exts: &["py", "pyi"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_python::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_python::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "javascript",
        exts: &["js", "jsx", "mjs", "cjs"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_javascript::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_javascript::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "typescript",
        exts: &["ts", "mts", "cts"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        #[cfg(feature = "code-index")]
        // Upstream TS tags.scm is declaration-only; append the concrete forms.
        tags_query: || format!("{}{}", tree_sitter_typescript::TAGS_QUERY, TS_TAGS_EXTRA),
    },
    LangDef {
        name: "tsx",
        exts: &["tsx"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        #[cfg(feature = "code-index")]
        tags_query: || format!("{}{}", tree_sitter_typescript::TAGS_QUERY, TS_TAGS_EXTRA),
    },
    LangDef {
        name: "go",
        exts: &["go"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_go::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_go::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "java",
        exts: &["java"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_java::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_java::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "c",
        exts: &["c", "h"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_c::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_c::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "cpp",
        exts: &["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_cpp::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_cpp::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "csharp",
        exts: &["cs"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_c_sharp::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "ruby",
        exts: &["rb"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_ruby::LANGUAGE.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_ruby::TAGS_QUERY.to_string(),
    },
    LangDef {
        name: "php",
        exts: &["php"],
        #[cfg(feature = "code-index")]
        language: || tree_sitter_php::LANGUAGE_PHP.into(),
        #[cfg(feature = "code-index")]
        tags_query: || tree_sitter_php::TAGS_QUERY.to_string(),
    },
];
