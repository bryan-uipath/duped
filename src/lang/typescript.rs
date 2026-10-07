//! TypeScript / JavaScript extractor over the tree-sitter TypeScript and TSX grammars.

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Parser};

use super::{collapse_whitespace, has_token, join_scope, push_field, signature};
use crate::record::{
    Field, FieldKind, FunctionRecord, Language, Location, Param, Record, TypeKind, TypeRecord,
};

/// Parse one file and return its top-level functions, class members and types.
pub fn extract(parser: &mut Parser, source: &str, rel: &str, language: Language) -> Vec<Record> {
    // JSX is only valid in the TSX grammar, which also parses plain JS.
    let grammar = if rel.ends_with(".ts") || rel.ends_with(".mts") || rel.ends_with(".cts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    } else {
        tree_sitter_typescript::LANGUAGE_TSX
    };
    parser
        .set_language(&grammar.into())
        .expect("bundled grammar is compatible");
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut extractor = Extractor {
        source,
        rel,
        language,
        records: Vec::new(),
        listed_exports: Vec::new(),
        overload_docs: HashMap::new(),
    };
    extractor.statements(tree.root_node(), &[]);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    language: Language,
    records: Vec<Record>,
    /// Per block: names exported by `export { a }` / `export default a`.
    listed_exports: Vec<HashSet<String>>,
    /// Doc of a skipped overload signature, keyed by qualified name, for its implementation.
    overload_docs: HashMap<String, String>,
}

/// Where a declaration's doc comment and line span start, and whether it is exported.
#[derive(Clone, Copy)]
struct Decl<'t> {
    anchor: Node<'t>,
    exported: bool,
}

