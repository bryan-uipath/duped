//! TypeScript / JavaScript extractor over the tree-sitter TypeScript and TSX grammars.

use tree_sitter::{Node, Parser};

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
    };
    extractor.statements(tree.root_node(), &[]);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    language: Language,
    records: Vec<Record>,
}

/// Where a declaration's doc comment and line span start: the `export` wrapper when present.
#[derive(Clone, Copy)]
struct Decl<'t> {
    anchor: Node<'t>,
    exported: bool,
}

impl<'a> Extractor<'a> {
    fn statements(&mut self, block: Node, scope: &[String]) {
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            self.statement(
                child,
                scope,
                Decl {
                    anchor: child,
                    exported: false,
                },
            );
        }
    }

    fn statement(&mut self, node: Node, scope: &[String], decl: Decl) {
        match node.kind() {
            "export_statement" => {
                let exported = Decl {
                    anchor: decl.anchor,
                    exported: true,
                };
                if let Some(inner) = node.child_by_field_name("declaration") {
                    self.statement(inner, scope, exported);
                } else if let Some(value) = node.child_by_field_name("value") {
                    // `export default function () {}` / `export default class {}`
                    match value.kind() {
                        "function_expression" | "arrow_function" => {
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
                    self.statement(child, scope, decl);
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
            // `function_signature` covers `declare function` and overload signatures.
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let name = node
                    .child_by_field_name("name")
                    .map_or_else(|| "default".to_string(), |n| self.text(n));
                self.function(name, node, scope, decl);
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = node.walk();
                for declarator in node
                    .named_children(&mut cursor)
                    .filter(|n| n.kind() == "variable_declarator")
                {
                    let (Some(name), Some(value)) = (
                        declarator.child_by_field_name("name"),
                        declarator.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    if matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "function"
                    ) {
                        self.function(self.text(name), value, scope, decl);
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
        let record = FunctionRecord {
            language: self.language,
            name,
            scope: join_scope(scope),
            params: self.params(node),
            returns: node
                .child_by_field_name("return_type")
                .map(|n| self.annotation(n)),
            exported: decl.exported,
            location: self.location(decl.anchor, node),
            doc: self.doc(decl.anchor),
        };
        self.records.push(Record::Function(record));
    }

    fn class(&mut self, node: Node, scope: &[String], decl: Decl) {
        let name = node
            .child_by_field_name("name")
            .map_or_else(|| "default".to_string(), |n| self.text(n));
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
            let mut members = body.walk();
            for member in body.named_children(&mut members) {
                self.class_member(member, &member_scope, &mut fields);
            }
        }

        self.records.push(Record::Type(TypeRecord {
            language: self.language,
            name,
            kind: TypeKind::Class,
            scope: join_scope(scope),
            fields,
            extends,
            exported: decl.exported,
            location: self.location(decl.anchor, node),
            doc: self.doc(decl.anchor),
        }));
    }

    fn class_member(&mut self, member: Node, scope: &[String], fields: &mut Vec<Field>) {
        match member.kind() {
            "public_field_definition" => {
                if let Some(name) = member.child_by_field_name("name") {
                    fields.push(Field {
                        name: self.text(name),
                        ty: member
                            .child_by_field_name("type")
                            .map(|t| self.annotation(t)),
                        optional: has_token(member, "?"),
                        kind: FieldKind::Property,
                    });
                }
            }
            "method_definition" => {
                let Some(name) = member.child_by_field_name("name").map(|n| self.text(n)) else {
                    return;
                };
                if name == "constructor" {
                    // `constructor(private readonly x: T)` declares a property.
                    fields.extend(self.parameter_properties(member));
                } else if has_token(member, "get") || has_token(member, "set") {
                    if !fields.iter().any(|f| f.name == name) {
                        let ty = member
                            .child_by_field_name("return_type")
                            .map(|t| self.annotation(t));
                        fields.push(Field {
                            name,
                            ty,
                            optional: false,
                            kind: FieldKind::Property,
                        });
                    }
                } else {
                    let params = self.params(member);
                    let returns = member
                        .child_by_field_name("return_type")
                        .map(|t| self.annotation(t));
                    fields.push(Field {
                        name: name.clone(),
                        ty: Some(signature(&params, returns.as_deref())),
                        optional: false,
                        kind: FieldKind::Method,
                    });
                    let anchor = Decl {
                        anchor: member,
                        exported: false,
                    };
                    self.function(name, member, scope, anchor);
                }
            }
            "abstract_method_signature" | "method_signature" => {
                fields.extend(self.member_field(member))
            }
            _ => {}
        }
    }

    fn interface(&mut self, node: Node, scope: &[String], decl: Decl) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
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
            node.child_by_field_name("value"),
        ) else {
            return;
        };
        let name = self.text(name);
        let (kind, fields, extends) = match value.kind() {
            "object_type" => (TypeKind::Type, self.object_members(value), Vec::new()),
            "union_type" => {
                let mut leaves = Vec::new();
                flatten(value, "union_type", &mut leaves);
                if leaves.iter().all(|l| l.kind() == "literal_type") {
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

    /// Fields of an interface body or object type literal.
    fn object_members(&self, body: Node) -> Vec<Field> {
        let mut cursor = body.walk();
        body.named_children(&mut cursor)
            .filter_map(|m| self.member_field(m))
            .collect()
    }

    fn member_field(&self, member: Node) -> Option<Field> {
        let name = self.text(member.child_by_field_name("name")?);
        match member.kind() {
            "property_signature" => Some(Field {
                name,
                ty: member
                    .child_by_field_name("type")
                    .map(|t| self.annotation(t)),
                optional: has_token(member, "?"),
                kind: FieldKind::Property,
            }),
            "method_signature" | "abstract_method_signature" => {
                let params = self.params(member);
                let returns = member
                    .child_by_field_name("return_type")
                    .map(|t| self.annotation(t));
                Some(Field {
                    name,
                    ty: Some(signature(&params, returns.as_deref())),
                    optional: has_token(member, "?"),
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
            .map(|p| match p.kind() {
                "required_parameter" | "optional_parameter" => Param {
                    name: p
                        .child_by_field_name("pattern")
                        .map_or_else(|| self.text(p), |n| self.text(n)),
                    ty: p.child_by_field_name("type").map(|t| self.annotation(t)),
                    optional: p.kind() == "optional_parameter"
                        || p.child_by_field_name("value").is_some(),
                },
                _ => Param {
                    name: self.text(p),
                    ty: None,
                    optional: false,
                },
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

    /// `: string` → `string`; also handles `asserts x` and `x is T` annotations.
    fn annotation(&self, node: Node) -> String {
        self.text(node).trim_start_matches(':').trim().to_string()
    }

    /// The `/** … */` comment directly above `anchor`, cleaned to plain text.
    fn doc(&self, anchor: Node) -> Option<String> {
        let comment = anchor
            .prev_named_sibling()
            .filter(|c| c.kind() == "comment")?;
        if comment.end_position().row + 1 < anchor.start_position().row {
            return None;
        }
        let raw = &self.source[comment.byte_range()];
        let body = raw.strip_prefix("/**")?.strip_suffix("*/")?;
        let text = body
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty()).then_some(text)
    }

    fn location(&self, anchor: Node, node: Node) -> Location {
        Location {
            file: self.rel.to_string(),
            start_line: anchor.start_position().row + 1,
            end_line: node.end_position().row + 1,
        }
    }

    /// Node text with whitespace runs collapsed, e.g. multi-line types on one line.
    fn text(&self, node: Node) -> String {
        self.source[node.byte_range()]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Collect the leaves of a left-nested `A | B | C` (or `&`) chain.
fn flatten<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            flatten(child, kind, out);
        } else {
            out.push(child);
        }
    }
}

fn has_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token)
}

/// Method field type, e.g. `(id: string, force?: boolean) => void`.
fn signature(params: &[Param], returns: Option<&str>) -> String {
    let params = params
        .iter()
        .map(|p| {
            let optional = if p.optional { "?" } else { "" };
            match &p.ty {
                Some(ty) => format!("{}{optional}: {ty}", p.name),
                None => format!("{}{optional}", p.name),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("({params}) => {}", returns.unwrap_or("unknown"))
}

fn member(name: &str) -> Field {
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

fn join_scope(scope: &[String]) -> Option<String> {
    (!scope.is_empty()).then(|| scope.join("."))
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
            "export class Repo extends Store<Item> implements Disposable {\n  count = 0;\n  label?: string;\n  constructor(private readonly db: Db, plain: number) { super(); }\n  get size(): number { return 0; }\n  load(id: string, force = false): Promise<Item> { return x; }\n}",
        );
        let [repo] = types(&records)[..] else {
            panic!()
        };
        assert_eq!(repo.extends, vec!["Store<Item>", "Disposable"]);
        assert_eq!(
            field_names(repo),
            vec!["count", "label", "db", "size", "load"]
        );
        let [load] = functions(&records)[..] else {
            panic!()
        };
        assert_eq!(
            (load.name.as_str(), load.scope.as_deref()),
            ("load", Some("Repo"))
        );
        assert_eq!(load.returns.as_deref(), Some("Promise<Item>"));
        assert!(load.params[1].optional);
    }

    #[test]
    fn functions_and_arrow_consts() {
        let records = run(
            "/** Adds. */\nexport function add(a: number, b?: number): number { return a; }\nconst twice = (x: number) => x * 2;\nexport const id = x => x;\nexport default function () {}\nfunction inner() { function nested() {} }",
        );
        let f = functions(&records);
        let names: Vec<_> = f.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["add", "twice", "id", "default", "inner"]);
        assert_eq!(f[0].doc.as_deref(), Some("Adds."));
        assert_eq!(f[0].params[1].ty.as_deref(), Some("number"));
        assert!(f[0].exported && !f[1].exported && f[2].exported);
        assert_eq!(f[2].params[0].name, "x");
    }

    #[test]
    fn namespaces_and_declare_blocks() {
        let records = run(
            "export namespace Api { export interface Req { url: string; method: string } }\ndeclare function g(x: string): void;",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].name.as_str(), t[0].scope.as_deref()),
            ("Req", Some("Api"))
        );
        assert_eq!(functions(&records)[0].name, "g");
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
    fn doc_requires_adjacency() {
        let records = run("/** Far away. */\n\n\nexport type A = { x: 1 };");
        assert_eq!(types(&records)[0].doc, None);
    }
}
