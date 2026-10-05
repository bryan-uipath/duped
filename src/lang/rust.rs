//! Rust extractor over the tree-sitter Rust grammar.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::record::{
    Field, FieldKind, FunctionRecord, Language, Location, Param, Record, TypeKind, TypeRecord,
    format_params,
};

/// Parse one file and return its functions, methods and type definitions.
pub fn extract(parser: &mut Parser, source: &str, rel: &str) -> Vec<Record> {
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("bundled grammar is compatible");
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut extractor = Extractor {
        source,
        rel,
        records: Vec::new(),
    };
    extractor.items(tree.root_node(), &[]);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    records: Vec<Record>,
}

/// Leading attributes and doc comments of an item.
struct Preamble<'t> {
    /// First attribute line, or the item itself when it has none.
    anchor: Node<'t>,
    doc: Option<String>,
    /// `#[cfg(test)]` or a `#[test]`-style attribute.
    is_test: bool,
}

impl<'a> Extractor<'a> {
    /// Items of a file, inline `mod`, or `extern` block.
    fn items(&mut self, list: Node, scope: &[String]) {
        let visibility = self.type_visibility(list);
        let mut cursor = list.walk();
        for item in list.named_children(&mut cursor) {
            let preamble = self.preamble(item);
            if preamble.is_test {
                continue;
            }
            self.item(item, scope, &preamble, &visibility);
        }
    }

