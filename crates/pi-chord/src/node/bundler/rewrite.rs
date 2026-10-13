//! Rewrites one transpiled module into a CommonJS function body for the bundle.

use std::collections::HashMap;

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    Declaration, ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, ModuleExportName, Statement,
};
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::{Semantic, SemanticBuilder};
use oxc_span::{GetSpan, SourceType, Span};

use crate::error::{ChordError, ChordResult};

/// How a module's own code is evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModuleFormat {
    Esm,
    CommonJs,
}

/// Where a request leads: another module of the bundle or a host-resolved external.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    Internal(usize),
    External(String),
}

impl Target {
    fn load(&self) -> String {
        match self {
            Target::Internal(id) => format!("__load({id})"),
            Target::External(specifier) => format!("require({})", js_string(specifier)),
        }
    }
}

/// Resolves a static request at a byte offset; `require` is true for CommonJS `require` calls.
pub(crate) type Resolve<'r> = dyn FnMut(&str, bool, u32) -> ChordResult<Target> + 'r;

/// A rewritten body. `pieces` maps output ranges back to source offsets for source maps.
pub(crate) struct Rewritten {
    pub code: String,
    /// `(output offset, source offset, length)` for each copied range.
    pub pieces: Vec<(usize, usize, usize)>,
    /// Internal module ids for `require` calls in CommonJS code, keyed by specifier.
    pub require_table: Vec<(String, usize)>,
}

pub(crate) fn js_string(text: &str) -> String {
    let mut out = String::new();
    let _ = crate::json::write_json_string(&mut out, text);
    out
}

fn member(base: &str, name: &str) -> String {
    let identifier = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c == '_' || c == '$' || c.is_alphanumeric());
    if identifier { format!("{base}.{name}") } else { format!("{base}[{}]", js_string(name)) }
}

struct Edits {
    edits: Vec<(u32, u32, String)>,
    removed: Vec<Span>,
}

impl Edits {
    fn replace(&mut self, span: Span, text: impl Into<String>) {
        self.edits.push((span.start, span.end, text.into()));
    }

    fn remove(&mut self, span: Span) {
        self.removed.push(span);
        self.replace(span, "");
    }

    fn inside_removed(&self, span: Span) -> bool {
        self.removed.iter().any(|removed| removed.start <= span.start && span.end <= removed.end)
    }
}

