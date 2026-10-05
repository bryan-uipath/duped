//! Python extractor over the tree-sitter Python grammar.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use crate::lang::typescript::{collapse_whitespace, push_field};
use crate::record::{
    Field, FieldKind, FunctionRecord, Language, Location, Param, Record, TypeKind, TypeRecord,
    format_params,
};

/// Bases whose subclasses are enums; their class-body assignments become members.
const ENUM_BASES: &[&str] = &["Enum", "IntEnum", "StrEnum", "Flag", "IntFlag"];

/// Parse one file and return its top-level functions, classes, methods and type aliases.
pub fn extract(parser: &mut Parser, source: &str, rel: &str) -> Vec<Record> {
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("bundled grammar is compatible");
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut extractor = Extractor {
        source,
        rel,
        records: Vec::new(),
        all: None,
    };
    extractor.all = extractor.dunder_all(root);
    extractor.statements(root);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    records: Vec<Record>,
    /// Names listed in a literal `__all__`, which then decides what is exported.
    all: Option<HashSet<String>>,
}

/// A `def` or `class`, with the `decorated_definition` wrapper (when present) as its anchor.
struct Definition<'t> {
    node: Node<'t>,
    anchor: Node<'t>,
    decorators: Vec<String>,
}

impl<'a> Extractor<'a> {
    /// Module level, and `if TYPE_CHECKING:` blocks within it.
    fn statements(&mut self, block: Node) {
        let overloaded = implemented(block, self.source);
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            if let Some(def) = self.definition(child) {
                let exported = self.module_exported(&self.name(def.node));
                match def.node.kind() {
                    "class_definition" => self.class(&def, &[], exported),
                    _ if is_skipped_overload(&def, &overloaded, self.source) => {}
                    _ => self.function(&def, &[], exported, false),
                }
                continue;
            }
            match child.kind() {
                "if_statement" if self.is_type_checking(child) => {
                    if let Some(body) = child.child_by_field_name("consequence") {
                        self.statements(body);
                    }
                }
                "type_alias_statement" => {
                    // `type Vector[T] = list[T]` → `Vector`
                    if let Some(left) = child.child_by_field_name("left") {
                        let text = self.text(left);
                        let name = text.split('[').next().unwrap_or(&text).trim().to_string();
                        self.alias(name, child);
                    }
                }
                "expression_statement" => {
                    // `X: TypeAlias = int`
                    let Some(assignment) =
                        child.named_child(0).filter(|n| n.kind() == "assignment")
                    else {
                        continue;
                    };
                    let is_alias = assignment.child_by_field_name("type").is_some_and(|t| {
                        matches!(self.text(t).as_str(), "TypeAlias" | "typing.TypeAlias")
                    });
                    if let (true, Some(left)) = (is_alias, assignment.child_by_field_name("left")) {
                        self.alias(self.text(left), child);
                    }
                }
                _ => {}
            }
        }
    }

    fn function(&mut self, def: &Definition, scope: &[String], exported: bool, method: bool) {
        // Methods take their receiver first, except static methods.
        let drop_receiver = method && !def.decorators.iter().any(|d| d == "staticmethod");
        let (params, _) = self.params(def.node, drop_receiver);
        let record = FunctionRecord {
            language: Language::Python,
            name: self.name(def.node),
            scope: join_scope(scope),
            params,
            returns: self.returns(def.node),
            exported,
            location: self.location(def.anchor, def.node),
            doc: self.docstring(def.node),
        };
        self.records.push(Record::Function(record));
    }

    fn class(&mut self, def: &Definition, scope: &[String], exported: bool) {
        let name = self.name(def.node);
        let extends = self.bases(def.node);
        let is_enum = extends
            .iter()
            .any(|b| ENUM_BASES.contains(&b.rsplit('.').next().unwrap_or(b)));
        let mut member_scope = scope.to_vec();
        member_scope.push(name.clone());

        let mut fields = Vec::new();
        if let Some(body) = def.node.child_by_field_name("body") {
            let overloaded = implemented(body, self.source);
            let mut cursor = body.walk();
            for statement in body.named_children(&mut cursor) {
                if let Some(member) = self.definition(statement) {
                    let member_exported = exported && is_public(&self.name(member.node));
                    if member.node.kind() == "class_definition" {
                        self.class(&member, &member_scope, member_exported);
                    } else if !is_skipped_overload(&member, &overloaded, self.source) {
                        self.method(
                            &member,
                            &member_scope,
                            member_exported,
                            is_enum,
                            &mut fields,
                        );
                    }
                } else if statement.kind() == "expression_statement" {
                    self.class_attribute(statement, is_enum, &mut fields);
                }
            }
        }

        let kind = if is_enum {
            TypeKind::Enum
        } else {
            TypeKind::Class
        };
        self.records.push(Record::Type(TypeRecord {
            language: Language::Python,
            name,
            kind,
            scope: join_scope(scope),
            fields,
            extends,
            exported,
            location: self.location(def.anchor, def.node),
            doc: self.docstring(def.node),
        }));
    }

    /// One method: a property field for `@property`, otherwise a method field plus a
    /// function record. `__init__` contributes the `self.x = …` attributes it assigns.
    fn method(
        &mut self,
        def: &Definition,
        scope: &[String],
        exported: bool,
        is_enum: bool,
        fields: &mut Vec<Field>,
    ) {
        let name = self.name(def.node);
        if def.decorators.iter().any(|d| is_property_decorator(d)) {
            if !is_enum {
                push_field(
                    fields,
                    Some(Field {
                        name,
                        ty: self.returns(def.node),
                        optional: false,
                        kind: FieldKind::Property,
                    }),
                );
            }
            return;
        }
        if name == "__init__" {
            let (_, receiver) = self.params(def.node, true);
            if let (Some(receiver), Some(body), false) =
                (receiver, def.node.child_by_field_name("body"), is_enum)
            {
                self.self_attributes(body, &receiver, fields);
            }
            return;
        }
        if !is_enum {
            let drop_receiver = !def.decorators.iter().any(|d| d == "staticmethod");
            let (params, _) = self.params(def.node, drop_receiver);
            let returns = self.returns(def.node);
            push_field(
                fields,
                Some(Field {
                    name,
                    ty: Some(format!(
                        "({}) => {}",
                        format_params(&params),
                        returns.as_deref().unwrap_or("unknown")
                    )),
                    optional: false,
                    kind: FieldKind::Method,
                }),
            );
        }
        self.function(def, scope, exported, true);
    }

    /// `x: int = 0`, `x: int` and `x = 1` in a class body; enum members in an enum.
    fn class_attribute(&self, statement: Node, is_enum: bool, fields: &mut Vec<Field>) {
        let Some(assignment) = statement
            .named_child(0)
            .filter(|n| n.kind() == "assignment")
        else {
            return;
        };
        let Some(left) = assignment.child_by_field_name("left") else {
            return;
        };
        let ty = assignment.child_by_field_name("type").map(|t| self.text(t));
        for target in targets(left)
            .into_iter()
            .filter(|t| t.kind() == "identifier")
        {
            let name = self.text(target);
            if is_enum {
                // `_ignore_`, `_order_` and other sunder names configure the enum.
                if !name.starts_with('_') {
                    push_field(fields, Some(member(name)));
                }
            } else {
                push_field(
                    fields,
                    Some(Field {
                        name,
                        ty: ty.clone(),
                        optional: false,
                        kind: FieldKind::Property,
                    }),
                );
            }
        }
    }

    /// `self.x = …` / `self.x: T = …` anywhere in `__init__`, outside nested scopes.
    fn self_attributes(&self, node: Node, receiver: &str, fields: &mut Vec<Field>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "function_definition" | "class_definition" | "decorated_definition" | "lambda" => {}
                "assignment" => self.assigned_attributes(child, receiver, fields),
                _ => self.self_attributes(child, receiver, fields),
            }
        }
    }

    /// Targets of one assignment, following chains such as `self.a = self.b = x`.
    fn assigned_attributes(&self, assignment: Node, receiver: &str, fields: &mut Vec<Field>) {
        let ty = assignment.child_by_field_name("type").map(|t| self.text(t));
        for target in assignment
            .child_by_field_name("left")
            .map(targets)
            .unwrap_or_default()
        {
            let (Some(object), Some(attribute)) = (
                target.child_by_field_name("object"),
                target.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            if self.text(object) == receiver {
                push_field(
                    fields,
                    Some(Field {
                        name: self.text(attribute),
                        ty: ty.clone(),
                        optional: false,
                        kind: FieldKind::Property,
                    }),
                );
            }
        }
        if let Some(right) = assignment
            .child_by_field_name("right")
            .filter(|r| r.kind() == "assignment")
        {
            self.assigned_attributes(right, receiver, fields);
        }
    }

    fn alias(&mut self, name: String, node: Node) {
        let exported = self.module_exported(&name);
        self.records.push(Record::Type(TypeRecord {
            language: Language::Python,
            name,
            kind: TypeKind::Alias,
            scope: None,
            fields: Vec::new(),
            extends: Vec::new(),
            exported,
            location: self.location(node, node),
            doc: None,
        }));
    }

    /// Unwrap `decorated_definition`; `None` for anything that is not a `def` or `class`.
    fn definition<'t>(&self, node: Node<'t>) -> Option<Definition<'t>> {
        match node.kind() {
            "function_definition" | "class_definition" => Some(Definition {
                node,
                anchor: node,
                decorators: Vec::new(),
            }),
            "decorated_definition" => {
                let inner = node.child_by_field_name("definition")?;
                let mut cursor = node.walk();
                let decorators = node
                    .named_children(&mut cursor)
                    .filter(|n| n.kind() == "decorator")
                    .filter_map(|d| d.named_child(0))
                    .map(|expr| {
                        // `@lru_cache(maxsize=1)` → `lru_cache`
                        let callee = if expr.kind() == "call" {
                            expr.child_by_field_name("function").unwrap_or(expr)
                        } else {
                            expr
                        };
                        self.text(callee)
                    })
                    .collect();
                Some(Definition {
                    node: inner,
                    anchor: node,
                    decorators,
                })
            }
            _ => None,
        }
    }

    /// Parameters, and the receiver's name when `drop_receiver` removed it.
    fn params(&self, def: Node, drop_receiver: bool) -> (Vec<Param>, Option<String>) {
        let Some(list) = def.child_by_field_name("parameters") else {
            return (Vec::new(), None);
        };
        let mut params = Vec::new();
        let mut cursor = list.walk();
        for p in list.named_children(&mut cursor) {
            let param = match p.kind() {
                "identifier"
                | "list_splat_pattern"
                | "dictionary_splat_pattern"
                | "tuple_pattern" => Param {
                    name: self.text(p),
                    ty: None,
                    optional: false,
                },
                "typed_parameter" => Param {
                    name: p.named_child(0).map(|n| self.text(n)).unwrap_or_default(),
                    ty: p.child_by_field_name("type").map(|t| self.text(t)),
                    optional: false,
                },
                "default_parameter" | "typed_default_parameter" => Param {
                    name: p
                        .child_by_field_name("name")
                        .map(|n| self.text(n))
                        .unwrap_or_default(),
                    ty: p.child_by_field_name("type").map(|t| self.text(t)),
                    optional: true,
                },
                // `*` and `/` separators, comments.
                _ => continue,
            };
            params.push((p.kind(), param));
        }
        // Only a plain first parameter is a receiver; `def f(*args)` keeps its splat.
        let receiver = match params.first() {
            Some((kind, param))
                if drop_receiver
                    && matches!(
                        *kind,
                        "identifier"
                            | "typed_parameter"
                            | "default_parameter"
                            | "typed_default_parameter"
                    )
                    && !param.name.starts_with('*') =>
            {
                Some(param.name.clone())
            }
            _ => None,
        };
        let skip = usize::from(receiver.is_some());
        let params = params.into_iter().skip(skip).map(|(_, p)| p).collect();
        (params, receiver)
    }

    fn returns(&self, def: Node) -> Option<String> {
        def.child_by_field_name("return_type").map(|t| self.text(t))
    }

    /// Positional bases as written; keyword arguments such as `metaclass=` are skipped.
    fn bases(&self, class: Node) -> Vec<String> {
        let Some(list) = class.child_by_field_name("superclasses") else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|n| {
                !matches!(
                    n.kind(),
                    "keyword_argument" | "comment" | "dictionary_splat"
                )
            })
            .map(|n| self.text(n))
            .collect()
    }

    /// The first statement of a body when it is a string literal, as plain text.
    fn docstring(&self, def: Node) -> Option<String> {
        let body = def.child_by_field_name("body")?;
        let mut cursor = body.walk();
        let first = body
            .named_children(&mut cursor)
            .find(|n| n.kind() != "comment")?;
        if first.kind() != "expression_statement" || first.named_child_count() != 1 {
            return None;
        }
        let string = first.named_child(0).filter(|n| n.kind() == "string")?;
        let text = string_value(string, self.source)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty()).then_some(text)
    }

    /// `__all__ = [...]` or `(...)` of string literals at module level.
    fn dunder_all(&self, root: Node) -> Option<HashSet<String>> {
        let mut cursor = root.walk();
        let mut found = None;
        for statement in root
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "expression_statement")
        {
            let Some(assignment) = statement
                .named_child(0)
                .filter(|n| n.kind() == "assignment")
            else {
                continue;
            };
            let is_all = assignment
                .child_by_field_name("left")
                .is_some_and(|l| self.text(l) == "__all__");
            let Some(right) = assignment.child_by_field_name("right").filter(|_| is_all) else {
                continue;
            };
            if !matches!(right.kind(), "list" | "tuple") {
                found = None;
                continue;
            }
            let mut items = right.walk();
            let names: Option<HashSet<String>> = right
                .named_children(&mut items)
                .filter(|n| n.kind() != "comment")
                .map(|n| (n.kind() == "string").then(|| string_value(n, self.source)))
                .collect();
            found = names;
        }
        found
    }

    fn module_exported(&self, name: &str) -> bool {
        match &self.all {
            Some(all) => all.contains(name),
            None => is_public(name),
        }
    }

    fn is_type_checking(&self, if_statement: Node) -> bool {
        if_statement
            .child_by_field_name("condition")
            .is_some_and(|c| {
                matches!(
                    self.text(c).as_str(),
                    "TYPE_CHECKING" | "typing.TYPE_CHECKING"
                )
            })
    }

    fn name(&self, def: Node) -> String {
        def.child_by_field_name("name")
            .map(|n| self.text(n))
            .unwrap_or_default()
    }

    fn location(&self, anchor: Node, node: Node) -> Location {
        Location {
            file: self.rel.to_string(),
            start_line: anchor.start_position().row + 1,
            end_line: node.end_position().row + 1,
        }
    }

    fn text(&self, node: Node) -> String {
        collapse_whitespace(&self.source[node.byte_range()])
    }
}

