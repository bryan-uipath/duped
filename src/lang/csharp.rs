//! C# extractor over the tree-sitter C# grammar.

use tree_sitter::{Node, Parser};

use super::{collapse_whitespace, push_field};
use crate::record::{
    Field, FieldKind, FunctionRecord, Language, Location, Param, Record, TypeKind, TypeRecord,
    format_params,
};

/// Parse one file and return its types (including nested ones) and their methods.
pub fn extract(parser: &mut Parser, source: &str, rel: &str) -> Vec<Record> {
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .expect("bundled grammar is compatible");
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut extractor = Extractor {
        source,
        rel,
        records: Vec::new(),
    };
    extractor.declarations(tree.root_node(), &[], None);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    records: Vec<Record>,
}

/// The type a member is declared in.
#[derive(Clone, Copy)]
struct Owner {
    exported: bool,
    /// Interface members are public unless marked otherwise.
    interface: bool,
}

impl Extractor<'_> {
    /// Types in a compilation unit, namespace body or type body.
    fn declarations(&mut self, container: Node, scope: &[String], owner: Option<Owner>) {
        let mut cursor = container.walk();
        for child in container.named_children(&mut cursor) {
            match child.kind() {
                "namespace_declaration" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        self.declarations(body, scope, owner);
                    }
                }
                // `#if DEBUG … #endif` around declarations.
                kind if kind.starts_with("preproc_") => self.declarations(child, scope, owner),
                _ => self.type_declaration(child, scope, owner),
            }
        }
    }

    fn type_declaration(&mut self, node: Node, scope: &[String], owner: Option<Owner>) {
        let kind = match node.kind() {
            "class_declaration" => TypeKind::Class,
            "struct_declaration" => TypeKind::Struct,
            "interface_declaration" => TypeKind::Interface,
            "record_declaration" => TypeKind::Record,
            "enum_declaration" => TypeKind::Enum,
            "delegate_declaration" => TypeKind::Alias,
            _ => return,
        };
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let exported = is_public(node, owner);
        let mut member_scope = scope.to_vec();
        member_scope.push(name.clone());
        let me = Owner {
            exported,
            interface: kind == TypeKind::Interface,
        };

        let mut fields = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                // `record Person(string Name, int Age)`: positional parameters are properties.
                // A class or struct primary constructor only captures them, so it adds none.
                "parameter_list" if kind == TypeKind::Record => {
                    for param in self.params(child) {
                        push_field(
                            &mut fields,
                            Some(Field {
                                name: param.name,
                                ty: param.ty,
                                optional: false,
                                kind: FieldKind::Property,
                            }),
                        );
                    }
                }
                "enum_member_declaration_list" => {
                    let mut members = child.walk();
                    for member in child
                        .named_children(&mut members)
                        .filter(|m| m.kind() == "enum_member_declaration")
                    {
                        if let Some(n) = member.child_by_field_name("name") {
                            fields.push(Field {
                                name: self.text(n),
                                ty: None,
                                optional: false,
                                kind: FieldKind::Member,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.members(body, &member_scope, me, &mut fields);
        }

        self.records.push(Record::Type(TypeRecord {
            language: Language::CSharp,
            name,
            kind,
            scope: join_scope(scope),
            fields,
            extends: self.bases(node),
            exported,
            location: self.location(node),
            doc: self.doc(node),
        }));
    }

    fn members(&mut self, body: Node, scope: &[String], owner: Owner, fields: &mut Vec<Field>) {
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "property_declaration" | "event_declaration" => {
                    if let Some(name) = member.child_by_field_name("name") {
                        push_field(
                            fields,
                            Some(Field {
                                name: self.text(name),
                                ty: member.child_by_field_name("type").map(|t| self.text(t)),
                                optional: false,
                                kind: FieldKind::Property,
                            }),
                        );
                    }
                }
                // `int a, b = 1;` declares one field per declarator.
                "field_declaration" | "event_field_declaration" => {
                    let mut inner = member.walk();
                    let Some(declaration) = member
                        .named_children(&mut inner)
                        .find(|n| n.kind() == "variable_declaration")
                    else {
                        continue;
                    };
                    let ty = declaration
                        .child_by_field_name("type")
                        .map(|t| self.text(t));
                    let mut declarators = declaration.walk();
                    for declarator in declaration
                        .named_children(&mut declarators)
                        .filter(|n| n.kind() == "variable_declarator")
                    {
                        if let Some(name) = declarator.child_by_field_name("name") {
                            push_field(
                                fields,
                                Some(Field {
                                    name: self.text(name),
                                    ty: ty.clone(),
                                    optional: false,
                                    kind: FieldKind::Property,
                                }),
                            );
                        }
                    }
                }
                "method_declaration" => self.method(member, scope, owner, fields),
                kind if kind.starts_with("preproc_") => self.members(member, scope, owner, fields),
                _ => self.type_declaration(member, scope, Some(owner)),
            }
        }
    }

    fn method(&mut self, member: Node, scope: &[String], owner: Owner, fields: &mut Vec<Field>) {
        let Some(name) = member.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let params = member
            .child_by_field_name("parameters")
            .map(|p| self.params(p))
            .unwrap_or_default();
        let returns = member.child_by_field_name("returns").map(|t| self.text(t));
        push_field(
            fields,
            Some(Field {
                name: name.clone(),
                ty: Some(format!(
                    "({}) => {}",
                    format_params(&params),
                    returns.as_deref().unwrap_or("void")
                )),
                optional: false,
                kind: FieldKind::Method,
            }),
        );
        // Abstract and interface methods without a body are signatures only.
        if member.child_by_field_name("body").is_none() {
            return;
        }
        self.records.push(Record::Function(FunctionRecord {
            language: Language::CSharp,
            name,
            scope: join_scope(scope),
            params,
            returns,
            exported: owner.exported && is_public(member, Some(owner)),
            location: self.location(member),
            doc: self.doc(member),
        }));
    }

    /// Parameters, with modifiers kept in the type: `this string s`, `out int n`, `params int[] rest`.
    fn params(&self, list: Node) -> Vec<Param> {
        let mut params = Vec::new();
        let mut cursor = list.walk();
        for param in list
            .named_children(&mut cursor)
            .filter(|p| p.kind() == "parameter")
        {
            let Some(name) = param.child_by_field_name("name") else {
                continue;
            };
            let mut inner = param.walk();
            let modifiers: Vec<String> = param
                .named_children(&mut inner)
                .filter(|c| c.kind() == "modifier")
                .map(|m| self.text(m))
                .collect();
            let ty = param.child_by_field_name("type").map(|t| {
                let mut parts = modifiers.clone();
                parts.push(self.text(t));
                parts.join(" ")
            });
            params.push(Param {
                name: self.text(name),
                ty,
                optional: has_token(param, "="),
            });
        }
        // A `params T[] rest` array is stored on the list itself, not as a `parameter`.
        if let (Some(name), Some(ty)) = (
            list.child_by_field_name("name"),
            list.child_by_field_name("type"),
        ) {
            params.push(Param {
                name: self.text(name),
                ty: Some(format!("params {}", self.text(ty))),
                optional: false,
            });
        }
        params
    }

    /// `class A : Base(x), IFoo` → `["Base", "IFoo"]`.
    fn bases(&self, node: Node) -> Vec<String> {
        let mut cursor = node.walk();
        let Some(list) = node
            .named_children(&mut cursor)
            .find(|n| n.kind() == "base_list")
        else {
            return Vec::new();
        };
        let mut entries = list.walk();
        list.named_children(&mut entries)
            .filter(|n| n.kind() != "argument_list" && n.kind() != "comment")
            .map(|n| {
                let text = self.text(n);
                match text.find('(') {
                    Some(i) if n.kind() == "primary_constructor_base_type" => {
                        text[..i].trim().to_string()
                    }
                    _ => text,
                }
            })
            .collect()
    }

    /// Consecutive `///` comments directly above `node`, with XML tags stripped.
    fn doc(&self, node: Node) -> Option<String> {
        let mut lines = Vec::new();
        let mut below = node;
        while let Some(comment) = below.prev_named_sibling().filter(|c| c.kind() == "comment") {
            if comment.end_position().row + 1 < below.start_position().row {
                break;
            }
            let Some(line) = self.source[comment.byte_range()].strip_prefix("///") else {
                break;
            };
            lines.push(line);
            below = comment;
        }
        lines.reverse();
        let text = collapse_whitespace(&strip_xml(&lines.join(" ")));
        (!text.is_empty()).then_some(text)
    }

    fn location(&self, node: Node) -> Location {
        Location {
            file: self.rel.to_string(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        }
    }

    fn text(&self, node: Node) -> String {
        collapse_whitespace(&self.source[node.byte_range()])
    }
}

/// `public`, or an interface member with no access modifier; nested types also need
/// an exported owner. Explicit interface implementations are public through the interface.
fn is_public(node: Node, owner: Option<Owner>) -> bool {
    if owner.is_some_and(|o| !o.exported) {
        return false;
    }
    let mut cursor = node.walk();
    let mut access = Vec::new();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "explicit_interface_specifier" => return true,
            "modifier" => access.extend(
                child
                    .child(0)
                    .map(|k| k.kind())
                    .filter(|k| matches!(*k, "public" | "private" | "protected" | "internal")),
            ),
            _ => {}
        }
    }
    access.contains(&"public") || (access.is_empty() && owner.is_some_and(|o| o.interface))
}

