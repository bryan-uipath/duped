//! Python extractor over the tree-sitter Python grammar.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::typescript::member;
use super::{collapse_whitespace, join_scope, push_field, signature};
use crate::record::{
    Field, FieldKind, FunctionRecord, Language, Location, Param, Record, TypeKind, TypeRecord,
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
        enums: HashSet::new(),
        module_stubs: HashSet::new(),
    };
    extractor.all = extractor.dunder_all(root);
    let implemented = extractor.implemented(root, true);
    extractor.statements(root, &implemented);
    extractor.records
}

struct Extractor<'a> {
    source: &'a str,
    rel: &'a str,
    records: Vec<Record>,
    /// Names listed in a literal `__all__`, which then decides what is exported.
    all: Option<HashSet<String>>,
    /// Enum classes seen so far, so `class Color(MyEnumBase)` is an enum too.
    enums: HashSet<String>,
    /// Module-level `@overload` stubs already emitted for names with no implementation.
    module_stubs: HashSet<String>,
}

/// A `def` or `class`, with the `decorated_definition` wrapper (when present) as its anchor.
struct Definition<'t> {
    node: Node<'t>,
    anchor: Node<'t>,
    decorators: Vec<String>,
}

/// How a decorated method contributes to its class.
enum Accessor {
    Getter,
    Setter,
    Deleter,
}