/// Names defined by non-`@overload` `def`s in a block; their `@overload` stubs are skipped.
fn implemented(block: Node, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        let (def, decorated) = match child.kind() {
            "function_definition" => (Some(child), None),
            "decorated_definition" => (child.child_by_field_name("definition"), Some(child)),
            _ => continue,
        };
        let Some(def) = def.filter(|d| d.kind() == "function_definition") else {
            continue;
        };
        let is_overload = decorated.is_some_and(|d| has_overload_decorator(d, source));
        if !is_overload && let Some(name) = def.child_by_field_name("name") {
            names.insert(source[name.byte_range()].to_string());
        }
    }
    names
}

fn is_skipped_overload(def: &Definition, implemented: &HashSet<String>, source: &str) -> bool {
    def.decorators.iter().any(|d| is_overload(d))
        && def
            .node
            .child_by_field_name("name")
            .is_some_and(|n| implemented.contains(&source[n.byte_range()]))
}

fn has_overload_decorator(decorated: Node, source: &str) -> bool {
    let mut cursor = decorated.walk();
    decorated
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "decorator")
        .filter_map(|d| d.named_child(0))
        .any(|expr| is_overload(&source[expr.byte_range()]))
}

fn is_overload(decorator: &str) -> bool {
    decorator == "overload" || decorator.ends_with(".overload")
}

