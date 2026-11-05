//! A namespaced ES module reads `import.meta` through a hoisted function that first sets the
//! module's original location on it, so a read that a cycle runs before the module body sees it too.

use oxc_allocator::Allocator;
use oxc_ast::ast::{ImportMeta, NewExpression};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::error::JsError;

#[derive(Default)]
struct Reads {
    metas: Vec<Span>,
    callees: Vec<Span>,
}

impl<'a> Visit<'a> for Reads {
    fn visit_import_meta(&mut self, it: &ImportMeta) {
        self.metas.push(it.span);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        self.callees.push(it.callee.span());
        walk::walk_new_expression(self, it);
    }
}

/// `source` with each `import.meta` replaced by a call of a hoisted function. Its first call runs
/// `locate`, which sets the original location on `import.meta`; every call returns `import.meta`.
pub(crate) fn lazy_import_meta(source: &str, locate: &str) -> Result<String, JsError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    if !parsed.diagnostics.is_empty() {
        return Err(JsError::with_name("SyntaxError", parsed.diagnostics[0].message.to_string()));
    }
    let mut reads = Reads::default();
    reads.visit_program(&parsed.program);
    if reads.metas.is_empty() {
        return Ok(source.to_string());
    }
    let mut name = String::from("__pi_import_meta");
    while source.contains(&name) {
        name.push('_');
    }
    let call = format!("{name}()");
    let mut edits: Vec<(u32, u32, &str)> =
        reads.metas.iter().map(|span| (span.start, span.end, call.as_str())).collect();
    // `new import.meta.X()` must still construct `import.meta.X`, not the helper's result.
    for callee in reads.callees.iter().filter(|callee| reads.metas.iter().any(|meta| meta.start == callee.start)) {
        edits.push((callee.start, callee.start, "("));
        edits.push((callee.end, callee.end, ")"));
    }
    edits.sort_by_key(|&(start, end, _)| (start, end));
    let mut out = String::with_capacity(source.len() + edits.len() * call.len() + locate.len() + 96);
    let mut last = 0;
    for (start, end, text) in edits {
        out.push_str(&source[last..start as usize]);
        out.push_str(text);
        last = end as usize;
    }
    out.push_str(&source[last..]);
    out.push_str(&format!(
        "\n;function {name}() {{ if (!{name}.done) {{ {name}.done = true; {locate}}} return import.meta; }}\n"
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_meta_reads_go_through_one_hoisted_helper() {
        let source = "const a = import.meta.url;\nnew import.meta.Thing(import.meta.x);\nimport.meta.y = 1;\n";
        let expected = "const a = __pi_import_meta().url;\nnew (__pi_import_meta().Thing)(__pi_import_meta().x);\n\
                        __pi_import_meta().y = 1;\n\n;function __pi_import_meta() { if (!__pi_import_meta.done) \
                        { __pi_import_meta.done = true; L; } return import.meta; }\n";
        assert_eq!(lazy_import_meta(source, "L; ").unwrap(), expected);
    }

    #[test]
    fn the_helper_name_avoids_names_in_the_source() {
        let out = lazy_import_meta("const __pi_import_meta = import.meta;\n", "").unwrap();
        assert!(out.starts_with("const __pi_import_meta = __pi_import_meta_();\n"), "{out}");
        assert_eq!(lazy_import_meta("export const x = new.target;", "").ok(), None);
        assert_eq!(lazy_import_meta("export const x = 1;\n", "L; ").unwrap(), "export const x = 1;\n");
    }
}