impl<'a> Extractor<'a> {
    fn statements(&mut self, block: Node, scope: &[String]) {
        self.listed_exports.push(self.export_lists(block));
        let implemented = implemented_functions(block, self.source);
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            let decl = Decl {
                anchor: child,
                exported: false,
            };
            self.statement(child, scope, decl, &implemented);
        }
        self.listed_exports.pop();
    }

    fn statement(
        &mut self,
        node: Node,
        scope: &[String],
        decl: Decl,
        implemented: &HashSet<String>,
    ) {
        match node.kind() {
            "export_statement" => {
                let exported = Decl {
                    exported: true,
                    ..decl
                };
                if let Some(inner) = node.child_by_field_name("declaration") {
                    self.statement(inner, scope, exported, implemented);
                } else if let Some(value) = node.child_by_field_name("value").map(unparen) {
                    // `export default function () {}` / `export default class {}`
                    match value.kind() {
                        "function_expression" | "arrow_function" | "generator_function" => {
                            let name = value
                                .child_by_field_name("name")
                                .map_or_else(|| "default".into(), |n| self.text(n));
                            self.function(name, value, scope, exported);
                        }
                        "class" => self.class(value, scope, exported),
                        _ => {}
                    }
                }
            }
            "ambient_declaration" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "statement_block" {
                        // `declare global { … }`
                        self.statements(child, scope);
                    } else {
                        self.statement(child, scope, decl, implemented);
                    }
                }
            }
            // A bare `namespace X {}` parses as an expression statement.
            "expression_statement" => {
                if let Some(inner) = node
                    .named_child(0)
                    .filter(|n| matches!(n.kind(), "internal_module" | "module"))
                {
                    self.statement(inner, scope, decl, implemented);
                }
            }
            "internal_module" | "module" => {
                if let (Some(name), Some(body)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("body"),
                ) {
                    let mut inner = scope.to_vec();
                    inner.push(unquote(&self.text(name)).to_string());
                    self.statements(body, &inner);
                }
            }
            "function_declaration" | "generator_function_declaration" => {
                let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
                    return;
                };
                let decl = self.listed(decl, &name);
                self.function(name, node, scope, decl);
            }
            // `declare function` and overload signatures; an overload yields to its implementation.
            "function_signature" => {
                let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
                    return;
                };
                if implemented.contains(&name) {
                    if let Some(doc) = self.doc(decl.anchor) {
                        self.overload_docs
                            .entry(qualified(scope, &name))
                            .or_insert(doc);
                    }
                } else {
                    let decl = self.listed(decl, &name);
                    self.function(name, node, scope, decl);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = node.walk();
                for declarator in node
                    .named_children(&mut cursor)
                    .filter(|n| n.kind() == "variable_declarator")
                {
                    let (Some(name), Some(value)) = (
                        declarator.child_by_field_name("name"),
                        declarator.child_by_field_name("value").map(unparen),
                    ) else {
                        continue;
                    };
                    if matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "generator_function"
                    ) {
                        let name = self.text(name);
                        let decl = self.listed(decl, &name);
                        self.function(name, value, scope, decl);
                    }
                }
            }
            "class_declaration" | "abstract_class_declaration" => self.class(node, scope, decl),
            "interface_declaration" => self.interface(node, scope, decl),
            "type_alias_declaration" => self.type_alias(node, scope, decl),
            "enum_declaration" => self.enumeration(node, scope, decl),
            _ => {}
        }
    }

    fn function(&mut self, name: String, node: Node, scope: &[String], decl: Decl) {
        let doc = self
            .doc(decl.anchor)
            .or_else(|| self.overload_docs.remove(&qualified(scope, &name)));
        let record = FunctionRecord {
            language: self.language,
            name,
            scope: join_scope(scope),
            params: self.params(node),
            returns: self.returns(node),
            exported: decl.exported,
            location: self.location(decl.anchor, node),
            doc,
        };
        self.records.push(Record::Function(record));
    }

    fn class(&mut self, node: Node, scope: &[String], decl: Decl) {
        let name = node
            .child_by_field_name("name")
            .map_or_else(|| "default".to_string(), |n| self.text(n));
        let decl = self.listed(decl, &name);
        let mut extends = Vec::new();
        let mut cursor = node.walk();
        if let Some(heritage) = node
            .named_children(&mut cursor)
            .find(|n| n.kind() == "class_heritage")
        {
            let mut clauses = heritage.walk();
            for clause in heritage.named_children(&mut clauses) {
                match clause.kind() {
                    "extends_clause" => {
                        extends.push(
                            self.text(clause)
                                .trim_start_matches("extends")
                                .trim()
                                .to_string(),
                        );
                    }
                    "implements_clause" => {
                        let mut types = clause.walk();
                        extends.extend(clause.named_children(&mut types).map(|t| self.text(t)));
                    }
                    _ => {}
                }
            }
        }

        let mut member_scope = scope.to_vec();
        member_scope.push(name.clone());
        let mut fields = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            let implemented = implemented_methods(body, self.source);
            let mut members = body.walk();
            for member in body.named_children(&mut members) {
                let anchor = Decl {
                    anchor: leading_decorators(member),
                    exported: decl.exported && !is_private(member),
                };
                self.class_member(member, &member_scope, anchor, &implemented, &mut fields);
            }
        }

        self.push_type(name, TypeKind::Class, fields, extends, node, scope, decl);
    }

    fn class_member(
        &mut self,
        member: Node,
        scope: &[String],
        decl: Decl,
        implemented: &HashSet<String>,
        fields: &mut Vec<Field>,
    ) {
        match member.kind() {
            "public_field_definition" => push_field(fields, self.member_field(member)),
            "method_definition" if self.name(member).as_deref() == Some("constructor") => {
                // `constructor(private readonly x: T)` declares a property.
                for field in self.parameter_properties(member) {
                    push_field(fields, Some(field));
                }
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let field = self.member_field(member);
                let is_method = field.as_ref().is_some_and(|f| f.kind == FieldKind::Method);
                let name = field.as_ref().map(|f| f.name.clone());
                push_field(fields, field);
                // A signature with an implementation in the same class is an overload.
                let overload = member.kind() != "method_definition"
                    && name.as_ref().is_some_and(|n| implemented.contains(n));
                if let (true, false, Some(name)) = (is_method, overload, name) {
                    self.function(name, member, scope, decl);
                }
            }
            _ => {}
        }
    }

    fn interface(&mut self, node: Node, scope: &[String], decl: Decl) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let decl = self.listed(decl, &name);
        let mut extends = Vec::new();
        let mut cursor = node.walk();
        for clause in node
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "extends_type_clause")
        {
            let mut types = clause.walk();
            extends.extend(clause.named_children(&mut types).map(|t| self.text(t)));
        }
        let fields = node
            .child_by_field_name("body")
            .map(|b| self.object_members(b))
            .unwrap_or_default();
        self.push_type(
            name,
            TypeKind::Interface,
            fields,
            extends,
            node,
            scope,
            decl,
        );
    }

    fn type_alias(&mut self, node: Node, scope: &[String], decl: Decl) {
        let (Some(name), Some(value)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("value").map(unparen),
        ) else {
            return;
        };
        let name = self.text(name);
        let decl = self.listed(decl, &name);
        let (kind, fields, extends) = match value.kind() {
            "object_type" => (TypeKind::Type, self.object_members(value), Vec::new()),
            "union_type" => {
                let mut leaves = Vec::new();
                flatten(value, "union_type", &mut leaves);
                // `'a' | 'b' | undefined`: nullish leaves mark optionality, not members.
                leaves.retain(|l| !matches!(self.text(*l).as_str(), "null" | "undefined"));
                if !leaves.is_empty() && leaves.iter().all(|l| l.kind() == "literal_type") {
                    let members = leaves
                        .iter()
                        .map(|l| member(unquote(&self.text(*l))))
                        .collect();
                    (TypeKind::Union, members, Vec::new())
                } else {
                    (TypeKind::Alias, Vec::new(), Vec::new())
                }
            }
            "intersection_type" => {
                // `A & { x: T }`: object parts are fields, named parts are bases.
                let mut parts = Vec::new();
                flatten(value, "intersection_type", &mut parts);
                let mut fields = Vec::new();
                let mut extends = Vec::new();
                for part in parts {
                    if part.kind() == "object_type" {
                        fields.extend(self.object_members(part));
                    } else {
                        extends.push(self.text(part));
                    }
                }
                let kind = if fields.is_empty() {
                    TypeKind::Alias
                } else {
                    TypeKind::Type
                };
                (kind, fields, extends)
            }
            _ => (TypeKind::Alias, Vec::new(), Vec::new()),
        };
        self.push_type(name, kind, fields, extends, node, scope, decl);
    }

    fn enumeration(&mut self, node: Node, scope: &[String], decl: Decl) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let decl = self.listed(decl, &name);
        let mut fields = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for entry in body.named_children(&mut cursor) {
                let name_node = match entry.kind() {
                    "enum_assignment" => entry.child_by_field_name("name"),
                    "comment" => None,
                    _ => Some(entry),
                };
                if let Some(n) = name_node {
                    fields.push(member(unquote(&self.text(n))));
                }
            }
        }
        self.push_type(name, TypeKind::Enum, fields, Vec::new(), node, scope, decl);
    }

    #[allow(clippy::too_many_arguments)]
    fn push_type(
        &mut self,
        name: String,
        kind: TypeKind,
        fields: Vec<Field>,
        extends: Vec<String>,
        node: Node,
        scope: &[String],
        decl: Decl,
    ) {
        self.records.push(Record::Type(TypeRecord {
            language: self.language,
            name,
            kind,
            scope: join_scope(scope),
            fields,
            extends,
            exported: decl.exported,
            location: self.location(decl.anchor, node),
            doc: self.doc(decl.anchor),
        }));
    }

    /// Fields of an interface body or object type literal, one per name.
    fn object_members(&self, body: Node) -> Vec<Field> {
        let mut fields = Vec::new();
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            push_field(&mut fields, self.member_field(member));
        }
        fields
    }

    /// A property, accessor or method member of a class, interface or object type.
    fn member_field(&self, member: Node) -> Option<Field> {
        let name = self.name(member)?;
        let optional = has_token(member, "?");
        match member.kind() {
            "property_signature" | "public_field_definition" => Some(Field {
                name,
                ty: member
                    .child_by_field_name("type")
                    .map(|t| self.annotation(t)),
                optional,
                kind: FieldKind::Property,
            }),
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                if has_token(member, "get") || has_token(member, "set") {
                    // Accessors are properties: the getter's return or the setter's parameter type.
                    let ty = self
                        .returns(member)
                        .or_else(|| self.params(member).into_iter().next().and_then(|p| p.ty));
                    return Some(Field {
                        name,
                        ty,
                        optional,
                        kind: FieldKind::Property,
                    });
                }
                let params = self.params(member);
                let returns = self.returns(member);
                Some(Field {
                    name,
                    ty: Some(signature(&params, returns.as_deref())),
                    optional,
                    kind: FieldKind::Method,
                })
            }
            _ => None,
        }
    }

    fn params(&self, function: Node) -> Vec<Param> {
        // `x => x` has a bare `parameter` instead of a `parameters` list.
        if let Some(single) = function.child_by_field_name("parameter") {
            return vec![Param {
                name: self.text(single),
                ty: None,
                optional: false,
            }];
        }
        let Some(list) = function.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .filter_map(|p| match p.kind() {
                "required_parameter" | "optional_parameter" => {
                    let pattern = p.child_by_field_name("pattern");
                    // `this: Window` types the receiver; it is not an argument.
                    if pattern.is_some_and(|n| n.kind() == "this") {
                        return None;
                    }
                    Some(Param {
                        name: pattern.map_or_else(|| self.text(p), |n| self.text(n)),
                        ty: p.child_by_field_name("type").map(|t| self.annotation(t)),
                        optional: p.kind() == "optional_parameter"
                            || p.child_by_field_name("value").is_some(),
                    })
                }
                _ => Some(Param {
                    name: self.text(p),
                    ty: None,
                    optional: false,
                }),
            })
            .collect()
    }

    fn parameter_properties(&self, constructor: Node) -> Vec<Field> {
        let Some(list) = constructor.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|p| {
                let mut c = p.walk();
                p.children(&mut c)
                    .any(|m| matches!(m.kind(), "accessibility_modifier" | "readonly"))
            })
            .filter_map(|p| {
                Some(Field {
                    name: self.text(p.child_by_field_name("pattern")?),
                    ty: p.child_by_field_name("type").map(|t| self.annotation(t)),
                    optional: p.kind() == "optional_parameter",
                    kind: FieldKind::Property,
                })
            })
            .collect()
    }

    fn returns(&self, node: Node) -> Option<String> {
        node.child_by_field_name("return_type")
            .map(|n| self.annotation(n))
    }

    /// A member's name, with quotes removed so `'id'` and `id` match.
    fn name(&self, member: Node) -> Option<String> {
        let name = member.child_by_field_name("name")?;
        Some(unquote(&self.text(name)).to_string())
    }

    /// `: string` → `string`; also handles `asserts x` and `x is T` annotations.
    fn annotation(&self, node: Node) -> String {
        self.text(node).trim_start_matches(':').trim().to_string()
    }

    /// `decl` marked exported when a later `export { name }` lists it.
    fn listed<'t>(&self, decl: Decl<'t>, name: &str) -> Decl<'t> {
        let listed = self
            .listed_exports
            .last()
            .is_some_and(|set| set.contains(name));
        Decl {
            exported: decl.exported || listed,
            ..decl
        }
    }

    /// Names a block exports by list (`export { a, b as c }`) or `export default a`.
    fn export_lists(&self, block: Node) -> HashSet<String> {
        let mut names = HashSet::new();
        let mut cursor = block.walk();
        for statement in block
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "export_statement" && n.child_by_field_name("source").is_none())
        {
            if let Some(value) = statement
                .child_by_field_name("value")
                .filter(|v| v.kind() == "identifier")
            {
                names.insert(self.text(value));
            }
            let mut inner = statement.walk();
            for clause in statement
                .named_children(&mut inner)
                .filter(|n| n.kind() == "export_clause")
            {
                let mut specifiers = clause.walk();
                for specifier in clause.named_children(&mut specifiers) {
                    if let Some(name) = specifier.child_by_field_name("name") {
                        names.insert(self.text(name));
                    }
                }
            }
        }
        names
    }

    /// The `/** … */` comment above `anchor`, skipping adjacent `//` comments, as plain text.
    fn doc(&self, anchor: Node) -> Option<String> {
        let mut below = anchor;
        loop {
            let comment = below
                .prev_named_sibling()
                .filter(|c| c.kind() == "comment")?;
            if comment.end_position().row + 1 < below.start_position().row {
                return None;
            }
            let raw = &self.source[comment.byte_range()];
            if let Some(body) = raw.strip_prefix("/**").and_then(|r| r.strip_suffix("*/")) {
                let text = body
                    .lines()
                    .map(|l| l.trim().trim_start_matches('*').trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                return (!text.is_empty()).then_some(text);
            }
            if !raw.starts_with("//") {
                return None;
            }
            below = comment;
        }
    }

    fn location(&self, anchor: Node, node: Node) -> Location {
        Location {
            file: self.rel.to_string(),
            start_line: anchor.start_position().row + 1,
            end_line: node.end_position().row + 1,
        }
    }

    /// Node text on one line: whitespace runs collapse, except inside string literals.
    fn text(&self, node: Node) -> String {
        collapse_whitespace(&self.source[node.byte_range()])
    }
}