/// `@property`, `@cached_property` and `@x.setter`-style accessors.
fn is_property_decorator(decorator: &str) -> bool {
    matches!(
        decorator.rsplit('.').next(),
        Some("property" | "cached_property" | "setter" | "getter" | "deleter")
    )
}

/// Public by Python convention: no leading `_`, except dunder names such as `__eq__`.
fn is_public(name: &str) -> bool {
    !name.starts_with('_') || (name.len() > 4 && name.starts_with("__") && name.ends_with("__"))
}

/// Assignment targets: `x`, or each element of `a, b` / `(a, b)` / `[a, b]`.
fn targets(left: Node) -> Vec<Node> {
    match left.kind() {
        "pattern_list" | "tuple_pattern" | "list_pattern" => {
            let mut cursor = left.walk();
            left.named_children(&mut cursor)
                .filter(|n| matches!(n.kind(), "identifier" | "attribute"))
                .collect()
        }
        "identifier" | "attribute" => vec![left],
        _ => Vec::new(),
    }
}

/// The literal text of a string node, without prefixes or quotes.
fn string_value(string: Node, source: &str) -> String {
    let mut cursor = string.walk();
    string
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "string_content")
        .map(|n| &source[n.byte_range()])
        .collect()
}

fn member(name: String) -> Field {
    Field {
        name,
        ty: None,
        optional: false,
        kind: FieldKind::Member,
    }
}