fn parse_error(path: &str, source: &str, offset: u32, message: &str) -> ChordError {
    let before = &source[..(offset as usize).min(source.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |text| text.chars().count()) + 1;
    ChordError::error(format!("{path}:{line}:{column}: {message}"))
}

fn export_name(name: &ModuleExportName<'_>) -> String {
    name.name().to_string()
}

struct Context<'s, 'r> {
    source: &'s str,
    edits: Edits,
    header: Vec<String>,
    exports: Vec<(String, String)>,
    imports: HashMap<String, String>,
    next_import: usize,
    resolve: &'s mut Resolve<'r>,
}

impl Context<'_, '_> {
    fn import(&mut self, specifier: &str, offset: u32) -> ChordResult<String> {
        let target = (self.resolve)(specifier, false, offset)?;
        let name = format!("__import{}", self.next_import);
        self.next_import += 1;
        self.header.push(format!("var {name} = __toESM({});", target.load()));
        Ok(name)
    }

    fn local_expression(&self, name: &str) -> String {
        self.imports.get(name).cloned().unwrap_or_else(|| name.to_string())
    }
}

fn module_statements(program_body: &[Statement<'_>], cx: &mut Context<'_, '_>) -> ChordResult<()> {
    for statement in program_body {
        match statement {
            Statement::ImportDeclaration(declaration) => {
                cx.edits.remove(declaration.span);
                let specifier = declaration.source.value.as_str();
                let Some(specifiers) = &declaration.specifiers else {
                    let target = (cx.resolve)(specifier, false, declaration.source.span.start)?;
                    cx.header.push(format!("{};", target.load()));
                    continue;
                };
                let name = cx.import(specifier, declaration.source.span.start)?;
                for item in specifiers {
                    let expression = match item {
                        ImportDeclarationSpecifier::ImportSpecifier(item) => {
                            member(&name, &export_name(&item.imported))
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => format!("{name}.default"),
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => name.clone(),
                    };
                    cx.imports.insert(item.local().name.to_string(), expression);
                }
            }
            Statement::ExportDeclaration(declaration) => {
                let inner = declaration.declaration.span();
                cx.edits.replace(Span::new(declaration.span.start, inner.start), "");
                let names: Vec<String> = match &declaration.declaration {
                    Declaration::VariableDeclaration(variables) => variables
                        .declarations
                        .iter()
                        .flat_map(|declarator| declarator.id.get_binding_identifiers())
                        .map(|identifier| identifier.name.to_string())
                        .collect(),
                    Declaration::FunctionDeclaration(function) => {
                        function.id.iter().map(|id| id.name.to_string()).collect()
                    }
                    Declaration::ClassDeclaration(class) => class.id.iter().map(|id| id.name.to_string()).collect(),
                    _ => Vec::new(),
                };
                cx.exports.extend(names.into_iter().map(|name| (name.clone(), name)));
            }
            Statement::ExportNamedDeclaration(declaration) => {
                cx.edits.remove(declaration.span);
                for item in &declaration.specifiers {
                    let local = export_name(&item.local);
                    cx.exports.push((export_name(&item.exported), cx.local_expression(&local)));
                }
            }
            Statement::ExportFromDeclaration(declaration) => {
                cx.edits.remove(declaration.span);
                let name = cx.import(declaration.source.value.as_str(), declaration.source.span.start)?;
                for item in &declaration.specifiers {
                    cx.exports.push((export_name(&item.exported), member(&name, &export_name(&item.local))));
                }
            }
            Statement::ExportAllDeclaration(declaration) => {
                cx.edits.remove(declaration.span);
                let specifier = declaration.source.value.as_str();
                match &declaration.exported {
                    Some(exported) => {
                        let name = cx.import(specifier, declaration.source.span.start)?;
                        cx.exports.push((export_name(exported), name));
                    }
                    None => {
                        let target = (cx.resolve)(specifier, false, declaration.source.span.start)?;
                        cx.header.push(format!("__reExport(exports, {});", target.load()));
                    }
                }
            }
            Statement::ExportDefaultDeclaration(declaration) => {
                let inner = declaration.declaration.span();
                let prefix = Span::new(declaration.span.start, inner.start);
                let named = match &declaration.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(function) => function.id.as_ref(),
                    ExportDefaultDeclarationKind::ClassDeclaration(class) => class.id.as_ref(),
                    _ => None,
                };
                match named {
                    Some(id) => {
                        cx.edits.replace(prefix, "");
                        cx.exports.push(("default".into(), id.name.to_string()));
                    }
                    None => {
                        cx.edits.replace(prefix, "var __default = ");
                        let declared = matches!(
                            declaration.declaration,
                            ExportDefaultDeclarationKind::FunctionDeclaration(_)
                                | ExportDefaultDeclarationKind::ClassDeclaration(_)
                        );
                        if declared {
                            cx.edits.replace(Span::new(declaration.span.end, declaration.span.end), ";");
                        }
                        cx.exports.push(("default".into(), "__default".into()));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Rewrite references to imported bindings so they read the live import object.
fn rewrite_import_references(semantic: &Semantic<'_>, program_body: &[Statement<'_>], cx: &mut Context<'_, '_>) {
    let scoping = semantic.scoping();
    let nodes = semantic.nodes();
    for statement in program_body {
        let Statement::ImportDeclaration(declaration) = statement else { continue };
        for item in declaration.specifiers.iter().flatten() {
            let Some(expression) = cx.imports.get(item.local().name.as_str()).cloned() else { continue };
            for reference in scoping.get_resolved_reference_ids(item.symbol_id()) {
                let node = scoping.get_reference(*reference).node_id();
                let span = nodes.kind(node).span();
                if cx.edits.inside_removed(span) {
                    continue;
                }
                let name = &cx.source[span.start as usize..span.end as usize];
                let text = match nodes.parent_kind(node) {
                    AstKind::ObjectProperty(property) if property.shorthand => format!("{name}: {expression}"),
                    AstKind::CallExpression(call) if call.callee.span() == span => format!("(0, {expression})"),
                    AstKind::TaggedTemplateExpression(tagged) if tagged.tag.span() == span => {
                        format!("(0, {expression})")
                    }
                    _ => expression.clone(),
                };
                cx.edits.replace(span, text);
            }
        }
    }
}

/// Dynamic imports, `import.meta`, free `require` calls, and defined globals.
fn rewrite_expressions(
    semantic: &Semantic<'_>,
    cx: &mut Context<'_, '_>,
    define: &[(String, String)],
    require_table: &mut Vec<(String, usize)>,
) -> ChordResult<()> {
    let scoping = semantic.scoping();
    let unresolved = |reference: oxc_semantic::ReferenceId| scoping.get_reference(reference).symbol_id().is_none();
    for node in semantic.nodes().iter() {
        match node.kind() {
            AstKind::ImportExpression(import) => {
                let load = match &import.source {
                    Expression::StringLiteral(literal) => {
                        (cx.resolve)(literal.value.as_str(), false, literal.span.start)?.load()
                    }
                    other => {
                        let span = other.span();
                        format!("require({})", &cx.source[span.start as usize..span.end as usize])
                    }
                };
                cx.edits.replace(import.span, format!("Promise.resolve().then(() => __toESM({load}))"));
            }
            AstKind::ImportMeta(meta) => cx.edits.replace(meta.span, "({})"),
            AstKind::CallExpression(call) => {
                let Expression::Identifier(callee) = &call.callee else { continue };
                if callee.name != "require" || !unresolved(callee.reference_id()) || call.arguments.len() != 1 {
                    continue;
                }
                let Some(Expression::StringLiteral(literal)) = call.arguments[0].as_expression() else { continue };
                if let Target::Internal(id) = (cx.resolve)(literal.value.as_str(), true, literal.span.start)? {
                    require_table.push((literal.value.to_string(), id));
                }
            }
            AstKind::IdentifierReference(identifier) if unresolved(identifier.reference_id()) => {
                if let Some((_, value)) = define.iter().find(|(key, _)| key == identifier.name.as_str()) {
                    cx.edits.replace(identifier.span, value.clone());
                }
            }
            AstKind::StaticMemberExpression(expression) => {
                let text = &cx.source[expression.span.start as usize..expression.span.end as usize];
                let Some((_, value)) = define.iter().find(|(key, _)| key.contains('.') && key == text) else {
                    continue;
                };
                let mut root = &expression.object;
                while let Expression::StaticMemberExpression(inner) = root {
                    root = &inner.object;
                }
                if let Expression::Identifier(identifier) = root
                    && unresolved(identifier.reference_id())
                {
                    cx.edits.replace(expression.span, value.clone());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Splice the edits into `source`, keeping the copied ranges for source maps.
fn splice(source: &str, mut edits: Vec<(u32, u32, String)>, prefix: String, output: &mut Rewritten) {
    edits.sort_by_key(|(start, end, _)| (*start, *end));
    output.code.push_str(&prefix);
    let mut cursor = 0usize;
    for (start, end, text) in edits {
        let (start, end) = (start as usize, end as usize);
        if start < cursor {
            continue;
        }
        if start > cursor {
            output.pieces.push((output.code.len(), cursor, start - cursor));
            output.code.push_str(&source[cursor..start]);
        }
        output.code.push_str(&text);
        cursor = end;
    }
    if cursor < source.len() {
        output.pieces.push((output.code.len(), cursor, source.len() - cursor));
        output.code.push_str(&source[cursor..]);
    }
}

/// Rewrite one module body. `path` names it in errors.
pub(crate) fn rewrite(
    path: &str,
    source: &str,
    format: ModuleFormat,
    define: &[(String, String)],
    resolve: &mut Resolve<'_>,
) -> ChordResult<Rewritten> {
    let allocator = Allocator::default();
    let source_type = match format {
        ModuleFormat::Esm => SourceType::mjs(),
        ModuleFormat::CommonJs => SourceType::cjs(),
    };
    let options =
        ParseOptions { allow_return_outside_function: format == ModuleFormat::CommonJs, ..ParseOptions::default() };
    let parsed = Parser::new(&allocator, source, source_type).with_options(options).parse();
    if let Some(error) = parsed.diagnostics.first() {
        let offset = error.labels.as_ref().first().map_or(0, |label| label.offset());
        return Err(parse_error(path, source, offset, &error.to_string()));
    }
    let program = parsed.program;
    let semantic = SemanticBuilder::new().with_build_nodes(true).build(&program).semantic;
    let mut cx = Context {
        source,
        edits: Edits { edits: Vec::new(), removed: Vec::new() },
        header: Vec::new(),
        exports: Vec::new(),
        imports: HashMap::new(),
        next_import: 0,
        resolve,
    };
    let mut output = Rewritten { code: String::new(), pieces: Vec::new(), require_table: Vec::new() };
    if format == ModuleFormat::Esm {
        module_statements(&program.body, &mut cx)?;
        rewrite_import_references(&semantic, &program.body, &mut cx);
    }
    let mut require_table = Vec::new();
    rewrite_expressions(&semantic, &mut cx, define, &mut require_table)?;
    output.require_table = require_table;
    let mut prefix = String::new();
    if format == ModuleFormat::Esm {
        prefix.push_str("__markESM(exports);\n");
        if !cx.exports.is_empty() {
            let getters: Vec<String> = cx
                .exports
                .iter()
                .map(|(name, expression)| format!("{}: () => {expression}", js_string(name)))
                .collect();
            prefix.push_str(&format!("__export(exports, {{ {} }});\n", getters.join(", ")));
        }
    }
    for line in &cx.header {
        prefix.push_str(line);
        prefix.push('\n');
    }
    splice(source, cx.edits.edits, prefix, &mut output);
    Ok(output)
}