fn has_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token)
}

/// `<summary>Loads <see cref="Item"/>.</summary>` → `Loads Item.`
fn strip_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = &rest[open + 1..open + close];
        // Self-closing references keep their target: `<see cref="T:Item"/>` → `Item`.
        if tag.ends_with('/')
            && let Some(value) = ["cref", "name", "langword", "href"]
                .iter()
                .find_map(|attr| attribute(tag, attr))
        {
            out.push_str(value.rsplit(':').next().unwrap_or(value));
        }
        out.push(' ');
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

fn attribute<'t>(tag: &'t str, name: &str) -> Option<&'t str> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

fn join_scope(scope: &[String]) -> Option<String> {
    (!scope.is_empty()).then(|| scope.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str) -> Vec<Record> {
        extract(&mut Parser::new(), source, "src/A.cs")
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

    fn find<'r>(records: &'r [Record], name: &str) -> &'r TypeRecord {
        types(records)
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no type {name}"))
    }

    #[test]
    fn type_kinds() {
        let records = run(
            "public class C {}\npublic struct S { public int X; }\npublic interface I {}\npublic record R(int A);\npublic record struct P(int X, int Y);\npublic enum E { A, B = 2 }\npublic delegate void D(object s);",
        );
        let kinds: Vec<_> = types(&records)
            .iter()
            .map(|t| (t.name.as_str(), t.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("C", TypeKind::Class),
                ("S", TypeKind::Struct),
                ("I", TypeKind::Interface),
                ("R", TypeKind::Record),
                ("P", TypeKind::Record),
                ("E", TypeKind::Enum),
                ("D", TypeKind::Alias),
            ]
        );
        assert_eq!(field_names(find(&records, "E")), vec!["A", "B"]);
        assert_eq!(field_names(find(&records, "P")), vec!["X", "Y"]);
        assert!(find(&records, "D").fields.is_empty());
    }

    #[test]
    fn members_fields_properties_events_and_methods() {
        let records = run(
            "public class Repo<T> : Base, IDisposable where T : class {\n  public int Count { get; set; }\n  private string? _name, _other = \"x\";\n  public event EventHandler Changed;\n  public event EventHandler Custom { add {} remove {} }\n  public Repo(int x) {}\n  public T Load(int id, bool force = false) => default;\n  public T Load(string key) => default;\n  protected abstract void Reset();\n  public static Repo operator +(Repo a, Repo b) => a;\n  ~Repo() {}\n}",
        );
        let repo = find(&records, "Repo");
        assert_eq!(repo.extends, vec!["Base", "IDisposable"]);
        assert_eq!(
            field_names(repo),
            vec![
                "Count", "_name", "_other", "Changed", "Custom", "Load", "Reset"
            ]
        );
        assert_eq!(repo.fields[1].ty.as_deref(), Some("string?"));
        assert_eq!(
            repo.fields[5].ty.as_deref(),
            Some("(id: int, force?: bool) => T")
        );
        let f = functions(&records);
        assert_eq!(
            f.len(),
            2,
            "both Load overloads have bodies; Reset is abstract"
        );
        assert_eq!(
            (f[0].scope.as_deref(), f[0].returns.as_deref()),
            (Some("Repo"), Some("T"))
        );
        assert!(f[0].params[1].optional);
    }

    #[test]
    fn positional_records_and_generic_methods() {
        let records = run(
            "public record Person(string Name, int Age) : Entity(Name) { public string Email { get; init; } }\npublic static class Ext { public static TOut Map<TIn, TOut>(this TIn x, Func<TIn, TOut> f, params int[] rest) => f(x); }",
        );
        let person = find(&records, "Person");
        assert_eq!(field_names(person), vec!["Name", "Age", "Email"]);
        assert_eq!(person.extends, vec!["Entity"]);
        let [map] = functions(&records)[..] else {
            panic!()
        };
        let params: Vec<_> = map
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.ty.as_deref()))
            .collect();
        assert_eq!(
            params,
            vec![
                ("x", Some("this TIn")),
                ("f", Some("Func<TIn, TOut>")),
                ("rest", Some("params int[]"))
            ]
        );
    }

    #[test]
    fn nested_types_and_both_namespace_forms() {
        let block = run(
            "namespace A.B { public class Outer { public class Inner { public void Go() {} } } }",
        );
        let inner = find(&block, "Inner");
        assert_eq!(inner.scope.as_deref(), Some("Outer"));
        assert_eq!(functions(&block)[0].scope.as_deref(), Some("Outer.Inner"));
        let file_scoped = run("namespace A.B;\npublic class Top { }\npublic interface ITop { }");
        assert_eq!(types(&file_scoped).len(), 2);
        assert_eq!(find(&file_scoped, "Top").scope, None);
    }

    #[test]
    fn visibility() {
        let records = run(
            "public class Pub {\n  public void A() {}\n  void B() {}\n  internal void C() {}\n  public class Nested {}\n  private class Hidden {}\n  void IFoo.Explicit() {}\n}\nclass Internal { public void D() {} }\npublic interface IApi { void Run(); int Size { get; } void Def() {} private void Helper() {} }",
        );
        let exported: Vec<_> = functions(&records)
            .iter()
            .map(|f| (f.name.as_str(), f.exported))
            .collect();
        assert_eq!(
            exported,
            vec![
                ("A", true),
                ("B", false),
                ("C", false),
                ("Explicit", true),
                ("D", false),
                ("Def", true),
                ("Helper", false)
            ]
        );
        assert!(find(&records, "Nested").exported);
        assert!(!find(&records, "Hidden").exported);
        assert!(!find(&records, "Internal").exported);
        assert_eq!(
            field_names(find(&records, "IApi")),
            vec!["Run", "Size", "Def", "Helper"]
        );
    }

    #[test]
    fn doc_comments_and_attributes() {
        let records = run(
            "/// <summary>\n/// Loads <see cref=\"T:Acme.Item\"/> by id &amp; caches it.\n/// </summary>\n/// <param name=\"id\">The id.</param>\n[Serializable]\n[Obsolete]\npublic class Loader {\n  /// <summary>Runs.</summary>\n  [HttpGet]\n  public void Run() {}\n}\n// not a doc comment\npublic class Plain {}",
        );
        let loader = find(&records, "Loader");
        assert_eq!(
            loader.doc.as_deref(),
            Some("Loads Acme.Item by id & caches it. The id.")
        );
        assert_eq!(
            loader.location.start_line, 5,
            "starts at the first attribute"
        );
        assert_eq!(functions(&records)[0].doc.as_deref(), Some("Runs."));
        assert_eq!(find(&records, "Plain").doc, None);
    }

    #[test]
    fn partial_classes_are_separate_records() {
        let records = run(
            "public partial class Part { public int A { get; set; } }\npublic partial class Part { public int B { get; set; } }",
        );
        let parts: Vec<_> = types(&records).iter().map(|t| field_names(t)).collect();
        assert_eq!(parts, vec![vec!["A"], vec!["B"]]);
    }

    #[test]
    fn preprocessor_blocks_are_traversed() {
        let records = run(
            "#if DEBUG\npublic class Dbg { public void X() {} }\n#endif\npublic class After {}",
        );
        assert_eq!(types(&records).len(), 2);
        assert_eq!(functions(&records).len(), 1);
    }

    #[test]
    fn strips_xml_doc_tags() {
        assert_eq!(
            collapse_whitespace(&strip_xml(
                "<summary>Use <paramref name=\"x\"/> or <c>null</c>.</summary>"
            )),
            "Use x or null ."
        );
    }
}
