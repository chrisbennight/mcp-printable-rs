//! The confined-source gate.
//!
//! OpenSCAD source is untrusted. Included source (`include`/`use`) and the
//! DXF/OFF/STL import directives can perform file reads that are not visible in
//! the submitted text, so they are refused in confined mode. Geometry/surface
//! imports must use a **literal** path, so the complete read set can be checked
//! and snapshotted before OpenSCAD starts; each literal `import(...)` /
//! `surface(...)` path is rewritten to a private, immutable snapshot the
//! subprocess cannot escape.
//!
//! The tokenizer ([`tokenize`]) is a [`logos`] lexer over the small subset
//! needed to police file reads; [`confine_source`] walks the tokens, enforces
//! the forbidden set, and rewrites literal import/surface paths through an
//! injected snapshot function (the workspace wires the real one).
//!
//! The security property is one-directional: the gate must snapshot **every**
//! path OpenSCAD could read, so it errs toward over-confinement. Identifiers
//! follow OpenSCAD's ASCII grammar; a non-ASCII character can only split a
//! token, never fuse a keyword to a neighbour, so `import`/`surface` are never
//! missed.

use std::ops::Range;

use logos::{FilterResult, Lexer, Logos};

/// Directives refused in confined mode: they read files not named in the source.
const FORBIDDEN: &[&str] = &[
    "include",
    "use",
    "import_dxf",
    "import_off",
    "import_stl",
    "dxf_dim",
    "dxf_cross",
];

/// Upper bound on the number of `import`/`surface` file references a single
/// confined source may contain. Snapshotting each reference is real work — a
/// temp directory plus up to the workspace transfer cap copied — so an
/// unbounded count would let one source drive unbounded snapshot I/O. The cap
/// is enforced during the side-effect-free validation pass, so an over-limit
/// source is rejected before any snapshot runs, and it bounds the worst-case
/// snapshot I/O of an accepted source to `MAX_IMPORTS` × the transfer cap. It
/// is far above any legitimate print-in-place model, which imports a handful of
/// meshes; raise it if a real assembly ever needs more.
const MAX_IMPORTS: usize = 64;

/// Errors from the source gate. The messages are stable operator-facing text.
#[derive(Debug, Clone, PartialEq, Eq, Default, thiserror::Error)]
pub enum GateError {
    #[error("unterminated OpenSCAD block comment")]
    UnterminatedBlockComment,
    // `#[default]` only satisfies logos's requirement that the lexer error type
    // be `Default`. Every character matches some token, so a defaulted error is
    // never actually produced by lexing.
    #[default]
    #[error("unterminated OpenSCAD string")]
    UnterminatedString,
    #[error("OpenSCAD {0} is not allowed in confined mode")]
    Forbidden(String),
    #[error("unterminated OpenSCAD {0} call")]
    UnterminatedCall(String),
    #[error("OpenSCAD {0} requires a literal workspace path")]
    RequiresLiteralPath(String),
    #[error("OpenSCAD {0} requires exactly one literal file argument")]
    RequiresOneFileArgument(String),
    #[error("OpenSCAD source references too many files (maximum {0})")]
    TooManyImports(usize),
    #[error("OpenSCAD caller cannot declare reserved product symbol {0}")]
    ReservedProductSymbol(String),
    /// A snapshot of a referenced path could not be taken. Carries the injected
    /// snapshot function's message so the workspace's own error text surfaces.
    #[error("{0}")]
    Snapshot(String),
}

impl GateError {
    pub fn code(&self) -> &'static str {
        match self {
            GateError::UnterminatedBlockComment => "unterminated_block_comment",
            GateError::UnterminatedString => "unterminated_string",
            GateError::Forbidden(_) => "forbidden_directive",
            GateError::UnterminatedCall(_) => "unterminated_call",
            GateError::RequiresLiteralPath(_) => "requires_literal_path",
            GateError::RequiresOneFileArgument(_) => "requires_one_file_argument",
            GateError::TooManyImports(_) => "too_many_imports",
            GateError::ReservedProductSymbol(_) => "reserved_product_symbol",
            GateError::Snapshot(_) => "snapshot",
        }
    }
}

