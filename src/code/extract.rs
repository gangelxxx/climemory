#[cfg(feature = "code-index")]
use super::languages::LANGUAGES;
use super::CodeParse;
#[cfg(feature = "code-index")]
use super::{Def, Ref};
use crate::util::{AppError, Result};

/// Parse one source file's text into definitions and references. `path` is the
/// file's (relative) path — used only to tell test code from product code; `lang`
/// is a registry name from `lang_for_path`. Errors only on a genuinely broken
/// grammar query (a build-time bug, not user input); a file with no tags yields an
/// empty `CodeParse`.
#[cfg(feature = "code-index")]
pub fn parse(path: &str, lang: &str, src: &str) -> Result<CodeParse> {
    use tree_sitter_tags::{TagsConfiguration, TagsContext};

    thread_local! {
        static CONFIGURATIONS: std::cell::RefCell<
            std::collections::BTreeMap<&'static str, std::result::Result<TagsConfiguration, String>>
        > = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    }

    let def = LANGUAGES
        .iter()
        .find(|l| l.name == lang)
        .ok_or_else(|| AppError::new(format!("no grammar registered for language '{lang}'")))?;

    CONFIGURATIONS.with(|configurations| {
        let mut configurations = configurations.borrow_mut();
        let cached = configurations.entry(def.name).or_insert_with(|| {
            let query = sanitize_tags_query(&(def.tags_query)());
            TagsConfiguration::new((def.language)(), &query, "").map_err(|error| error.to_string())
        });
        let config = cached.as_ref().map_err(|error| {
            AppError::new(format!("tags query for '{lang}' is invalid: {error}"))
        })?;
        let mut ctx = TagsContext::new();
        let bytes = src.as_bytes();
        let (tags, _) = ctx
            .generate_tags(config, bytes, None)
            .map_err(|e| AppError::new(format!("tag generation failed for '{lang}': {e}")))?;

        // Whole-file test files (tests/…, *_test.go, test_*.py, *.test.ts, …) make every
        // symbol a test symbol; otherwise a symbol is a test only if it sits inside an
        // inline test module/region (see `inline_test_from`).
        let file_is_test = path_is_test(path);
        let inline_test_line = if file_is_test {
            Some(1) // everything counts as test
        } else {
            inline_test_from(src)
        };

        let mut out = CodeParse::default();
        // Line-start byte offsets, so a definition's END line can be derived
        // from the tag's whole-node byte range (tag.span covers only the name).
        let line_starts = line_starts(src);
        for tag in tags {
            let tag = match tag {
                Ok(t) => t,
                Err(_) => continue, // a bad single tag never sinks the whole file
            };
            let name = match src.get(tag.name_range.clone()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let line = tag.span.start.row + 1; // tree-sitter rows are 0-based
            if tag.is_definition {
                let kind = config.syntax_type_name(tag.syntax_type_id).to_string();
                let signature = src
                    .get(tag.line_range.clone())
                    .map(|l| crate::util::preview(l, 120))
                    .unwrap_or_default();
                let is_test = inline_test_line.is_some_and(|start| line >= start);
                // tag.range.end is EXCLUSIVE; the node's last byte ends the def.
                let end_line =
                    line_of_byte(&line_starts, tag.range.end.saturating_sub(1)).max(line);
                out.defs.push(Def {
                    name,
                    kind,
                    line,
                    end_line,
                    signature,
                    is_test,
                });
            } else {
                let kind = config.syntax_type_name(tag.syntax_type_id).to_string();
                out.refs.push(Ref { name, line, kind });
            }
        }
        Ok(out)
    })
}

/// Byte offsets of every line start in `src` (line 1 starts at 0). Used to map
/// a definition node's byte range back to a 1-based line number.
#[cfg(feature = "code-index")]
fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in src.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// The 1-based line containing byte offset `byte`, given `line_starts(src)`.
#[cfg(feature = "code-index")]
fn line_of_byte(starts: &[usize], byte: usize) -> usize {
    starts.partition_point(|start| *start <= byte)
}

/// The capture-name prefixes `tree-sitter-tags` accepts in a tags query. Anything
/// else (e.g. C#'s bare `@module`, which is `@definition.module` written wrong)
/// makes `TagsConfiguration::new` reject the WHOLE query, so that language indexes
/// nothing. See the upstream allow-list in the error it raises:
///   "Expected one of: @definition.*, @reference.*, @doc, @name, @local.(...)".
#[cfg(feature = "code-index")]
const ALLOWED_TAG_CAPTURES: &[&str] = &[
    "@definition.",
    "@reference.",
    "@local.",
    "@name",
    "@doc",
    "@ignore",
];

/// Drop any tags-query pattern that ends in a capture `tree-sitter-tags` doesn't
/// understand, so one bad stanza can't sink the whole grammar. We split on the
/// blank lines that separate stanzas in every shipped `tags.scm`, and a stanza is
/// dropped only if its LAST top-level `@capture` (the one classifying the match) is
/// not in `ALLOWED_TAG_CAPTURES`. Inner `@name` captures are fine; comments and the
/// well-formed stanzas pass through untouched. The concrete bug this fixes: the C#
/// grammar's `(namespace_declaration …) @module` (a stray duplicate of the valid
/// `@definition.module` line right above it) — dropping it leaves namespaces still
/// tagged via that valid line.
#[cfg(feature = "code-index")]
fn sanitize_tags_query(query: &str) -> String {
    // Stanzas are separated by blank lines. Keep a stanza unless its classifying
    // capture (the last @token in it) is disallowed.
    let kept: Vec<&str> = query
        .split("\n\n")
        .filter(|stanza| {
            let last_capture = stanza.split_whitespace().rfind(|tok| tok.starts_with('@'));
            match last_capture {
                // No capture at all (blank/comment-only) — harmless, keep it.
                None => true,
                Some(cap) => {
                    // Strip a trailing ')' the capture may butt against, then check.
                    let cap = cap.trim_end_matches(')');
                    ALLOWED_TAG_CAPTURES
                        .iter()
                        .any(|ok| cap == *ok || cap.starts_with(ok))
                }
            }
        })
        .collect();
    kept.join("\n\n")
}

/// Without the `code` feature, source-code indexing isn't built into the binary.
/// Self-healing error (same shape as `import::pdf_chunks`): tell the caller how to
/// get it. Keeping the signature identical means `commands::map` compiles either way.
#[cfg(not(feature = "code-index"))]
pub fn parse(_path: &str, _lang: &str, _src: &str) -> Result<CodeParse> {
    Err(AppError::with_hint(
        "source-code indexing is not built into this binary",
        "Rebuild with `cargo build --release --features code-index`.",
    ))
}

/// True when a path is a whole test file by common convention across the supported
/// languages: a `tests/` directory segment (Rust integration tests, Python/JS test
/// trees), `_test.go` / `Test.java`, `test_*.py` / `*_test.py`, or a
/// `*.test|spec.{ts,tsx,js,jsx}` file (JS/TS). Cheap string checks, no parsing.
/// Chunk construction also consults this helper in builds without the optional
/// parser feature, so keep the cheap path classifier feature-independent.
pub fn path_is_test(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let original_file = normalized.rsplit('/').next().unwrap_or(&normalized);
    let p = normalized.to_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.split('/')
        .any(|seg| seg == "tests" || seg == "test" || seg == "__tests__")
        || file.ends_with("_test.go")
        // JUnit convention is `FooTest.java`; a case-sensitive suffix keeps
        // `Latest.java`/`Contest.java` out of the test class.
        || original_file.ends_with("Test.java")
        || file.starts_with("test_") && file.ends_with(".py")
        || file.ends_with("_test.py")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.tsx")
        || file.ends_with(".test.js")
        || file.ends_with(".test.jsx")
        || file.ends_with(".spec.ts")
        || file.ends_with(".spec.js")
}

/// The 1-based line at which an INLINE test MODULE begins, or `None`. Rust
/// convention puts `#[cfg(test)] mod tests { … }` at the end of a file and that
/// module holds the file's unit tests, so the first `mod tests`/`mod test` line
/// marks the boundary: every definition at/after it is test code.
///
/// We deliberately key off the `mod tests` line, NOT a bare `#[cfg(test)]`
/// attribute. A `#[cfg(test)]` can sit on a single product-adjacent item (e.g. a
/// test-only `insert_note` helper inside `impl Store`) with real product code
/// AFTER it; treating "everything past the first #[cfg(test)]" as test wrongly
/// buried ~64% of this very repo's symbols. The trade-off: a lone `#[cfg(test)]`
/// item outside a test module is seen as product code — rare and harmless next to
/// the win of getting the common `mod tests` layout right. No brace matching;
/// whole-file detection (`path_is_test`) covers separate test files.
#[cfg(feature = "code-index")]
fn inline_test_from(src: &str) -> Option<usize> {
    let mut line_no = 0usize;
    for line in src.lines() {
        line_no += 1;
        let t = line.trim_start();
        // Match a test-module declaration: `mod tests`, `pub mod tests`, `mod test`
        // (with the next char a space/brace/EOL so `mod testing` doesn't match).
        let after_mod = t
            .strip_prefix("pub ")
            .unwrap_or(t)
            .strip_prefix("mod ")
            .map(str::trim_start);
        if let Some(rest) = after_mod {
            let name_ok = rest == "tests"
                || rest == "test"
                || rest.starts_with("tests ")
                || rest.starts_with("tests{")
                || rest.starts_with("test ")
                || rest.starts_with("test{");
            if name_ok {
                return Some(line_no);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::{is_source_file, is_stdlib_combinator, lang_for_path, symbol_id, LANGUAGES};
    use std::path::Path;

    #[test]
    fn lang_for_path_maps_known_extensions_case_insensitively() {
        assert_eq!(lang_for_path(Path::new("src/store.rs")), Some("rust"));
        assert_eq!(lang_for_path(Path::new("a/b/main.PY")), Some("python"));
        assert_eq!(lang_for_path(Path::new("app.tsx")), Some("tsx"));
        assert_eq!(lang_for_path(Path::new("x.cpp")), Some("cpp"));
        assert_eq!(lang_for_path(Path::new("README.md")), None);
        assert_eq!(lang_for_path(Path::new("no_extension")), None);
    }

    #[test]
    fn is_source_file_agrees_with_lang_for_path() {
        assert!(is_source_file(Path::new("x.go")));
        assert!(!is_source_file(Path::new("x.txt")));
    }

    #[test]
    fn symbol_id_is_stable_and_distinct() {
        let a = symbol_id("src/store.rs", "function", "upsert_note", 195);
        let b = symbol_id("src/store.rs", "function", "upsert_note", 195);
        let c = symbol_id("src/store.rs", "function", "upsert_note", 196); // diff line
        assert_eq!(a, b, "same inputs -> same id (rebuild-stable)");
        assert_ne!(a, c, "different line -> different id");
    }

    #[test]
    fn registry_extensions_are_disjoint() {
        // No extension may map to two languages, or dispatch becomes order-dependent.
        let mut seen = std::collections::HashSet::new();
        for lang in LANGUAGES {
            for ext in lang.exts {
                assert!(seen.insert(*ext), "extension '{ext}' is claimed twice");
            }
        }
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_rust_extracts_defs_and_refs() {
        let src = "struct Store { x: i32 }\n\
                   fn upsert(s: &Store) -> i32 { helper(s) }\n\
                   fn helper(s: &Store) -> i32 { s.x }\n";
        let p = parse("src/store.rs", "rust", src).unwrap();
        let names: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Store"));
        assert!(names.contains(&"upsert"));
        assert!(names.contains(&"helper"));
        // `helper(s)` on line 2 is a reference.
        assert!(p.refs.iter().any(|r| r.name == "helper" && r.line == 2));
        // A def carries a usable signature and 1-based line.
        let store = p.defs.iter().find(|d| d.name == "Store").unwrap();
        assert_eq!(store.line, 1);
        assert!(store.signature.contains("Store"));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_rust_captures_scoped_and_generic_calls() {
        // RUST_TAGS_EXTRA: the upstream rust tags.scm matches only plain
        // identifiers and field expressions in call position, so scoped
        // (crate::x::foo()) and generic (foo::<T>()) calls produced no
        // references at all.
        let src = "fn plain() {}\n\
                   fn generic<T>(_t: T) {}\n\
                   fn caller() {\n\
                       crate::plain();\n\
                       generic::<u32>(1);\n\
                       crate::generic::<u32>(2);\n\
                       let _n: u32 = \"5\".parse::<u32>().unwrap();\n\
                   }\n";
        let p = parse("src/x.rs", "rust", src).unwrap();
        let refs: Vec<(&str, usize)> = p.refs.iter().map(|r| (r.name.as_str(), r.line)).collect();
        let count = |name, line| refs.iter().filter(|&&r| r == (name, line)).count();
        // Exact occurrence counts: no missed captures, no double captures.
        assert_eq!(count("plain", 4), 1, "scoped call: {refs:?}");
        assert_eq!(count("generic", 5), 1, "generic call: {refs:?}");
        assert_eq!(count("generic", 6), 1, "scoped generic call: {refs:?}");
        assert_eq!(count("parse", 7), 1, "method generic call: {refs:?}");
        assert!(
            p.refs.iter().all(|r| r.kind == "call"),
            "every ref is a call: {:?}",
            p.refs
        );
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_rust_tags_const_and_static_as_constant_defs() {
        // Registry item 505: the upstream rust tags.scm tags no const/static
        // items, so a `#SYMBOL` evidence anchor could not cite a module-level
        // constant. RUST_TAGS_EXTRA tags them as `constant` defs.
        let src = "pub const LIMIT: usize = 10;\n\
                   static TABLE: [u8; 2] = [0, 1];\n\
                   fn read() -> usize { LIMIT }\n";
        let p = parse("src/x.rs", "rust", src).unwrap();
        let limit = p
            .defs
            .iter()
            .find(|d| d.name == "LIMIT")
            .expect("const tagged as a def");
        assert_eq!(limit.kind, "constant");
        assert_eq!(limit.line, 1);
        let table = p
            .defs
            .iter()
            .find(|d| d.name == "TABLE")
            .expect("static tagged as a def");
        assert_eq!(table.kind, "constant");
        assert_eq!(table.line, 2);
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_typescript_uses_supplemental_tags_for_concrete_forms() {
        // Regression: the upstream TS tags.scm is declaration-only, so without our
        // TS_TAGS_EXTRA a normal .ts file yields nothing. Assert the concrete
        // function/class/call forms come through.
        let src = "export class Service { run(): number { return helper(); } }\n\
                   function helper(): number { return 42; }\n";
        let p = parse("app.ts", "typescript", src).unwrap();
        assert!(
            p.defs.iter().any(|d| d.name == "Service"),
            "class not tagged"
        );
        assert!(p.defs.iter().any(|d| d.name == "helper"), "fn not tagged");
        assert!(p.refs.iter().any(|r| r.name == "helper"), "call not tagged");
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_tsx_uses_the_tsx_grammar() {
        let src = "export const View = () => <section>{helper()}</section>;\nfunction helper() { return 'ok'; }\n";
        let p = parse("view.tsx", "tsx", src).unwrap();
        assert!(p.defs.iter().any(|definition| definition.name == "View"));
        assert!(p.refs.iter().any(|reference| reference.name == "helper"));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_captures_definition_end_lines_for_lexical_ownership() {
        // Lexical call ownership keys off each definition's full node range:
        // `tag.span` covers only the name, so the end line must come from the
        // definition node's byte range. A variable-bound arrow function spans
        // its whole body; a one-line function ends on its own line.
        let src = "export function View() {\n\
                   \x20   const handleClick = () => {\n\
                   \x20       trackClick();\n\
                   \x20   };\n\
                   \x20   return <button onClick={handleClick} />;\n\
                   }\n\
                   function helper() { return 42; }\n";
        let p = parse("view.tsx", "tsx", src).unwrap();
        let def = |name| {
            p.defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} not tagged"))
        };
        assert_eq!((def("View").line, def("View").end_line), (1, 6));
        assert_eq!(
            (def("handleClick").line, def("handleClick").end_line),
            (2, 4)
        );
        assert_eq!((def("helper").line, def("helper").end_line), (7, 7));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_rust_captures_definition_end_lines() {
        let src = "fn outer() {\n\
                   \x20   helper();\n\
                   }\n\
                   fn helper() {}\n";
        let p = parse("src/x.rs", "rust", src).unwrap();
        let def = |name| p.defs.iter().find(|d| d.name == name).unwrap();
        assert_eq!((def("outer").line, def("outer").end_line), (1, 3));
        assert_eq!((def("helper").line, def("helper").end_line), (4, 4));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_maps_end_lines_through_crlf_and_multibyte_source() {
        // The end-line math is byte-based (`tag.range.end` -> line-start table)
        // while the start line is tree-sitter's row: both count only `\n`, so
        // CRLF endings and multi-byte UTF-8 lines must not shift the mapping.
        let src = "// комментарий 🚀\r\nfn outer() {\r\n    helper();\r\n}\r\nfn helper() {}\r\n";
        let p = parse("src/x.rs", "rust", src).unwrap();
        let def = |name| p.defs.iter().find(|d| d.name == name).unwrap();
        assert_eq!((def("outer").line, def("outer").end_line), (2, 4));
        assert_eq!((def("helper").line, def("helper").end_line), (5, 5));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_javascript_captures_variable_bound_arrow_ranges() {
        // The shared-grammar rule (tsx-lexical-call-ownership): plain JS gets
        // the same lexical-ownership ranges where the upstream tags query
        // covers variable-bound arrows.
        let src = "const onSave = () => {\n    persist();\n};\nfunction persist() {}\n";
        let p = parse("app.js", "javascript", src).unwrap();
        let def = |name| {
            p.defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} not tagged"))
        };
        assert_eq!((def("onSave").line, def("onSave").end_line), (1, 3));
        assert!(p.refs.iter().any(|r| r.name == "persist" && r.line == 2));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_python_extracts_class_and_calls() {
        let src = "class Store:\n    pass\n\
                   def upsert(s):\n    return helper(s)\n\
                   def helper(s):\n    return s\n";
        let p = parse("a.py", "python", src).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "Store"));
        assert!(p.defs.iter().any(|d| d.name == "upsert"));
        assert!(p.refs.iter().any(|r| r.name == "helper"));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_csharp_handles_bad_module_capture_in_tags_query() {
        // Regression: the C# grammar's tags.scm ends with a stray bare `@module`
        // capture, which `tree-sitter-tags` rejects — sinking the WHOLE query so no
        // .cs file indexed at all. The sanitizer drops that one stanza; namespace,
        // class and method must still come through (namespace via the valid
        // `@definition.module` line right above the bad one).
        let src = "namespace Foo {\n\
                   \x20 public class Bar {\n\
                   \x20   public void Baz() { Qux(); }\n\
                   \x20 }\n\
                   }\n";
        let p = parse("App.cs", "csharp", src).expect("C# must parse after sanitize");
        assert!(p.defs.iter().any(|d| d.name == "Foo" && d.kind == "module"));
        assert!(p.defs.iter().any(|d| d.name == "Bar" && d.kind == "class"));
        assert!(p.defs.iter().any(|d| d.name == "Baz" && d.kind == "method"));
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn sanitize_tags_query_drops_only_disallowed_stanzas() {
        // Keeps valid stanzas (definition/reference/name), drops a bare @module.
        let q = "(class_declaration name: (identifier) @name) @definition.class\n\n\
                 (namespace_declaration name: (identifier) @name) @definition.module\n\n\
                 (namespace_declaration name: (identifier) @name) @module\n\n\
                 (invocation_expression (identifier) @name) @reference.send\n";
        let out = sanitize_tags_query(q);
        assert!(out.contains("@definition.class"));
        assert!(out.contains("@definition.module"));
        assert!(out.contains("@reference.send"));
        // The only stanza classified by a bare `@module` is gone.
        assert!(!out.contains("@module\n") && !out.trim_end().ends_with("@module"));
        // A comment-only / blank stanza is harmless and preserved.
        assert_eq!(sanitize_tags_query("; just a comment"), "; just a comment");
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_marks_inline_test_module_symbols_as_test() {
        // Product code above an inline `mod tests` is not test; symbols at/after the
        // module boundary are. Mirrors this repo's own #[cfg(test)] layout.
        let src = "pub fn real() {}\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       fn helper_test() {}\n\
                   }\n";
        let p = parse("src/lib.rs", "rust", src).unwrap();
        let real = p.defs.iter().find(|d| d.name == "real").unwrap();
        assert!(!real.is_test, "product fn must not be test");
        let ht = p.defs.iter().find(|d| d.name == "helper_test").unwrap();
        assert!(ht.is_test, "symbol inside mod tests must be test");
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_bare_cfg_test_attr_does_not_bury_following_product_code() {
        // Regression (caught dogfooding on memory/): a #[cfg(test)] on a single
        // method inside an impl, with REAL product code after it, must NOT mark the
        // rest of the file as test. Only a `mod tests` boundary does that.
        let src = "impl S {\n\
                       #[cfg(test)]\n\
                       fn test_only(&self) {}\n\
                       pub fn product(&self) {}\n\
                   }\n\
                   mod tests {\n\
                       fn real_test() {}\n\
                   }\n";
        let p = parse("src/store.rs", "rust", src).unwrap();
        let product = p.defs.iter().find(|d| d.name == "product").unwrap();
        assert!(
            !product.is_test,
            "product fn after a bare #[cfg(test)] must stay product"
        );
        let rt = p.defs.iter().find(|d| d.name == "real_test").unwrap();
        assert!(rt.is_test, "fn inside mod tests is still test");
    }

    #[cfg(feature = "code-index")]
    #[test]
    fn parse_marks_whole_test_file_symbols_as_test() {
        let src = "fn it_works() {}\n";
        let p = parse("tests/lifecycle.rs", "rust", src).unwrap();
        assert!(
            p.defs.iter().all(|d| d.is_test),
            "every symbol in a tests/ file is test code"
        );
    }

    #[test]
    fn path_is_test_recognizes_conventions() {
        assert!(path_is_test("tests/lifecycle.rs"));
        assert!(path_is_test("src/__tests__/x.ts"));
        assert!(path_is_test("foo_test.go"));
        assert!(path_is_test("test_thing.py"));
        assert!(path_is_test("widget.test.tsx"));
        assert!(!path_is_test("src/store.rs"));
        assert!(!path_is_test("src/contest.rs")); // 'test' substring, not a segment
    }

    #[test]
    fn path_is_test_java_suffix_respects_junit_case() {
        assert!(path_is_test("src/FooTest.java"));
        assert!(path_is_test("Test.java"));
        assert!(!path_is_test("src/Latest.java"));
        assert!(!path_is_test("Contest.java"));
    }

    #[test]
    fn is_stdlib_combinator_flags_common_methods() {
        for m in ["map", "filter", "unwrap", "collect", "get", "clone", "Some"] {
            assert!(is_stdlib_combinator(m), "{m} should be a combinator");
        }
        for m in ["reindex_notes", "upsert_note", "recall_with"] {
            assert!(!is_stdlib_combinator(m), "{m} is a real symbol");
        }
    }

    #[cfg(not(feature = "code-index"))]
    #[test]
    fn parse_without_feature_errors_with_hint() {
        let err = parse("x.rs", "rust", "fn x() {}").unwrap_err();
        assert!(err.msg.contains("not built into this binary"));
        assert!(err.hint.is_some());
    }
}