/// Names of functions implemented in a block, so their overload signatures can be skipped.
fn implemented_functions(block: Node, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        let node = if child.kind() == "export_statement" {
            child.child_by_field_name("declaration")
        } else {
            Some(child)
        };
        if let Some(name) = node
            .filter(|n| {
                matches!(
                    n.kind(),
                    "function_declaration" | "generator_function_declaration"
                )
            })
            .and_then(|n| n.child_by_field_name("name"))
        {
            names.insert(source[name.byte_range()].to_string());
        }
    }
    names
}

/// Names of methods with a body in a class, so their overload signatures can be skipped.
fn implemented_methods(body: Node, source: &str) -> HashSet<String> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|m| m.kind() == "method_definition")
        .filter_map(|m| m.child_by_field_name("name"))
        .map(|n| unquote(&source[n.byte_range()]).to_string())
        .collect()
}

/// The first of the decorators directly before a class member, or the member itself.
fn leading_decorators(member: Node) -> Node {
    let mut first = member;
    while let Some(prev) = first
        .prev_named_sibling()
        .filter(|p| p.kind() == "decorator")
    {
        first = prev;
    }
    first
}

/// `private`, `protected` or `#name` class members are not part of an exported API.
fn is_private(member: Node) -> bool {
    if member
        .child_by_field_name("name")
        .is_some_and(|n| n.kind() == "private_property_identifier")
    {
        return true;
    }
    let mut cursor = member.walk();
    member.children(&mut cursor).any(|c| {
        c.kind() == "accessibility_modifier" && {
            let mut inner = c.walk();
            c.children(&mut inner)
                .any(|k| matches!(k.kind(), "private" | "protected"))
        }
    })
}