/// One token of the policed OpenSCAD subset. `Str` carries the decoded string
/// value (escapes resolved); `Sym` is any single non-identifier, non-string
/// character. Whitespace and comments are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Logos)]
#[logos(error = GateError)]
#[logos(skip r"[ \t\r\n\x0b\x0c]+")]
#[logos(skip("//[^\n]*", allow_greedy = true))]
pub enum Token {
    #[token("/*", block_comment)]
    BlockComment,
    #[token("\"", string_literal)]
    Str(String),
    // ASCII identifiers, matching OpenSCAD's own grammar (the actual consumer of
    // this source). This is deliberately conservative: a non-ASCII character is
    // a symbol, so it can only *split* a token, never glue a keyword to its
    // neighbour. That means the gate detects `import`/`surface` at least as
    // readily as OpenSCAD does and can never under-confine — a raw path cannot
    // slip through by prefixing the keyword with an exotic letter.
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*", |lex| lex.slice().to_owned())]
    Ident(String),
    // Lowest priority: the fallback for any single non-whitespace character
    // not already claimed by a string, identifier, or comment.
    #[regex(r"[^ \t\r\n\x0b\x0c]", |lex| lex.slice().chars().next().unwrap(), priority = 0)]
    Sym(char),
}

/// Consume a `/* ... */` block comment (already past the opening `/*`), or fail
/// if it is never closed.
fn block_comment(lex: &mut Lexer<Token>) -> FilterResult<(), GateError> {
    match lex.remainder().find("*/") {
        Some(end) => {
            lex.bump(end + 2);
            FilterResult::Skip
        }
        None => FilterResult::Error(GateError::UnterminatedBlockComment),
    }
}

/// Read a `"..."` string (already past the opening quote). A backslash drops
/// itself and keeps the next character verbatim (`\n` is a literal `n`, `\"`
/// a literal quote).
fn string_literal(lex: &mut Lexer<Token>) -> Result<String, GateError> {
    let rem = lex.remainder();
    let mut value = String::new();
    let mut chars = rem.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            lex.bump(i + c.len_utf8());
            return Ok(value);
        }
        if c == '\\' {
            match chars.next() {
                Some((_, next)) => value.push(next),
                None => value.push('\\'),
            }
        } else {
            value.push(c);
        }
    }
    Err(GateError::UnterminatedString)
}

/// Tokenize `code`, or return the first lexing error (an unterminated comment or
/// string). Each token carries its byte span in `code`.
pub fn tokenize(code: &str) -> Result<Vec<(Token, Range<usize>)>, GateError> {
    let mut out = Vec::new();
    let mut lex = Token::lexer(code);
    while let Some(result) = lex.next() {
        out.push((result?, lex.span()));
    }
    Ok(out)
}

/// The sole literal file argument of one `import`/`surface` call: its decoded
/// value and its byte span in the source (for rewriting).
struct FileArgument {
    value: String,
    span: Range<usize>,
}

/// Extract the literal file argument from the call whose `(` is at `open_paren`.
/// Accepts a positional first string argument or a `file = "..."` keyword, and
/// enforces exactly one.
fn literal_file_argument(
    tokens: &[(Token, Range<usize>)],
    open_paren: usize,
    call_name: &str,
) -> Result<FileArgument, GateError> {
    // Group tokens into comma-separated arguments until the matching `)`.
    let mut arguments: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    let mut closed = false;
    for (idx, (tok, _)) in tokens.iter().enumerate().skip(open_paren + 1) {
        match tok {
            Token::Sym('(') => {
                depth += 1;
                current.push(idx);
            }
            Token::Sym(')') if depth == 0 => {
                arguments.push(std::mem::take(&mut current));
                closed = true;
                break;
            }
            Token::Sym(')') => {
                depth -= 1;
                current.push(idx);
            }
            Token::Sym(',') if depth == 0 => {
                arguments.push(std::mem::take(&mut current));
            }
            _ => current.push(idx),
        }
    }
    if !closed {
        return Err(GateError::UnterminatedCall(call_name.to_string()));
    }

    let mut file_args: Vec<usize> = Vec::new();
    for (position, argument) in arguments.iter().enumerate() {
        // `file = "..."`
        if argument.len() >= 2
            && matches!(&tokens[argument[0]].0, Token::Ident(name) if name == "file")
            && matches!(&tokens[argument[1]].0, Token::Sym('='))
        {
            if argument.len() != 3 || !matches!(&tokens[argument[2]].0, Token::Str(_)) {
                return Err(GateError::RequiresLiteralPath(call_name.to_string()));
            }
            file_args.push(argument[2]);
        } else if position == 0
            && argument.len() == 1
            && matches!(&tokens[argument[0]].0, Token::Str(_))
        {
            file_args.push(argument[0]);
        }
    }

    match file_args.as_slice() {
        [] => Err(GateError::RequiresLiteralPath(call_name.to_string())),
        [one] => {
            let (Token::Str(value), span) = (&tokens[*one].0, tokens[*one].1.clone()) else {
                unreachable!("only string tokens are pushed as file arguments");
            };
            Ok(FileArgument {
                value: value.clone(),
                span,
            })
        }
        _ => Err(GateError::RequiresOneFileArgument(call_name.to_string())),
    }
}

