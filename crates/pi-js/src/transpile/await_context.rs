//! Rewrites `await x` as `__pi_au(await __pi_aw(x))`, so an async function resumes in the
//! async context (AsyncLocalStorage stores) it had when it awaited. QuickJS runs `await`
//! continuations without promise hooks, so this cannot be done at run time.

use oxc_ast::ast::{AwaitExpression, Program};
use oxc_ast_visit::{Visit, walk};
use oxc_span::GetSpan;

#[derive(Default)]
struct Collector {
    /// `(offset, closes first, order, text)`.
    edits: Vec<(u32, u8, i64, &'static str)>,
    count: i64,
}

impl<'a> Visit<'a> for Collector {
    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        self.count += 1;
        let order = self.count * 2;
        self.edits.push((it.span.start, 1, order, "__pi_au("));
        self.edits.push((it.argument.span().start, 1, order + 1, "__pi_aw("));
        self.edits.push((it.span.end, 0, -order, "))"));
        walk::walk_await_expression(self, it);
    }
}

/// The source with every `await` instrumented, or `None` when it has none.
pub(crate) fn instrument(source: &str, program: &Program<'_>) -> Option<String> {
    if !source.contains("await") {
        return None;
    }
    let mut collector = Collector::default();
    collector.visit_program(program);
    if collector.edits.is_empty() {
        return None;
    }
    collector.edits.sort_by_key(|(offset, kind, order, _)| (*offset, *kind, *order));
    let mut out = String::with_capacity(source.len() + collector.edits.len() * 8);
    let mut last = 0usize;
    for (offset, _, _, text) in collector.edits {
        let offset = offset as usize;
        out.push_str(&source[last..offset]);
        out.push_str(text);
        last = offset;
    }
    out.push_str(&source[last..]);
    Some(out)
}