/// Collect the leaves of a left-nested `A | B | C` (or `&`) chain, unwrapping parentheses.
fn flatten<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let child = unparen(child);
        if child.kind() == kind {
            flatten(child, kind, out);
        } else if child.kind() != "comment" {
            out.push(child);
        }
    }
}

/// `(T)` → `T`, for both types and expressions.
fn unparen(mut node: Node) -> Node {
    while matches!(
        node.kind(),
        "parenthesized_type" | "parenthesized_expression"
    ) {
        let mut cursor = node.walk();
        let Some(inner) = node
            .named_children(&mut cursor)
            .find(|n| n.kind() != "comment")
        else {
            break;
        };
        node = inner;
    }
    node
}

pub(crate) fn member(name: &str) -> Field {
    Field {
        name: name.to_string(),
        ty: None,
        optional: false,
        kind: FieldKind::Member,
    }
}

fn unquote(text: &str) -> &str {
    text.trim_matches(|c| c == '\'' || c == '"' || c == '`')
}

fn qualified(scope: &[String], name: &str) -> String {
    match join_scope(scope) {
        Some(scope) => format!("{scope}.{name}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str) -> Vec<Record> {
        extract(&mut Parser::new(), source, "src/a.ts", Language::TypeScript)
    }

    fn types(records: &[Record]) -> Vec<&TypeRecord> {
        records
            .iter()
            .filter_map(|r| {
                if let Record::Type(t) = r {
                    Some(t)
                } else {
                    None
                }
            })
            .collect()
    }

    fn functions(records: &[Record]) -> Vec<&FunctionRecord> {
        records
            .iter()
            .filter_map(|r| {
                if let Record::Function(f) = r {
                    Some(f)
                } else {
                    None
                }
            })
            .collect()
    }

    fn field_names(t: &TypeRecord) -> Vec<&str> {
        t.fields.iter().map(|f| f.name.as_str()).collect()
    }

    fn function_names(records: &[Record]) -> Vec<&str> {
        functions(records).iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn interface_fields_extends_and_doc() {
        let records = run(
            "/** A user.\n * Second line. */\nexport interface User extends Base, Named<T> {\n  id: string;\n  email?: string;\n  rename(name: string): void;\n  [key: string]: unknown;\n}",
        );
        let [user] = types(&records)[..] else {
            panic!("{records:?}")
        };
        assert_eq!(user.kind, TypeKind::Interface);
        assert!(user.exported);
        assert_eq!(user.extends, vec!["Base", "Named<T>"]);
        assert_eq!(field_names(user), vec!["id", "email", "rename"]);
        assert!(user.fields[1].optional);
        assert_eq!(user.fields[2].ty.as_deref(), Some("(name: string) => void"));
        assert_eq!(user.doc.as_deref(), Some("A user. Second line."));
        assert_eq!((user.location.start_line, user.location.end_line), (3, 8));
    }

    #[test]
    fn type_alias_shapes() {
        let records = run(
            "type Obj = { a: number; b?: string };\ntype Status = 'Pending' | 'Running' | \"Done\";\ntype Mixed = Base & { extra: boolean };\ntype Id = string | number;",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Type, vec!["a", "b"])
        );
        assert_eq!(
            (t[1].kind, field_names(t[1])),
            (TypeKind::Union, vec!["Pending", "Running", "Done"])
        );
        assert_eq!(
            (t[2].kind, field_names(t[2])),
            (TypeKind::Type, vec!["extra"])
        );
        assert_eq!(t[2].extends, vec!["Base"]);
        assert_eq!((t[3].kind, t[3].fields.len()), (TypeKind::Alias, 0));
        assert!(!t[0].exported);
    }

    #[test]
    fn unions_ignore_comments_parens_and_nullish_leaves() {
        let records = run(
            "type A = 'a' | /* legacy */ 'b' | undefined;\ntype B = ({ id: string });\ntype C = Base & ({ x: number });\ntype D = 'x  y' | 'x y';\ntype E = null | undefined;",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Union, vec!["a", "b"])
        );
        assert_eq!((t[1].kind, field_names(t[1])), (TypeKind::Type, vec!["id"]));
        assert_eq!(
            (field_names(t[2]), t[2].extends.clone()),
            (vec!["x"], vec!["Base".to_string()])
        );
        assert_eq!(field_names(t[3]), vec!["x  y", "x y"]);
        assert_eq!(t[4].kind, TypeKind::Alias);
    }

    #[test]
    fn enum_members() {
        let records = run("export enum Color { Red, Green = 'g', \"Blue\" = 3 }");
        let [color] = types(&records)[..] else {
            panic!()
        };
        assert_eq!(
            (color.kind, field_names(color)),
            (TypeKind::Enum, vec!["Red", "Green", "Blue"])
        );
    }

    #[test]
    fn class_members_and_methods() {
        let records = run(
            "export class Repo extends Store<Item> implements Disposable {\n  count = 0;\n  label?: string;\n  constructor(private readonly db: Db, plain: number) { super(); }\n  get size(): number { return 0; }\n  load(id: string, force = false): Promise<Item> { return x; }\n  private secret(): void {}\n  #hidden() {}\n}",
        );
        let [repo] = types(&records)[..] else {
            panic!()
        };
        assert_eq!(repo.extends, vec!["Store<Item>", "Disposable"]);
        assert_eq!(
            field_names(repo),
            vec!["count", "label", "db", "size", "load", "secret", "#hidden"]
        );
        assert!(repo.fields[1].optional);
        let f = functions(&records);
        assert_eq!(function_names(&records), vec!["load", "secret", "#hidden"]);
        assert_eq!((f[0].scope.as_deref(), f[0].exported), (Some("Repo"), true));
        assert_eq!(f[0].returns.as_deref(), Some("Promise<Item>"));
        assert!(f[0].params[1].optional);
        assert!(!f[1].exported && !f[2].exported);
    }

    #[test]
    fn accessors_merge_regardless_of_order() {
        let records = run(
            "class C { set v(n: number) {} get v(): number { return 0; } set w(s: string) {} }\ninterface I { get v(): number; set v(x: number); }",
        );
        let t = types(&records);
        let class_fields: Vec<_> = t[0]
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.ty.as_deref(), f.kind))
            .collect();
        assert_eq!(
            class_fields,
            vec![
                ("v", Some("number"), FieldKind::Property),
                ("w", Some("string"), FieldKind::Property)
            ]
        );
        assert_eq!(
            (t[1].fields.len(), t[1].fields[0].kind),
            (1, FieldKind::Property)
        );
        assert!(functions(&records).is_empty());
    }

    #[test]
    fn quoted_property_names_match_bare_ones() {
        let records =
            run("interface A { 'retry-after'?: string; \"id\": number }\nclass B { 'x' = 1 }");
        let t = types(&records);
        assert_eq!(field_names(t[0]), vec!["retry-after", "id"]);
        assert_eq!(field_names(t[1]), vec!["x"]);
    }

    #[test]
    fn overloads_collapse_into_the_implementation() {
        let records = run(
            "/** Formats. */\nexport function f(x: string): string;\nexport function f(x: number): string;\nexport function f(x: any) { return x; }\nclass K { load(a: string): void; load(a: any) {} }\ninterface I { get(a: string): A; get(a: number): B }\ndeclare abstract class D { run(x: string): void; abstract stop(): void; }",
        );
        let f = functions(&records);
        assert_eq!(function_names(&records), vec!["f", "load", "run", "stop"]);
        assert_eq!(
            (f[0].params[0].ty.as_deref(), f[0].doc.as_deref()),
            (Some("any"), Some("Formats."))
        );
        let t = types(&records);
        assert_eq!(field_names(t[0]), vec!["load"]);
        assert_eq!(field_names(t[1]), vec!["get"]);
    }

    #[test]
    fn decorated_members_keep_doc_and_start_line() {
        let records = run(
            "class C {\n  /** Handles. */\n  @HostListener('x')\n  @Other()\n  on(): void {}\n}",
        );
        let [on] = functions(&records)[..] else {
            panic!()
        };
        assert_eq!(
            (on.doc.as_deref(), on.location.start_line),
            (Some("Handles."), 3)
        );
    }

    #[test]
    fn functions_and_arrow_consts() {
        let records = run(
            "/** Adds. */\nexport function add(a: number, b?: number): number { return a; }\nconst twice = (x: number) => x * 2;\nexport const id = x => x;\nexport default function () {}\nfunction inner() { function nested() {} }\nconst wrapped = ((s: string) => s);\nconst gen = function* () {};\nfunction recv(this: Window, x: number) {}",
        );
        let f = functions(&records);
        assert_eq!(
            function_names(&records),
            vec![
                "add", "twice", "id", "default", "inner", "wrapped", "gen", "recv"
            ]
        );
        assert_eq!(f[0].doc.as_deref(), Some("Adds."));
        assert_eq!(f[0].params[1].ty.as_deref(), Some("number"));
        assert!(f[0].exported && !f[1].exported && f[2].exported);
        assert_eq!(f[2].params[0].name, "x");
        assert_eq!(f[7].params.len(), 1);
    }

    #[test]
    fn export_lists_mark_declarations_exported() {
        let records = run(
            "const a = () => 1;\nfunction b() {}\nclass Foo { m() {} }\ninterface Hidden { x: 1 }\nexport { a, b as bee };\nexport default Foo;\nexport { z } from './z';",
        );
        let exported: Vec<_> = records
            .iter()
            .map(|r| match r {
                Record::Function(f) => (f.name.as_str(), f.exported),
                Record::Type(t) => (t.name.as_str(), t.exported),
            })
            .collect();
        assert_eq!(
            exported,
            vec![
                ("a", true),
                ("b", true),
                ("m", true),
                ("Foo", true),
                ("Hidden", false)
            ]
        );
    }

    #[test]
    fn namespaces_and_declare_blocks() {
        let records = run(
            "export namespace Api { export interface Req { url: string; method: string } }\nnamespace Solo { export type S = { y: 1 } }\ndeclare function g(x: string): void;\ndeclare global { interface Window { custom: string } function ping(): void; }\ndeclare module 'm' { export const k: number; export function h(): void; }",
        );
        let t = types(&records);
        let scoped: Vec<_> = t
            .iter()
            .map(|t| (t.name.as_str(), t.scope.as_deref()))
            .collect();
        assert_eq!(
            scoped,
            vec![("Req", Some("Api")), ("S", Some("Solo")), ("Window", None)]
        );
        assert_eq!(function_names(&records), vec!["g", "ping", "h"]);
    }

    #[test]
    fn tsx_and_javascript_parse() {
        let tsx = extract(
            &mut Parser::new(),
            "export const View = ({ id }: Props) => <div>{id}</div>;",
            "a.tsx",
            Language::TypeScript,
        );
        assert_eq!(functions(&tsx)[0].params[0].name, "{ id }");
        let js = extract(
            &mut Parser::new(),
            "export function f(a, b = 1) { return <A />; }",
            "a.jsx",
            Language::JavaScript,
        );
        assert_eq!(functions(&js)[0].params.len(), 2);
    }

    #[test]
    fn default_exports() {
        let records = run("export default class { x = 1 }");
        assert_eq!(
            (
                types(&records)[0].name.as_str(),
                types(&records)[0].exported
            ),
            ("default", true)
        );
    }

    #[test]
    fn doc_skips_line_comments_but_requires_adjacency() {
        let records = run(
            "/** Far away. */\n\n\nexport type A = { x: 1 };\n/** Kept. */\n// eslint-disable-next-line\nexport type B = { y: 1 };",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].doc.as_deref(), t[1].doc.as_deref()),
            (None, Some("Kept."))
        );
    }

    #[test]
    fn collapses_whitespace_outside_quotes() {
        assert_eq!(collapse_whitespace("{ a:\n   'x  y' }"), "{ a: 'x  y' }");
        assert_eq!(collapse_whitespace("\"a\\\"  b\"   c"), "\"a\\\"  b\" c");
    }
}