/// Encode `s` as a deterministic JSON string literal. JSON metacharacters are
/// escaped and non-ASCII or control characters use `\uXXXX` so the result is a
/// safe OpenSCAD literal.
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                for unit in c.encode_utf16(&mut [0u16; 2]) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Rewrite `code` for confined execution: refuse the forbidden directives and
/// replace every literal `import(...)`/`surface(...)` path with the result of
/// `snapshot(path)` (JSON-quoted). `snapshot` maps a referenced workspace path
/// to its private snapshot path, or returns a [`GateError::Snapshot`] message.
///
/// Validation is a side-effect-free first pass — the forbidden set, import
/// arity and literal-ness, and the [`MAX_IMPORTS`] count cap are all checked
/// before `snapshot` is called even once — so a source rejected as malformed or
/// over-limit performs **zero** snapshots. Snapshotting then runs as a second
/// pass over the (capped) collected paths; it is not transactional, so a
/// snapshot that fails on a later path does not undo earlier ones, but the
/// total snapshot work is bounded by `MAX_IMPORTS` × the workspace transfer cap
/// regardless of where a failure lands.
///
/// The unconfined case (pass the source through untouched) is the caller's
/// decision; this function always confines.
pub fn confine_source(
    code: &str,
    mut snapshot: impl FnMut(&str) -> Result<String, GateError>,
) -> Result<String, GateError> {
    let tokens = tokenize(code)?;

    // Pass 1 — validate the whole source with no side effects: reject any
    // forbidden directive, malformed import/surface call, or over-limit import
    // count anywhere in the input, and collect the literal path spans to
    // rewrite. No snapshot runs until the entire source is accepted, so a
    // source rejected as malformed or over-limit drives zero snapshot work —
    // each snapshot copies up to the workspace transfer cap into a fresh temp
    // directory, so eagerly snapshotting while validating would let a source
    // that ends in a rejection burn disk I/O first.
    let mut pending: Vec<(usize, usize, String)> = Vec::new();
    for i in 0..tokens.len() {
        let Token::Ident(name) = &tokens[i].0 else {
            continue;
        };
        if FORBIDDEN.contains(&name.as_str()) {
            return Err(GateError::Forbidden(name.clone()));
        }
        if name != "import" && name != "surface" {
            continue;
        }
        let open = i + 1;
        if open >= tokens.len() || !matches!(&tokens[open].0, Token::Sym('(')) {
            continue;
        }
        let arg = literal_file_argument(&tokens, open, name)?;
        pending.push((arg.span.start, arg.span.end, arg.value));
        if pending.len() > MAX_IMPORTS {
            return Err(GateError::TooManyImports(MAX_IMPORTS));
        }
    }

    // Pass 2 — the source validated; snapshot each referenced path, then splice.
    // The paths are collected in token-iteration order, which is NOT necessarily
    // ascending by source position: an outer call's argument scan can reach past
    // a nested call, so a call processed later can own an earlier span. Splice
    // strictly high-to-low by start position so an earlier rewrite never shifts a
    // not-yet-applied span out from under its offsets. (Distinct string tokens
    // have disjoint spans, so ordering by start alone is unambiguous.)
    let mut rewrites: Vec<(usize, usize, String)> = Vec::with_capacity(pending.len());
    for (start, end, path) in pending {
        let snap = snapshot(&path)?;
        rewrites.push((start, end, json_quote(&snap)));
    }
    rewrites.sort_by_key(|&(start, ..)| start);
    let mut confined = code.to_string();
    for (start, end, replacement) in rewrites.into_iter().rev() {
        confined.replace_range(start..end, &replacement);
    }
    Ok(confined)
}