impl<'a> Extractor<'a> {
    /// Module level, and `if TYPE_CHECKING:` blocks within it.
    fn statements(&mut self, block: Node, implemented: &HashSet<String>) {
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            if let Some(def) = self.definition(child) {
                let exported = self.module_exported(&self.name(def.node));
                if def.node.kind() == "class_definition" {
                    self.class(&def, &[], exported);
                } else {
                    let mut stubs = std::mem::take(&mut self.module_stubs);
                    if self.emits(&def, implemented, &mut stubs) {
                        let (params, _) = self.params(def.node, false);
                        let returns = self.returns(def.node);
                        self.function(&def, &[], exported, params, returns);
                    }
                    self.module_stubs = stubs;
                }
                continue;
            }
            match child.kind() {
                "if_statement" if self.is_type_checking(child) => {
                    if let Some(body) = child.child_by_field_name("consequence") {
                        self.statements(body, implemented);
                    }
                }
                "type_alias_statement" => {
                    // `type Vector[T] = list[T]` → `Vector`
                    if let Some(left) = child.child_by_field_name("left") {
                        let text = self.text(left);
                        let name = text
                            .split('[')
                            .next()
                            .unwrap_or_default()
                            .trim()
                            .to_string();
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

    fn function(
        &mut self,
        def: &Definition,
        scope: &[String],
        exported: bool,
        params: Vec<Param>,
        returns: Option<String>,
    ) {
        let record = FunctionRecord {
            language: Language::Python,
            name: self.name(def.node),
            scope: join_scope(scope),
            params,
            returns,
            exported,
            location: self.location(def.anchor, def.node),
            doc: self.docstring(def.node),
            body: Vec::new(),
        };
        self.records.push(Record::Function(record));
    }

    fn class(&mut self, def: &Definition, scope: &[String], exported: bool) {
        let name = self.name(def.node);
        let extends = self.bases(def.node);
        let is_enum = extends.iter().any(|b| {
            ENUM_BASES.contains(&b.rsplit('.').next().unwrap_or_default()) || self.enums.contains(b)
        });
        if is_enum {
            self.enums.insert(name.clone());
        }
        let mut member_scope = scope.to_vec();
        member_scope.push(name.clone());

        let mut fields = Vec::new();
        if let Some(body) = def.node.child_by_field_name("body") {
            let implemented = self.implemented(body, false);
            let mut stubs = HashSet::new();
            let mut cursor = body.walk();
            for statement in body.named_children(&mut cursor) {
                if let Some(member) = self.definition(statement) {
                    let member_exported = exported && is_public(&self.name(member.node));
                    if member.node.kind() == "class_definition" {
                        self.class(&member, &member_scope, member_exported);
                    } else if self.emits(&member, &implemented, &mut stubs) {
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

    /// One method: a property field for accessors, otherwise a method field plus a
    /// function record. `__init__` contributes the `self.x = …` attributes it assigns.
    /// Dunder methods get a function record but no field: every class has them.
    fn method(
        &mut self,
        def: &Definition,
        scope: &[String],
        exported: bool,
        is_enum: bool,
        fields: &mut Vec<Field>,
    ) {
        let name = self.name(def.node);
        let is_static = def.decorators.iter().any(|d| d == "staticmethod");
        let (params, receiver) = self.params(def.node, !is_static);
        if let Some(accessor) = accessor(&def.decorators, &name) {
            if !is_enum {
                let ty = match accessor {
                    Accessor::Getter => self.returns(def.node),
                    Accessor::Setter => params.first().and_then(|p| p.ty.clone()),
                    Accessor::Deleter => None,
                };
                push_field(fields, Some(property(name, ty)));
            }
            return;
        }
        if name == "__init__" {
            if let (Some(receiver), Some(body), false) =
                (receiver, def.node.child_by_field_name("body"), is_enum)
            {
                self.self_attributes(body, &receiver, fields);
            }
            return;
        }
        let returns = self.returns(def.node);
        if !is_enum && !is_dunder(&name) {
            let field = Field {
                name,
                ty: Some(signature(&params, returns.as_deref())),
                optional: false,
                kind: FieldKind::Method,
            };
            push_field(fields, Some(field));
        }
        self.function(def, scope, exported, params, returns);
    }

    /// `x: int = 0`, `x: int`, `x = y = 1` and `a, b = …` in a class body; enum members in an enum.
    fn class_attribute(&self, statement: Node, is_enum: bool, fields: &mut Vec<Field>) {
        let Some(mut assignment) = statement
            .named_child(0)
            .filter(|n| n.kind() == "assignment")
        else {
            return;
        };
        let ty = assignment
            .child_by_field_name("type")
            .map(|t| self.annotation(t));
        loop {
            let has_value = assignment.child_by_field_name("right").is_some();
            for target in assignment
                .child_by_field_name("left")
                .map(targets)
                .unwrap_or_default()
            {
                if target.kind() != "identifier" {
                    continue;
                }
                let name = self.text(target);
                if is_enum {
                    // Members need a value; sunder (`_order_`) and private (`__x`) names are not members.
                    if has_value && !is_sunder(&name) && !name.starts_with("__") {
                        push_field(fields, Some(member(&name)));
                    }
                } else if !is_dunder(&name) {
                    push_field(fields, Some(property(name, ty.clone())));
                }
            }
            match assignment
                .child_by_field_name("right")
                .filter(|r| r.kind() == "assignment")
            {
                Some(next) => assignment = next,
                None => break,
            }
        }
    }

    /// `self.x = …` / `self.x: T = …` anywhere in `__init__`, outside nested scopes.
    fn self_attributes(&self, node: Node, receiver: &str, fields: &mut Vec<Field>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "function_definition" | "class_definition" => {}
                "assignment" => self.assigned_attributes(child, receiver, fields),
                _ => self.self_attributes(child, receiver, fields),
            }
        }
    }

    /// Targets of one assignment, following chains such as `self.a = self.b = x`.
    fn assigned_attributes(&self, assignment: Node, receiver: &str, fields: &mut Vec<Field>) {
        let ty = assignment
            .child_by_field_name("type")
            .map(|t| self.annotation(t));
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
                push_field(fields, Some(property(self.text(attribute), ty.clone())));
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

    /// Names of `def`s in a block that are not `@overload` stubs; on the module, also those in
    /// `if TYPE_CHECKING:` blocks, which share its scope.
    fn implemented(&self, block: Node, descend: bool) -> HashSet<String> {
        let mut names = HashSet::new();
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            if let Some(def) = self.definition(child) {
                if def.node.kind() == "function_definition"
                    && !def.decorators.iter().any(|d| is_overload(d))
                {
                    names.insert(self.name(def.node));
                }
            } else if descend
                && child.kind() == "if_statement"
                && self.is_type_checking(child)
                && let Some(body) = child.child_by_field_name("consequence")
            {
                names.extend(self.implemented(body, true));
            }
        }
        names
    }

    /// Whether a `def` produces records: an `@overload` stub yields to its implementation, and
    /// when there is none, only the first stub of a name is kept.
    fn emits(
        &self,
        def: &Definition,
        implemented: &HashSet<String>,
        stubs: &mut HashSet<String>,
    ) -> bool {
        if !def.decorators.iter().any(|d| is_overload(d)) {
            return true;
        }
        let name = self.name(def.node);
        !implemented.contains(&name) && stubs.insert(name)
    }

    /// Parameters, and the receiver's name when `drop_receiver` removed it. Only a plain first
    /// parameter is a receiver: `def f(*args)` and `def f(*, x)` keep theirs.
    fn params(&self, def: Node, drop_receiver: bool) -> (Vec<Param>, Option<String>) {
        let Some(list) = def.child_by_field_name("parameters") else {
            return (Vec::new(), None);
        };
        let mut params = Vec::new();
        let mut receiver = None;
        let mut first = true;
        let mut cursor = list.walk();
        for p in list
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
        {
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
                    ty: p.child_by_field_name("type").map(|t| self.annotation(t)),
                    optional: false,
                },
                "default_parameter" | "typed_default_parameter" => Param {
                    name: p
                        .child_by_field_name("name")
                        .map(|n| self.text(n))
                        .unwrap_or_default(),
                    ty: p.child_by_field_name("type").map(|t| self.annotation(t)),
                    optional: true,
                },
                // `*` and `/` separators.
                _ => {
                    first = false;
                    continue;
                }
            };
            let is_receiver =
                std::mem::take(&mut first) && drop_receiver && !param.name.starts_with(['*', '(']);
            if is_receiver {
                receiver = Some(param.name);
            } else {
                params.push(param);
            }
        }
        (params, receiver)
    }

    fn returns(&self, def: Node) -> Option<String> {
        def.child_by_field_name("return_type")
            .map(|t| self.annotation(t))
    }

    /// An annotation as written, with forward-reference quotes removed: `"Base"` → `Base`.
    fn annotation(&self, node: Node) -> String {
        node.named_child(0)
            .filter(|n| node.named_child_count() == 1 && n.kind() == "string")
            .and_then(|n| literal_string(n, self.source))
            .unwrap_or_else(|| self.text(node))
    }

    /// Positional bases as written; keyword arguments such as `metaclass=` and splats are skipped.
    fn bases(&self, class: Node) -> Vec<String> {
        let Some(list) = class.child_by_field_name("superclasses") else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|n| {
                !matches!(
                    n.kind(),
                    "keyword_argument" | "comment" | "dictionary_splat" | "list_splat"
                )
            })
            .map(|n| self.text(n))
            .collect()
    }

    /// The first statement of a body when it is a string literal, as written.
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
        let text = literal_string(string, self.source)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty()).then_some(text)
    }

    /// `__all__ = [...]` or `(...)` of string literals at module level. Any other change to
    /// `__all__` (`+=`, `.extend()`, a computed value) makes it unknown, so the underscore
    /// rule applies.
    fn dunder_all(&self, root: Node) -> Option<HashSet<String>> {
        let mut cursor = root.walk();
        let mut found = None;
        for statement in root
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "expression_statement")
        {
            let Some(expr) = statement.named_child(0) else {
                continue;
            };
            match expr.kind() {
                "assignment" if self.targets_all(expr.child_by_field_name("left")) => {
                    found = expr
                        .child_by_field_name("right")
                        .filter(|r| matches!(r.kind(), "list" | "tuple"))
                        .and_then(|right| {
                            let mut items = right.walk();
                            right
                                .named_children(&mut items)
                                .filter(|n| n.kind() != "comment")
                                .map(|n| {
                                    (n.kind() == "string")
                                        .then(|| literal_string(n, self.source))
                                        .flatten()
                                })
                                .collect()
                        });
                }
                "augmented_assignment" if self.targets_all(expr.child_by_field_name("left")) => {
                    found = None
                }
                "call" => {
                    let mutates = expr
                        .child_by_field_name("function")
                        .filter(|f| f.kind() == "attribute")
                        .is_some_and(|f| self.targets_all(f.child_by_field_name("object")));
                    if mutates {
                        found = None;
                    }
                }
                _ => {}
            }
        }
        found
    }

    fn targets_all(&self, node: Option<Node>) -> bool {
        node.is_some_and(|n| self.text(n) == "__all__")
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

/// `@property` / `@cached_property` / `@x.getter` read; `@x.setter` / `@x.deleter` must name
/// the method they decorate, so `@validator("x").setter` stays a plain method.
fn accessor(decorators: &[String], name: &str) -> Option<Accessor> {
    decorators.iter().find_map(|d| match d.rsplit_once('.') {
        Some((prefix, "setter")) if prefix == name => Some(Accessor::Setter),
        Some((prefix, "deleter")) if prefix == name => Some(Accessor::Deleter),
        Some((prefix, "getter")) if prefix == name => Some(Accessor::Getter),
        Some((_, "property" | "cached_property")) => Some(Accessor::Getter),
        None if matches!(d.as_str(), "property" | "cached_property") => Some(Accessor::Getter),
        _ => None,
    })
}

fn is_overload(decorator: &str) -> bool {
    decorator == "overload" || decorator.ends_with(".overload")
}

/// Public by Python convention: no leading `_`, except dunder names such as `__eq__`.
fn is_public(name: &str) -> bool {
    !name.starts_with('_') || is_dunder(name)
}

/// `__eq__`, `__slots__`: protocol names every class may define.
fn is_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// `_order_`, `_ignore_`: names reserved by `enum`.
fn is_sunder(name: &str) -> bool {
    name.len() > 2 && name.starts_with('_') && name.ends_with('_') && !name.starts_with("__")
}

/// Assignment targets, unpacking `a, b`, `(a, (b, c))`, `[a, b]` and `first, *rest`.
fn targets(left: Node) -> Vec<Node> {
    match left.kind() {
        "identifier" | "attribute" => vec![left],
        "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "list_splat_pattern"
        | "parenthesized_expression" => {
            let mut cursor = left.walk();
            left.named_children(&mut cursor).flat_map(targets).collect()
        }
        _ => Vec::new(),
    }
}

/// The text of a string literal without prefixes or quotes; `None` for f-strings.
fn literal_string(string: Node, source: &str) -> Option<String> {
    let mut cursor = string.walk();
    let mut text = String::new();
    for part in string.named_children(&mut cursor) {
        match part.kind() {
            "string_content" => text.push_str(&source[part.byte_range()]),
            "interpolation" => return None,
            _ => {}
        }
    }
    Some(text)
}

fn property(name: String, ty: Option<String>) -> Field {
    Field {
        name,
        ty,
        optional: false,
        kind: FieldKind::Property,
    }
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

    #[test]
    fn dunder_all_mutations_fall_back_to_underscores() {
        for source in [
            "__all__ = ['a']\n__all__ += ['b']\ndef b(): ...\n",
            "__all__ = ['a']\n__all__.extend(['b'])\ndef b(): ...\n",
            "name = 'b'\n__all__ = [f'{name}']\ndef b(): ...\n",
        ] {
            assert!(functions(&run(source))[0].exported, "{source}");
        }
        let records = run("__all__ = ('a',)\ndef a(): ...\ndef b(): ...\n");
        let exported: Vec<_> = functions(&records).iter().map(|f| f.exported).collect();
        assert_eq!(exported, vec![true, false]);
    }

    #[test]
    fn class_attribute_edge_cases() {
        let records = run(
            "class C:\n    __slots__ = ('a',)\n    x = y = 0\n    def __init__(self):\n        (self.a, self.b), self.c = (1, 2), 3\n        self.first, *self.rest = [1, 2]\n    def __repr__(self) -> str: ...\n",
        );
        let [c] = types(&records)[..] else {
            panic!("{records:?}")
        };
        assert_eq!(
            field_names(c),
            vec!["x", "y", "a", "b", "c", "first", "rest"]
        );
        assert_eq!(functions(&records)[0].name, "__repr__");
    }

    #[test]
    fn accessor_types_come_from_getter_or_setter_parameter() {
        let records = run(
            "class C:\n    @property\n    def a(self): ...\n    @a.setter\n    def a(self, v: int) -> None: ...\n    @functools.cached_property\n    def b(self) -> str: ...\n    @validator('x').setter\n    def check(self, v): ...\n",
        );
        let c = types(&records)[0];
        let shapes: Vec<_> = c
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.ty.as_deref(), f.kind))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("a", Some("int"), FieldKind::Property),
                ("b", Some("str"), FieldKind::Property),
                ("check", Some("(v) => unknown"), FieldKind::Method),
            ]
        );
    }

    #[test]
    fn enum_members_and_inherited_enums() {
        let records = run(
            "class Base(str, Enum):\n    pass\nclass Color(Base):\n    _private = 1\n    RED = CRIMSON = 'r'\n    label: str\n    __secret = 2\n",
        );
        let t = types(&records);
        assert_eq!(
            (t[1].kind, field_names(t[1])),
            (TypeKind::Enum, vec!["_private", "RED", "CRIMSON"])
        );
    }

    #[test]
    fn overloads_fold_across_type_checking_and_stubs() {
        let records = run(
            "if TYPE_CHECKING:\n    @overload\n    def parse(x: str) -> int: ...\n    @overload\n    def parse(x: bytes) -> int: ...\ndef parse(x): ...\n",
        );
        assert_eq!(functions(&records).len(), 1);
        let stub = extract(
            &mut Parser::new(),
            "@overload\ndef f(x: int) -> int: ...\n@overload\ndef f(x: str) -> str: ...\nclass K:\n    @overload\n    def g(self, a: int): ...\n    @overload\n    def g(self, a: str): ...\n",
            "pkg/a.pyi",
        );
        let f = functions(&stub);
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["f", "g"]
        );
        assert_eq!(f[0].params[0].ty.as_deref(), Some("int"));
    }

    #[test]
    fn annotations_receivers_and_bases() {
        let records = run(
            "class D(*mixins, Base):\n    async def run(self, *items: int, **opts: str) -> \"Result\": ...\n    def kw(*, self): ...\n",
        );
        assert_eq!(types(&records)[0].extends, vec!["Base"]);
        let f = functions(&records);
        assert_eq!(param_names(f[0]), vec!["*items", "**opts"]);
        assert_eq!(
            (f[0].params[0].ty.as_deref(), f[0].returns.as_deref()),
            (Some("int"), Some("Result"))
        );
        assert_eq!(param_names(f[1]), vec!["self"]);
    }

    #[test]
    fn qualified_typing_names() {
        let records = run(
            "import typing\nif typing.TYPE_CHECKING:\n    def hidden(): ...\nPath: typing.TypeAlias = str\n",
        );
        assert_eq!(functions(&records)[0].name, "hidden");
        assert_eq!(types(&records)[0].kind, TypeKind::Alias);
    }
}
