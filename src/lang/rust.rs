//! Rust extractor over the tree-sitter Rust grammar.

use std::collections::{HashMap, HashSet};

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
    let root = tree.root_node();
    let mut extractor = Extractor {
        source,
        rel,
        records: Vec::new(),
        visibility: HashMap::new(),
    };
    // `#![cfg(test)]` makes the whole file test code, e.g. a `tests.rs` module.
    let mut cursor = root.walk();
    if root
        .named_children(&mut cursor)
        .any(|n| n.kind() == "inner_attribute_item" && extractor.is_test_attribute(n))
    {
        return Vec::new();
    }
    extractor.collect_visibility(root, &[]);
    extractor.items(root, &[]);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    records: Vec<Record>,
    /// Types and traits defined in this file, keyed by module path (`m.Hidden`), to `pub`.
    visibility: HashMap<String, bool>,
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
        let mut cursor = list.walk();
        for item in list.named_children(&mut cursor) {
            let preamble = self.preamble(item);
            if preamble.is_test {
                continue;
            }
            self.item(item, scope, &preamble);
        }
    }

    fn item(&mut self, item: Node, scope: &[String], pre: &Preamble) {
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
            "impl_item" => self.implementation(item, scope),
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
    fn implementation(&mut self, item: Node, scope: &[String]) {
        let (Some(ty), Some(body)) = (
            item.child_by_field_name("type"),
            item.child_by_field_name("body"),
        ) else {
            return;
        };
        let generics = self.type_parameters(item);
        let target = self
            .resolve(ty, scope)
            .filter(|t| !generics.contains(&t.name));
        let trait_ref = item
            .child_by_field_name("trait")
            .and_then(|t| self.resolve(t, scope));
        // Only a type or trait defined privately in this file hides the methods.
        let visible = |r: &Option<TypeRef>| {
            r.as_ref()
                .and_then(|r| r.key.as_ref())
                .and_then(|k| self.visibility.get(k).copied())
                .unwrap_or(true)
        };
        let type_public = visible(&target);
        let trait_public = visible(&trait_ref);
        let is_trait_impl = item.child_by_field_name("trait").is_some();
        // Blanket (`impl<T> X for T`), `dyn`, tuple and array targets are named by the trait.
        let name = target
            .or(trait_ref)
            .map_or_else(|| self.text(ty), |r| r.name);
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
            // Trait methods take the trait's visibility; inherent methods need their own `pub`.
            let exported = type_public
                && if is_trait_impl {
                    trait_public
                } else {
                    is_pub(member, self.source)
                };
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
                    .filter(|b| !matches!(b.kind(), "lifetime" | "removed_trait_bound"))
                    .map(|b| self.text(b))
                    .collect()
            })
            .unwrap_or_default();

        let mut member_scope = scope.to_vec();
        member_scope.push(name);
        let mut fields = Vec::new();
        let mut defaults = Vec::new();
        if let Some(body) = item.child_by_field_name("body") {
            let mut cursor = body.walk();
            for member in body.named_children(&mut cursor) {
                let member_pre = self.preamble(member);
                if member_pre.is_test {
                    continue;
                }
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
                            defaults.push((member, member_pre));
                        }
                    }
                    "associated_type" | "const_item" => {
                        if let Some(field_name) = member.child_by_field_name("name") {
                            // `type Item: Clone + Send` → `Clone + Send`.
                            let ty = member
                                .child_by_field_name("type")
                                .or_else(|| member.child_by_field_name("bounds"))
                                .map(|t| self.text(t).trim_start_matches(':').trim().to_string());
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
        // After the trait, so a one-line trait still lists before its methods.
        for (member, member_pre) in defaults {
            self.function(member, &member_scope, &member_pre, exported);
        }
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

    /// Attributes and outer doc comments above `item`. Like rustc, blank lines and
    /// plain `//` comments between them don't detach them from the item.
    fn preamble<'t>(&self, item: Node<'t>) -> Preamble<'t> {
        let mut anchor = item;
        let mut below = item;
        let mut doc_lines = Vec::new();
        let mut is_test = false;
        while let Some(prev) = below.prev_named_sibling() {
            match prev.kind() {
                "attribute_item" => {
                    is_test |= self.is_test_attribute(prev);
                    anchor = prev;
                }
                "line_comment" | "block_comment" => {
                    if prev.child_by_field_name("outer").is_some() {
                        if let Some(doc) = prev.child_by_field_name("doc") {
                            doc_lines.push(self.doc_text(doc, prev.kind() == "block_comment"));
                        }
                    } else if prev.child_by_field_name("inner").is_some() {
                        break;
                    }
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

    /// `#[test]`, `#[tokio::test(flavor = "…")]`, `#[cfg(test)]`, `#[cfg(all(test, …))]`.
    /// Not `#[cfg(not(test))]` or `#[cfg(any(test, …))]`, which also build outside tests.
    fn is_test_attribute(&self, attribute: Node) -> bool {
        let text: String = self.source[attribute.byte_range()]
            .split_whitespace()
            .collect();
        let inner = text
            .trim_start_matches("#!")
            .trim_start_matches('#')
            .trim_start_matches('[')
            .trim_end_matches(']');
        let path = inner.split('(').next().unwrap_or(inner);
        if path == "test" || path.ends_with("::test") || inner == "cfg(test)" {
            return true;
        }
        inner
            .strip_prefix("cfg(all(")
            .and_then(|rest| rest.strip_suffix("))"))
            .is_some_and(|args| top_level_args(args).any(|a| a == "test"))
    }

    /// Doc text on one line; `/** */` bodies also lose their `*` gutters.
    fn doc_text(&self, doc: Node, block: bool) -> String {
        self.source[doc.byte_range()]
            .lines()
            .map(|l| {
                let l = l.trim();
                if block {
                    l.trim_start_matches('*').trim()
                } else {
                    l
                }
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The type an impl targets, e.g. `super::a::Foo<T>` in `m` → `a.Foo`. `None` for
    /// targets without a name (tuples, arrays, `fn` types).
    fn resolve(&self, ty: Node, scope: &[String]) -> Option<TypeRef> {
        match ty.kind() {
            "generic_type" | "reference_type" | "pointer_type" => {
                self.resolve(ty.child_by_field_name("type")?, scope)
            }
            // `dyn Trait` has no visibility of its own here; name it by the trait.
            "dynamic_type" => {
                let name = self.resolve(ty.child_by_field_name("trait")?, scope)?.name;
                Some(TypeRef { name, key: None })
            }
            "type_identifier" | "primitive_type" => {
                let name = self.text(ty);
                Some(TypeRef {
                    key: Some(module_key(scope, &name)),
                    name,
                })
            }
            "scoped_type_identifier" => {
                let name = self.text(ty.child_by_field_name("name")?);
                let mut module = scope.to_vec();
                if let Some(path) = ty.child_by_field_name("path") {
                    for segment in self.text(path).split("::").map(str::trim) {
                        match segment {
                            "crate" => module.clear(),
                            "super" => {
                                module.pop();
                            }
                            "self" => {}
                            other => module.push(other.to_string()),
                        }
                    }
                }
                Some(TypeRef {
                    key: Some(module_key(&module, &name)),
                    name,
                })
            }
            _ => None,
        }
    }

    /// Names of an impl's generic type parameters, e.g. `T` in `impl<T> X for T`.
    fn type_parameters(&self, item: Node) -> HashSet<String> {
        let Some(params) = item.child_by_field_name("type_parameters") else {
            return HashSet::new();
        };
        let mut cursor = params.walk();
        params
            .named_children(&mut cursor)
            .filter(|p| p.kind() == "type_parameter")
            .filter_map(|p| p.child_by_field_name("name"))
            .map(|n| self.text(n))
            .collect()
    }

    /// Record every type and trait in the file, including inline modules.
    fn collect_visibility(&mut self, list: Node, scope: &[String]) {
        let mut cursor = list.walk();
        for item in list.named_children(&mut cursor) {
            match item.kind() {
                "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item" => {
                    if let Some(name) = item.child_by_field_name("name") {
                        let key = module_key(scope, &self.text(name));
                        let public = is_pub(item, self.source);
                        self.visibility.insert(key, public);
                    }
                }
                "mod_item" => {
                    if let (Some(name), Some(body)) = (
                        item.child_by_field_name("name"),
                        item.child_by_field_name("body"),
                    ) {
                        let mut inner = scope.to_vec();
                        inner.push(self.text(name));
                        self.collect_visibility(body, &inner);
                    }
                }
                _ => {}
            }
        }
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

/// An impl target or trait: its display name and, when it can be in this file, its key.
struct TypeRef {
    name: String,
    key: Option<String>,
}

fn module_key(module: &[String], name: &str) -> String {
    module
        .iter()
        .map(String::as_str)
        .chain([name])
        .collect::<Vec<_>>()
        .join(".")
}

/// `test, feature = "x"` → `test`, `feature="x"`, splitting only at depth 0.
fn top_level_args(args: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts.into_iter()
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
            "/// A user.\n/// Second line.\n#[derive(Debug, Clone)]\n#[serde(rename_all = \"camelCase\")]\npub struct User { id: u32 }\n\n#[derive(Debug)]\n/** Block doc.\n * Gutter. */\npub enum E { A }\n\n/// Across a blank line, like rustc.\n\npub struct Spaced;\n// plain comment\n/// * bullet kept\n// another\npub fn f() {}\n#[inline]\n\npub fn g() {}",
        );
        let t = types(&records);
        assert_eq!(t[0].doc.as_deref(), Some("A user. Second line."));
        assert_eq!((t[0].location.start_line, t[0].location.end_line), (3, 5));
        assert_eq!(
            (t[1].doc.as_deref(), t[1].location.start_line),
            (Some("Block doc. Gutter."), 7)
        );
        assert_eq!(
            t[2].doc.as_deref(),
            Some("Across a blank line, like rustc.")
        );
        let f = functions(&records);
        assert_eq!(f[0].doc.as_deref(), Some("* bullet kept"));
        assert_eq!(f[1].location.start_line, 19);
    }

    #[test]
    fn test_attributes_with_arguments_and_gaps() {
        let records = run(
            "#[tokio::test(flavor = \"multi_thread\")]\nasync fn a() {}\n#[test_log::test(tokio::test)]\nasync fn b() {}\n#[cfg(all(test, feature = \"x\"))]\nmod gated { pub fn c() {} }\n#[cfg(test)]\n\nmod spaced { pub fn d() {} }\n#[cfg(any(test, feature = \"y\"))]\npub fn kept_any() {}\npub trait Tr { #[cfg(test)] fn only_test(&self) {} #[cfg(test)] type T; fn real(&self) {} }",
        );
        assert_eq!(function_names(&records), vec!["kept_any", "Tr.real"]);
        assert_eq!(field_names(types(&records)[0]), vec!["real"]);
        assert!(run("#![cfg(test)]\npub fn helper() {}\npub struct Fixture;").is_empty());
    }

    #[test]
    fn impl_visibility_resolves_module_paths_and_traits() {
        let records = run(
            "struct Hidden;\nmod m {\n  impl super::Hidden { pub fn inherent(&self) {} }\n  impl Clone for super::Hidden { fn clone(&self) -> Self { todo!() } }\n}\nstruct Dup;\nmod inner { pub struct Dup; }\nimpl Clone for inner::Dup { fn clone(&self) -> Self { todo!() } }\ntrait Sealed { fn seal(&self); }\npub struct Open;\nimpl Sealed for Open { fn seal(&self) {} }\nimpl Clone for crate::Open { fn clone(&self) -> Self { todo!() } }",
        );
        let exported: Vec<_> = functions(&records)
            .iter()
            .map(|f| {
                (
                    format!("{}.{}", f.scope.as_deref().unwrap_or(""), f.name),
                    f.exported,
                )
            })
            .collect();
        assert_eq!(
            exported,
            vec![
                ("m.Hidden.inherent".to_string(), false),
                ("m.Hidden.clone".to_string(), false),
                ("Dup.clone".to_string(), true),
                ("Open.seal".to_string(), false),
                ("Open.clone".to_string(), true),
            ]
        );
    }

    #[test]
    fn non_nominal_impl_targets_are_named_by_the_trait() {
        let records = run(
            "impl<T: Display> Shout for T { fn shout(&self) {} }\nimpl<T> Pointer for *const T { fn distance(&self) {} }\nimpl Shout for dyn std::any::Any { fn shout(&self) {} }\nimpl Shout for (u8, u16) { fn shout(&self) {} }\nimpl Shout for [u8; 4] { fn shout(&self) {} }\nimpl dyn Store { pub fn helper(&self) {} }",
        );
        assert_eq!(
            function_names(&records),
            vec![
                "Shout.shout",
                "Pointer.distance",
                "Any.shout",
                "Shout.shout",
                "Shout.shout",
                "Store.helper"
            ]
        );
    }

    #[test]
    fn trait_bounds_and_one_line_ordering() {
        let records = run(
            "pub trait Store: Send + ?Sized { type Item: Clone + Send; fn len(&self) -> usize { 0 } }",
        );
        let Record::Type(store) = &records[0] else {
            panic!("trait should come first: {records:?}")
        };
        assert_eq!(store.extends, vec!["Send"]);
        assert_eq!(store.fields[0].ty.as_deref(), Some("Clone + Send"));
        let index = crate::index::render(&records);
        assert!(index.contains("- trait `Store` extends Send { Item, len() } (L1)"));
        assert!(index.contains("- fn `Store.len(): usize` (L1)"));
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