/// The literal file path of every `import`/`surface` call in `code`, in source
/// order. Read-only (no snapshotting). Used to assert the security invariant —
/// that confined output references only snapshot paths, never a raw workspace
/// path — over both curated cases and fuzz input.
pub fn import_surface_paths(code: &str) -> Result<Vec<String>, GateError> {
    let tokens = tokenize(code)?;
    let mut paths = Vec::new();
    for i in 0..tokens.len() {
        let Token::Ident(name) = &tokens[i].0 else {
            continue;
        };
        if name != "import" && name != "surface" {
            continue;
        }
        let open = i + 1;
        if open >= tokens.len() || !matches!(&tokens[open].0, Token::Sym('(')) {
            continue;
        }
        paths.push(literal_file_argument(&tokens, open, name)?.value);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic stub snapshot for tests: maps a path to a fixed marker,
    /// so rewrites are checkable without the workspace.
    fn stub(path: &str) -> Result<String, GateError> {
        Ok(format!("/snap/{path}"))
    }

    fn kinds(code: &str) -> Vec<Token> {
        tokenize(code)
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn tokenizes_identifiers_strings_symbols() {
        let toks = kinds(r#"cube ( "a" , 3 )"#);
        assert_eq!(
            toks,
            vec![
                Token::Ident("cube".into()),
                Token::Sym('('),
                Token::Str("a".into()),
                Token::Sym(','),
                Token::Sym('3'),
                Token::Sym(')'),
            ]
        );
    }

    #[test]
    fn skips_line_and_block_comments() {
        let toks = kinds("a // line\n b /* block */ c");
        assert_eq!(
            toks,
            vec![
                Token::Ident("a".into()),
                Token::Ident("b".into()),
                Token::Ident("c".into()),
            ]
        );
    }

    #[test]
    fn string_escapes_drop_the_backslash() {
        // \" is a literal quote, \\ a literal backslash, \n a literal n.
        let toks = kinds(r#""a\"b\\c\n""#);
        assert_eq!(toks, vec![Token::Str(r#"a"b\cn"#.into())]);
    }

    #[test]
    fn unterminated_block_comment_errors() {
        assert_eq!(
            tokenize("a /* nope"),
            Err(GateError::UnterminatedBlockComment)
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert_eq!(tokenize("\"nope"), Err(GateError::UnterminatedString));
    }

    #[test]
    fn forbidden_directives_are_refused() {
        for directive in FORBIDDEN {
            let code = format!("{directive} something");
            let err = confine_source(&code, stub).unwrap_err();
            assert_eq!(err, GateError::Forbidden((*directive).to_string()));
            assert_eq!(
                err.to_string(),
                format!("OpenSCAD {directive} is not allowed in confined mode")
            );
        }
    }

    #[test]
    fn plain_code_is_returned_unchanged() {
        let code = "cube([1,2,3]); sphere(r=4);";
        assert_eq!(confine_source(code, stub).unwrap(), code);
    }

    #[test]
    fn import_positional_path_is_snapshotted() {
        let out = confine_source(r#"import("part.stl");"#, stub).unwrap();
        assert_eq!(out, r#"import("/snap/part.stl");"#);
    }

    #[test]
    fn surface_file_keyword_path_is_snapshotted() {
        let out = confine_source(r#"surface(file = "height.png", center=true);"#, stub).unwrap();
        assert_eq!(out, r#"surface(file = "/snap/height.png", center=true);"#);
    }

    #[test]
    fn import_with_no_literal_path_is_refused() {
        let err = confine_source("import(myvar);", stub).unwrap_err();
        assert_eq!(err, GateError::RequiresLiteralPath("import".into()));
    }

    #[test]
    fn import_with_two_file_arguments_is_refused() {
        let err = confine_source(r#"import("a.stl", file="b.stl");"#, stub).unwrap_err();
        assert_eq!(err, GateError::RequiresOneFileArgument("import".into()));
    }

    #[test]
    fn unterminated_import_call_errors() {
        let err = confine_source(r#"import("a.stl""#, stub).unwrap_err();
        assert_eq!(err, GateError::UnterminatedCall("import".into()));
    }

    #[test]
    fn json_quote_is_stable_for_ascii_and_unicode() {
        assert_eq!(json_quote("plain"), r#""plain""#);
        assert_eq!(json_quote(r#"a"b\c"#), r#""a\"b\\c""#);
        // Non-ASCII uses deterministic \uXXXX escapes.
        assert_eq!(json_quote("café"), "\"caf\\u00e9\"");
    }

    #[test]
    fn non_ascii_character_splits_and_never_hides_a_keyword() {
        // A non-ASCII character is a symbol, so it splits rather than fuses: an
        // `import` prefixed by one is still recognised and its path is still
        // snapshotted — conservative over-confinement, never a leaked raw path.
        let out = confine_source("éimport(\"x.stl\");", stub).unwrap();
        assert_eq!(out, "éimport(\"/snap/x.stl\");");
    }

    #[test]
    fn import_is_rewritten_mid_source() {
        let out = confine_source(r#"translate([10,0,0]) import("mid.stl");"#, stub).unwrap();
        assert_eq!(out, r#"translate([10,0,0]) import("/snap/mid.stl");"#);
    }

    #[test]
    fn whitespace_around_the_call_is_tolerated() {
        let out = confine_source(r#"import ( "spaced.stl" ) ;"#, stub).unwrap();
        assert_eq!(out, r#"import ( "/snap/spaced.stl" ) ;"#);
    }

    #[test]
    fn confined_output_references_only_snapshot_paths() {
        // The security invariant: after confining, every import/surface literal
        // is a snapshot path — never the original workspace path.
        let snap = |_p: &str| Ok::<_, GateError>("/snapshots/x".to_string());
        let confined = confine_source(r#"import("a.stl"); surface(file="b.png");"#, snap).unwrap();
        for path in import_surface_paths(&confined).unwrap() {
            assert!(
                path.starts_with("/snapshots/"),
                "un-snapshotted path: {path:?}"
            );
        }
    }

    #[test]
    fn rejected_source_runs_no_snapshots() {
        // Snapshotting is a real side effect (a temp dir + up to the transfer
        // cap copied per call), so a source that is ultimately rejected must
        // not snapshot any of the valid imports that precede the rejection.

        // Valid imports followed by a forbidden directive.
        let calls = std::cell::Cell::new(0usize);
        let result = confine_source(
            "import(\"a.stl\");\nimport(\"b.stl\");\ninclude <evil.scad>",
            |p| {
                calls.set(calls.get() + 1);
                Ok::<_, GateError>(format!("/snap/{p}"))
            },
        );
        assert!(matches!(result, Err(GateError::Forbidden(_))));
        assert_eq!(calls.get(), 0, "a rejected source snapshotted an import");

        // Valid import followed by a malformed (non-literal) import.
        let calls = std::cell::Cell::new(0usize);
        let result = confine_source("import(\"a.stl\");\nimport(nope);", |p| {
            calls.set(calls.get() + 1);
            Ok::<_, GateError>(format!("/snap/{p}"))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 0, "a malformed source snapshotted an import");

        // More imports than the cap: rejected during validation, zero snapshots.
        let calls = std::cell::Cell::new(0usize);
        let src: String = (0..=MAX_IMPORTS)
            .map(|i| format!("import(\"m{i}.stl\");\n"))
            .collect();
        let result = confine_source(&src, |p| {
            calls.set(calls.get() + 1);
            Ok::<_, GateError>(format!("/snap/{p}"))
        });
        assert!(matches!(
            result,
            Err(GateError::TooManyImports(MAX_IMPORTS))
        ));
        assert_eq!(calls.get(), 0, "an over-limit source snapshotted an import");
    }

    #[test]
    fn import_count_at_the_cap_is_accepted() {
        // Exactly MAX_IMPORTS references are fine — the cap only rejects strictly
        // more — and each is snapshotted once.
        let src: String = (0..MAX_IMPORTS)
            .map(|i| format!("import(\"m{i}.stl\");\n"))
            .collect();
        let calls = std::cell::Cell::new(0usize);
        let out = confine_source(&src, |p| {
            calls.set(calls.get() + 1);
            Ok::<_, GateError>(format!("/snap/{p}"))
        })
        .unwrap();
        assert_eq!(calls.get(), MAX_IMPORTS);
        for path in import_surface_paths(&out).unwrap() {
            assert!(path.starts_with("/snap/"), "un-snapshotted path: {path:?}");
        }
    }

    #[test]
    fn nested_call_out_of_order_spans_splice_without_panicking() {
        // An outer call's argument scan reaches past a nested call, so the outer
        // call's file arg (`late.stl`, later in the source) is collected *before*
        // the nested call's (`long_early_name.stl`, earlier). A snapshot shorter
        // than the original literal then shifts the string; the second rewrite
        // must still land rather than index past the shortened string. Regression
        // for the out-of-bounds splice a fuzzer found: with the paths spliced in
        // collection order the stale offset panicked.
        let src = r#"surface(surface(file="long_early_name.stl"),file="late.stl")"#;
        let snap = |p: &str| Ok::<_, GateError>(format!("/s/{}", p.len()));
        let out = confine_source(src, snap).unwrap();
        assert!(
            !out.contains("long_early_name.stl"),
            "raw path survived: {out}"
        );
        assert!(!out.contains("late.stl"), "raw path survived: {out}");
        for path in import_surface_paths(&out).unwrap() {
            assert!(path.starts_with("/s/"), "un-snapshotted path: {path:?}");
        }
    }
}