    fn item(
        &mut self,
        item: Node,
        scope: &[String],
        pre: &Preamble,
        visibility: &HashMap<String, bool>,
    ) {
        match item.kind() {
            "function_item" | "function_signature_item" => {
                let exported = is_pub(item, self.source);
                self.function(item, scope, pre, exported);
            }
            "foreign_mod_item" => {
                if let Some(body) = item.child_by_field_name("body") {
                    self.items(body, scope);
                }
            }
            "mod_item" => {
                if let (Some(name), Some(body)) = (
                    item.child_by_field_name("name"),
                    item.child_by_field_name("body"),
                ) {
                    let mut inner = scope.to_vec();
                    inner.push(self.text(name));
                    self.items(body, &inner);
                }
            }
            "impl_item" => self.implementation(item, scope, visibility),
            "struct_item" | "union_item" => {
                let fields = item
                    .child_by_field_name("body")
                    .map(|b| self.struct_fields(b))
                    .unwrap_or_default();
                self.push_type(item, TypeKind::Struct, fields, Vec::new(), scope, pre);
            }
            "enum_item" => {
                let mut fields = Vec::new();
                if let Some(body) = item.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for variant in body
                        .named_children(&mut cursor)
                        .filter(|v| v.kind() == "enum_variant")
                    {
                        if let Some(name) = variant.child_by_field_name("name") {
                            fields.push(Field {
                                name: self.text(name),
                                ty: None,
                                optional: false,
                                kind: FieldKind::Member,
                            });
                        }
                    }
                }
                self.push_type(item, TypeKind::Enum, fields, Vec::new(), scope, pre);
            }
            "trait_item" => self.trait_item(item, scope, pre),
            "type_item" => {
                self.push_type(item, TypeKind::Alias, Vec::new(), Vec::new(), scope, pre)
            }
            _ => {}
        }
    }

    /// `impl Foo { … }` / `impl Trait for Foo { … }`: methods scoped to `Foo`.
    fn implementation(&mut self, item: Node, scope: &[String], visibility: &HashMap<String, bool>) {
        let (Some(ty), Some(body)) = (
            item.child_by_field_name("type"),
            item.child_by_field_name("body"),
        ) else {
            return;
        };
        let name = self.base_type_name(ty);
        // Only a type defined privately in this file hides its methods.
        let type_public = visibility.get(&name).copied().unwrap_or(true);
        let trait_impl = item.child_by_field_name("trait").is_some();
        let mut member_scope = scope.to_vec();
        member_scope.push(name);
        let mut cursor = body.walk();
        for member in body
            .named_children(&mut cursor)
            .filter(|m| m.kind() == "function_item")
        {
            let pre = self.preamble(member);
            if pre.is_test {
                continue;
            }
            // Trait methods take the trait's visibility, which has no `pub` of its own.
            let exported = type_public && (trait_impl || is_pub(member, self.source));
            self.function(member, &member_scope, &pre, exported);
        }
    }

    fn trait_item(&mut self, item: Node, scope: &[String], pre: &Preamble) {
        let Some(name) = item.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let exported = is_pub(item, self.source);
        let extends = item
            .child_by_field_name("bounds")
            .map(|bounds| {
                let mut cursor = bounds.walk();
                bounds
                    .named_children(&mut cursor)
                    .filter(|b| b.kind() != "lifetime")
                    .map(|b| self.text(b))
                    .collect()
            })
            .unwrap_or_default();

        let mut member_scope = scope.to_vec();
        member_scope.push(name);
        let mut fields = Vec::new();
        if let Some(body) = item.child_by_field_name("body") {
            let mut cursor = body.walk();
            for member in body.named_children(&mut cursor) {
                match member.kind() {
                    "function_item" | "function_signature_item" => {
                        let Some(method) = member.child_by_field_name("name") else {
                            continue;
                        };
                        let params = self.params(member);
                        let returns = self.returns(member);
                        fields.push(Field {
                            name: self.text(method),
                            ty: Some(signature(&params, returns.as_deref())),
                            optional: false,
                            kind: FieldKind::Method,
                        });
                        // Only a default body is a function; a bare signature is a requirement.
                        if member.kind() == "function_item" {
                            let member_pre = self.preamble(member);
                            self.function(member, &member_scope, &member_pre, exported);
                        }
                    }
                    "associated_type" | "const_item" => {
                        if let Some(field_name) = member.child_by_field_name("name") {
                            let ty = member
                                .child_by_field_name("type")
                                .or_else(|| member.child_by_field_name("bounds"))
                                .map(|t| self.text(t));
                            fields.push(Field {
                                name: self.text(field_name),
                                ty,
                                optional: false,
                                kind: FieldKind::Property,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        self.push_type(item, TypeKind::Trait, fields, extends, scope, pre);
    }

    fn function(&mut self, item: Node, scope: &[String], pre: &Preamble, exported: bool) {
        let Some(name) = item.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        self.records.push(Record::Function(FunctionRecord {
            language: Language::Rust,
            name,
            scope: join_scope(scope),
            params: self.params(item),
            returns: self.returns(item),
            exported,
            location: self.location(pre.anchor, item),
            doc: pre.doc.clone(),
        }));
    }

    fn push_type(
        &mut self,
        item: Node,
        kind: TypeKind,
        fields: Vec<Field>,
        extends: Vec<String>,
        scope: &[String],
        pre: &Preamble,
    ) {
        let Some(name) = item.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        self.records.push(Record::Type(TypeRecord {
            language: Language::Rust,
            name,
            kind,
            scope: join_scope(scope),
            fields,
            extends,
            exported: is_pub(item, self.source),
            location: self.location(pre.anchor, item),
            doc: pre.doc.clone(),
        }));
    }

    /// Named fields, or `0`, `1`, … for a tuple struct.
    fn struct_fields(&self, body: Node) -> Vec<Field> {
        let mut cursor = body.walk();
        match body.kind() {
            "field_declaration_list" => body
                .named_children(&mut cursor)
                .filter(|f| f.kind() == "field_declaration")
                .filter_map(|f| {
                    Some(Field {
                        name: self.text(f.child_by_field_name("name")?),
                        ty: f.child_by_field_name("type").map(|t| self.text(t)),
                        optional: false,
                        kind: FieldKind::Property,
                    })
                })
                .collect(),
            "ordered_field_declaration_list" => body
                .children_by_field_name("type", &mut cursor)
                .enumerate()
                .map(|(i, t)| Field {
                    name: i.to_string(),
                    ty: Some(self.text(t)),
                    optional: false,
                    kind: FieldKind::Property,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn params(&self, function: Node) -> Vec<Param> {
        let Some(list) = function.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter_map(|p| match p.kind() {
                // Receivers (`self`, `&mut self`) are not arguments.
                "self_parameter" | "attribute_item" | "line_comment" | "block_comment" => None,
                "parameter" => {
                    let pattern = p.child_by_field_name("pattern")?;
                    if pattern.kind() == "self" {
                        return None;
                    }
                    Some(Param {
                        name: self.text(pattern),
                        ty: p.child_by_field_name("type").map(|t| self.text(t)),
                        optional: false,
                    })
                }
                "variadic_parameter" => Some(Param {
                    name: "...".to_string(),
                    ty: None,
                    optional: false,
                }),
                // An unnamed parameter type, e.g. in `extern` or 2015-edition trait methods.
                _ => Some(Param {
                    name: "_".to_string(),
                    ty: Some(self.text(p)),
                    optional: false,
                }),
            })
            .collect()
    }

    fn returns(&self, function: Node) -> Option<String> {
        function
            .child_by_field_name("return_type")
            .map(|t| self.text(t))
    }

    /// Attributes and `///` doc comments directly above `item`, in any order.
    fn preamble<'t>(&self, item: Node<'t>) -> Preamble<'t> {
        let mut anchor = item;
        let mut below = item;
        let mut doc_lines = Vec::new();
        let mut is_test = false;
        while let Some(prev) = below.prev_named_sibling() {
            if last_row(prev) + 1 < below.start_position().row {
                break;
            }
            match prev.kind() {
                "attribute_item" => {
                    is_test |= self.is_test_attribute(prev);
                    anchor = prev;
                }
                "line_comment" | "block_comment" => {
                    if prev.child_by_field_name("outer").is_some() {
                        if let Some(text) =
                            prev.child_by_field_name("doc").map(|d| self.doc_text(d))
                        {
                            doc_lines.push(text);
                        }
                    } else if prev.child_by_field_name("inner").is_some() {
                        break;
                    }
                    // Plain `//` comments are skipped without becoming doc.
                }
                _ => break,
            }
            below = prev;
        }
        doc_lines.reverse();
        let doc = doc_lines
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Preamble {
            anchor,
            doc: (!doc.is_empty()).then_some(doc),
            is_test,
        }
    }

    /// `#[test]`, `#[tokio::test]`, `#[cfg(test)]` (but not `#[cfg(not(test))]`).
    fn is_test_attribute(&self, attribute: Node) -> bool {
        let text: String = self.text(attribute).split_whitespace().collect();
        let inner = text.trim_start_matches("#[").trim_end_matches(']');
        inner == "test" || inner.ends_with("::test") || inner == "cfg(test)"
    }

    /// Doc comment body without its `*` gutters, on one line.
    fn doc_text(&self, doc: Node) -> String {
        self.source[doc.byte_range()]
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `Foo<T>` → `Foo`, `crate::a::Foo` → `Foo`, `&'a mut Foo` → `Foo`.
    fn base_type_name(&self, ty: Node) -> String {
        match ty.kind() {
            "generic_type" | "reference_type" | "pointer_type" => ty
                .child_by_field_name("type")
                .map_or_else(|| self.text(ty), |inner| self.base_type_name(inner)),
            "scoped_type_identifier" => ty
                .child_by_field_name("name")
                .map_or_else(|| self.text(ty), |n| self.text(n)),
            _ => self.text(ty),
        }
    }

    /// Names of types defined in a block, mapped to whether they are `pub`.
    fn type_visibility(&self, list: Node) -> HashMap<String, bool> {
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|n| {
                matches!(
                    n.kind(),
                    "struct_item" | "enum_item" | "union_item" | "type_item"
                )
            })
            .filter_map(|n| {
                let name = self.text(n.child_by_field_name("name")?);
                Some((name, is_pub(n, self.source)))
            })
            .collect()
    }

    fn location(&self, anchor: Node, item: Node) -> Location {
        Location {
            file: self.rel.to_string(),
            start_line: anchor.start_position().row + 1,
            end_line: item.end_position().row + 1,
        }
    }

    /// Node text with whitespace runs collapsed. Not quote-aware: `'a` is a lifetime.
    fn text(&self, node: Node) -> String {
        self.source[node.byte_range()]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Row of a node's last character; `//` comments end at column 0 of the next line.
fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

/// Plain `pub` only; `pub(crate)`, `pub(super)` and `pub(in …)` stay inside the crate.
fn is_pub(item: Node, source: &str) -> bool {
    let mut cursor = item.walk();
    item.named_children(&mut cursor)
        .find(|c| c.kind() == "visibility_modifier")
        .is_some_and(|v| &source[v.byte_range()] == "pub")
}

/// Method field type, e.g. `(id: u32) => Option<Item>`; no return type means `()`.
fn signature(params: &[Param], returns: Option<&str>) -> String {
    format!("({}) => {}", format_params(params), returns.unwrap_or("()"))
}

fn join_scope(scope: &[String]) -> Option<String> {
    (!scope.is_empty()).then(|| scope.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str) -> Vec<Record> {
        extract(&mut Parser::new(), source, "src/lib.rs")
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

    fn function_names(records: &[Record]) -> Vec<String> {
        functions(records)
            .iter()
            .map(|f| match &f.scope {
                Some(scope) => format!("{scope}.{}", f.name),
                None => f.name.clone(),
            })
            .collect()
    }

    #[test]
    fn structs_named_tuple_and_unit() {
        let records = run(
            "pub struct User<'a, T: Clone> { pub id: u32, name: &'a str, tags: Vec<T> }\nstruct Pair(pub i32, String);\npub struct Unit;\nunion Bits { i: u32, f: f32 }",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Struct, vec!["id", "name", "tags"])
        );
        assert_eq!(t[0].fields[1].ty.as_deref(), Some("&'a str"));
        assert!(t[0].exported && !t[1].exported);
        assert_eq!(field_names(t[1]), vec!["0", "1"]);
        assert_eq!(t[1].fields[1].ty.as_deref(), Some("String"));
        assert!(t[2].fields.is_empty());
        assert_eq!(
            (t[3].kind, field_names(t[3])),
            (TypeKind::Struct, vec!["i", "f"])
        );
    }

    #[test]
    fn enums_traits_and_aliases() {
        let records = run(
            "pub enum Status { Pending, Running(u32), Done { code: i32 } }\npub trait Store: Send + Sync + 'static {\n  type Item: Clone;\n  const LIMIT: usize;\n  fn get(&self, id: u32) -> Option<Self::Item>;\n  fn len(&self) -> usize { 0 }\n}\npub type Id = u64;",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Enum, vec!["Pending", "Running", "Done"])
        );
        assert_eq!(
            (t[1].kind, field_names(t[1])),
            (TypeKind::Trait, vec!["Item", "LIMIT", "get", "len"])
        );
        assert_eq!(t[1].extends, vec!["Send", "Sync"]);
        assert_eq!(
            t[1].fields[2].ty.as_deref(),
            Some("(id: u32) => Option<Self::Item>")
        );
        assert_eq!(t[1].fields[1].ty.as_deref(), Some("usize"));
        assert_eq!((t[2].kind, t[2].fields.len()), (TypeKind::Alias, 0));
        // Only the default method is a function.
        assert_eq!(function_names(&records), vec!["Store.len"]);
        assert!(functions(&records)[0].exported);
    }

    #[test]
    fn functions_receivers_and_visibility() {
        let records = run(
            "pub fn parse<'a>(input: &'a str, mut n: usize) -> Result<&'a str, Error> { todo!() }\npub(crate) fn helper() {}\nfn private() {}\npub struct Repo;\nimpl Repo {\n  pub fn load(&mut self, id: u32) -> Item { todo!() }\n  fn cache(self: Box<Self>) {}\n}\nimpl<T> Display for Wrapper<T> { fn fmt(&self, f: &mut Formatter) -> fmt::Result { Ok(()) } }\nstruct Hidden;\nimpl Clone for Hidden { fn clone(&self) -> Self { Hidden } }",
        );
        let f = functions(&records);
        assert_eq!(
            function_names(&records),
            vec![
                "parse",
                "helper",
                "private",
                "Repo.load",
                "Repo.cache",
                "Wrapper.fmt",
                "Hidden.clone"
            ]
        );
        let exported: Vec<_> = f.iter().map(|f| f.exported).collect();
        assert_eq!(exported, vec![true, false, false, true, false, true, false]);
        assert_eq!(f[0].params.len(), 2);
        // `mut` is a binding detail, not part of the signature.
        assert_eq!(f[0].params[1].name, "n");
        assert_eq!(f[0].returns.as_deref(), Some("Result<&'a str, Error>"));
        assert!(f[3].params.len() == 1 && f[4].params.is_empty());
    }

    #[test]
    fn modules_scope_and_test_code_is_skipped() {
        let records = run(
            "pub mod parser {\n  pub struct Tokenizer;\n  impl Tokenizer { pub fn next(&mut self) {} }\n  mod inner { pub fn deep() {} }\n}\n#[cfg(test)]\nmod tests { fn helper() {} #[test] fn works() {} }\n#[test]\nfn top_test() {}\n#[tokio::test]\nasync fn async_test() {}\n#[cfg(not(test))]\nfn real() {}\nmod external;",
        );
        assert_eq!(
            function_names(&records),
            vec!["parser.Tokenizer.next", "parser.inner.deep", "real"]
        );
        assert_eq!(types(&records)[0].scope.as_deref(), Some("parser"));
    }

    #[test]
    fn docs_skip_attributes_and_start_at_first_attribute() {
        let records = run(
            "/// A user.\n/// Second line.\n#[derive(Debug, Clone)]\n#[serde(rename_all = \"camelCase\")]\npub struct User { id: u32 }\n\n#[derive(Debug)]\n/** Block doc. */\npub enum E { A }\n\n/// Far away.\n\npub struct NoDoc;\n// plain comment\n/// Kept.\n// another\npub fn f() {}",
        );
        let t = types(&records);
        assert_eq!(t[0].doc.as_deref(), Some("A user. Second line."));
        assert_eq!((t[0].location.start_line, t[0].location.end_line), (3, 5));
        assert_eq!(
            (t[1].doc.as_deref(), t[1].location.start_line),
            (Some("Block doc."), 7)
        );
        assert_eq!(t[2].doc, None);
        assert_eq!(functions(&records)[0].doc.as_deref(), Some("Kept."));
    }

    #[test]
    fn extern_blocks_and_signatures() {
        let records = run(
            "extern \"C\" { pub fn abs(x: i32) -> i32; fn printf(fmt: *const c_char, ...) -> c_int; }\npub extern \"C\" fn callback(data: *mut c_void) {}",
        );
        let f = functions(&records);
        assert_eq!(function_names(&records), vec!["abs", "printf", "callback"]);
        assert_eq!(f[1].params.last().map(|p| p.name.as_str()), Some("..."));
        assert!(f[0].exported && f[2].exported);
    }

    #[test]
    fn macros_are_skipped() {
        let records = run("macro_rules! m { () => { fn inside() {} }; }\nm!();");
        assert!(records.is_empty());
    }
}