fn join_scope(scope: &[String]) -> Option<String> {
    (!scope.is_empty()).then(|| scope.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str) -> Vec<Record> {
        extract(&mut Parser::new(), source, "pkg/a.py")
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

    fn param_names(f: &FunctionRecord) -> Vec<&str> {
        f.params.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn functions_with_every_parameter_form() {
        let records = run(
            "async def fetch(url, retries: int, timeout=3, mode: str = 'x', *args, key, **kwargs) -> bytes:\n    \"\"\"Fetch one\n    URL.\"\"\"\n\ndef pos(a, /, b, *, c): ...\n\ndef outer():\n    def inner(): ...\n",
        );
        let f = functions(&records);
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["fetch", "pos", "outer"]
        );
        assert_eq!(
            param_names(f[0]),
            vec![
                "url", "retries", "timeout", "mode", "*args", "key", "**kwargs"
            ]
        );
        let optional: Vec<bool> = f[0].params.iter().map(|p| p.optional).collect();
        assert_eq!(
            optional,
            vec![false, false, true, true, false, false, false]
        );
        assert_eq!(f[0].params[1].ty.as_deref(), Some("int"));
        assert_eq!(f[0].params[3].ty.as_deref(), Some("str"));
        assert_eq!(f[0].returns.as_deref(), Some("bytes"));
        assert_eq!(f[0].doc.as_deref(), Some("Fetch one URL."));
        assert_eq!(param_names(f[1]), vec!["a", "b", "c"]);
        assert!(f[0].exported);
    }

    #[test]
    fn decorators_set_the_start_line() {
        let records = run("@app.route('/x')\n@cache\ndef handler():\n    pass\n");
        let [handler] = functions(&records)[..] else {
            panic!("{records:?}")
        };
        assert_eq!(
            (handler.location.start_line, handler.location.end_line),
            (1, 4)
        );
    }

    #[test]
    fn class_fields_from_every_source() {
        let records = run(
            "@dataclass\nclass User(Base, Mixin, metaclass=Meta):\n    \"\"\"A user.\"\"\"\n    id: int\n    name: str = ''\n    kind = 'user'\n    a, b = 1, 2\n    def __init__(self, x):\n        self.x = x\n        self.y: float = 0.0\n        self.p = self.q = None\n        if x:\n            self.z = 1\n        def helper():\n            self.hidden = 1\n    @property\n    def size(self) -> int: ...\n    @size.setter\n    def size(self, v: int): ...\n    def rename(self, name: str) -> None: ...\n    @staticmethod\n    def build(a: int) -> 'User': ...\n    @classmethod\n    def create(cls, b): ...\n",
        );
        let [user] = types(&records)[..] else {
            panic!("{records:?}")
        };
        assert_eq!(
            (user.kind, user.doc.as_deref()),
            (TypeKind::Class, Some("A user."))
        );
        assert_eq!(user.extends, vec!["Base", "Mixin"]);
        assert_eq!(
            field_names(user),
            vec![
                "id", "name", "kind", "a", "b", "x", "y", "p", "q", "z", "size", "rename", "build",
                "create"
            ]
        );
        let ty = |name: &str| {
            user.fields
                .iter()
                .find(|f| f.name == name)
                .and_then(|f| f.ty.as_deref())
        };
        assert_eq!(
            (ty("id"), ty("kind"), ty("y"), ty("size")),
            (Some("int"), None, Some("float"), Some("int"))
        );
        assert_eq!(ty("rename"), Some("(name: str) => None"));
        assert_eq!(
            user.fields
                .iter()
                .find(|f| f.name == "size")
                .map(|f| f.kind),
            Some(FieldKind::Property)
        );

        let f = functions(&records);
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["rename", "build", "create"]
        );
        assert_eq!(f[0].scope.as_deref(), Some("User"));
        assert_eq!(param_names(f[0]), vec!["name"]);
        assert_eq!(param_names(f[1]), vec!["a"]);
        assert_eq!(param_names(f[2]), vec!["b"]);
    }

    #[test]
    fn typed_dict_and_named_tuple_are_classes_with_fields() {
        let records = run(
            "class Movie(TypedDict, total=False):\n    title: str\n    year: int\n\nclass Point(NamedTuple):\n    x: float\n    y: float = 0.0\n",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Class, vec!["title", "year"])
        );
        assert_eq!(t[0].extends, vec!["TypedDict"]);
        assert_eq!(field_names(t[1]), vec!["x", "y"]);
    }

    #[test]
    fn enums_list_members_only() {
        let records = run(
            "class Color(enum.Enum):\n    _ignore_ = ['x']\n    RED = 1\n    GREEN = auto()\n    def describe(self) -> str: ...\n\nclass Perm(IntFlag):\n    R = 4\n",
        );
        let t = types(&records);
        assert_eq!(
            (t[0].kind, field_names(t[0])),
            (TypeKind::Enum, vec!["RED", "GREEN"])
        );
        assert_eq!((t[1].kind, field_names(t[1])), (TypeKind::Enum, vec!["R"]));
        assert_eq!(functions(&records)[0].scope.as_deref(), Some("Color"));
    }

    #[test]
    fn exported_follows_underscores_and_dunders() {
        let records = run(
            "def public(): ...\ndef _private(): ...\nclass Api:\n    def get(self): ...\n    def _helper(self): ...\n    def __eq__(self, other): ...\nclass _Internal:\n    def run(self): ...\n",
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
                ("public", true),
                ("_private", false),
                ("get", true),
                ("_helper", false),
                ("__eq__", true),
                ("Api", true),
                ("run", false),
                ("_Internal", false),
            ]
        );
    }

    #[test]
    fn dunder_all_decides_exports() {
        let records = run(
            "__all__ = ['listed', \"Model\"]\ndef listed(): ...\ndef unlisted(): ...\nclass Model: ...\n",
        );
        let exported: Vec<_> = functions(&records).iter().map(|f| f.exported).collect();
        assert_eq!(exported, vec![true, false]);
        assert!(types(&records)[0].exported);

        // A computed `__all__` falls back to the underscore rule.
        let records = run("__all__ = base.__all__ + ['x']\ndef shown(): ...\n");
        assert!(functions(&records)[0].exported);
    }

    #[test]
    fn overload_stubs_yield_to_the_implementation() {
        let records = run(
            "@overload\ndef parse(x: str) -> int: ...\n@typing.overload\ndef parse(x: bytes) -> int: ...\ndef parse(x): ...\n\nclass P:\n    @overload\n    def get(self, k: int) -> int: ...\n    def get(self, k): ...\n",
        );
        let f = functions(&records);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].params[0].ty, None);
        assert_eq!(field_names(types(&records)[0]), vec!["get"]);
    }

    #[test]
    fn type_aliases_and_type_checking_blocks() {
        let records = run(
            "type Vector[T] = list[T]\nUserId: TypeAlias = int\nif TYPE_CHECKING:\n    class Proto(Protocol):\n        def run(self) -> None: ...\nif other:\n    def skipped(): ...\n",
        );
        let t = types(&records);
        let shapes: Vec<_> = t.iter().map(|t| (t.name.as_str(), t.kind)).collect();
        assert_eq!(
            shapes,
            vec![
                ("Vector", TypeKind::Alias),
                ("UserId", TypeKind::Alias),
                ("Proto", TypeKind::Class)
            ]
        );
        assert_eq!(functions(&records)[0].name, "run");
    }

    #[test]
    fn stub_files_parse() {
        let records = extract(
            &mut Parser::new(),
            "class Client:\n    timeout: float\n    def send(self, data: bytes, *, flush: bool = ...) -> int: ...\ndef connect(host: str) -> Client: ...\n",
            "pkg/client.pyi",
        );
        assert_eq!(field_names(types(&records)[0]), vec!["timeout", "send"]);
        let f = functions(&records);
        assert_eq!(param_names(f[0]), vec!["data", "flush"]);
        assert_eq!(f[1].returns.as_deref(), Some("Client"));
    }

    #[test]
    fn receiver_is_only_a_plain_first_parameter() {
        let records = run("class C:\n    def f(*args): ...\n    def g(self: 'C', x): ...\n");
        let f = functions(&records);
        assert_eq!(param_names(f[0]), vec!["*args"]);
        assert_eq!(param_names(f[1]), vec!["x"]);
    }
}
