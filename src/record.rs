//! Language-neutral records every extractor emits and every analysis reads.

use serde::Serialize;

/// One extracted declaration, serialized as a JSON Lines row.
#[derive(Debug, Serialize)]
#[serde(tag = "record", rename_all = "lowercase")]
pub enum Record {
    Function(FunctionRecord),
    Type(TypeRecord),
}

#[derive(Debug, Serialize)]
pub struct FunctionRecord {
    pub language: Language,
    pub name: String,
    /// Enclosing class or namespace chain, outermost first, e.g. `Outer.Inner`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub params: Vec<Param>,
    /// Return type as written; `None` when not annotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
    pub exported: bool,
    #[serde(flatten)]
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

#[derive(Debug, Serialize)]
pub struct TypeRecord {
    pub language: Language,
    pub name: String,
    pub kind: TypeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub fields: Vec<Field>,
    /// Base types and implemented interfaces, as written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    pub exported: bool,
    #[serde(flatten)]
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeKind {
    Interface,
    /// Object-shaped type alias, e.g. `type A = { x: number }`.
    Type,
    Class,
    Enum,
    /// Union of literals, e.g. `type Status = 'a' | 'b'`; members become fields.
    Union,
    /// Any other alias; recorded for the index, with no fields.
    Alias,
    /// Rust struct, C# struct.
    Struct,
    /// Rust trait; methods become fields.
    #[allow(dead_code)]
    Trait,
    /// C# record; positional parameters become fields.
    Record,
}

#[derive(Debug, Serialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Property,
    Method,
    /// Enum member or union literal.
    Member,
}

#[derive(Debug, Serialize)]
pub struct Location {
    /// Root-relative, `/`-separated.
    pub file: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    TypeScript,
    JavaScript,
    CSharp,
}

impl Record {
    pub fn location(&self) -> &Location {
        match self {
            Record::Function(f) => &f.location,
            Record::Type(t) => &t.location,
        }
    }
}

/// `id: string, force?: boolean`: parameters as written in a signature.
pub fn format_params(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            let optional = if p.optional { "?" } else { "" };
            match &p.ty {
                Some(ty) => format!("{}{optional}: {ty}", p.name),
                None => format!("{}{optional}", p.name),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
