//! Greppable markdown summary of extracted records, one section per file.

use std::fmt::Write;

use crate::record::{FieldKind, FunctionRecord, Param, Record, TypeKind, TypeRecord};

/// Fields shown per type before the list is cut off with `+N more`.
const MAX_FIELDS: usize = 16;
/// Doc text shown per entry, in characters.
const MAX_DOC: usize = 120;

pub fn render(records: &[Record]) -> String {
    let functions = records
        .iter()
        .filter(|r| matches!(r, Record::Function(_)))
        .count();
    let types = records.len() - functions;
    let mut out = String::new();
    let mut current_file = "";
    let files = records
        .iter()
        .map(|r| r.location().file.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    writeln!(
        out,
        "# API index\n\n{functions} functions and {types} types in {} files.",
        files.len()
    )
    .ok();

    for record in records {
        let file = record.location().file.as_str();
        if file != current_file {
            writeln!(out, "\n## {file}").ok();
            current_file = file;
        }
        let line = match record {
            Record::Function(f) => function_line(f),
            Record::Type(t) => type_line(t),
        };
        writeln!(out, "- {line}").ok();
    }
    out
}

/// e.g. `` fn `Repo.load(id: string, force?): Promise<Item>` — Loads one. (L12) ``
fn function_line(f: &FunctionRecord) -> String {
    let qualified = match &f.scope {
        Some(scope) => format!("{scope}.{}", f.name),
        None => f.name.clone(),
    };
    let returns = f
        .returns
        .as_deref()
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    let head = format!("fn `{qualified}({}){returns}`", params(&f.params));
    finish(head, f.doc.as_deref(), f.location.start_line, f.exported)
}

/// e.g. `` interface `User` extends Base { id, email?, rename() } (L3) ``
fn type_line(t: &TypeRecord) -> String {
    let kind = match t.kind {
        TypeKind::Interface => "interface",
        TypeKind::Type => "type",
        TypeKind::Class => "class",
        TypeKind::Enum => "enum",
        TypeKind::Union => "union",
        TypeKind::Alias => "alias",
    };
    let name = match &t.scope {
        Some(scope) => format!("{scope}.{}", t.name),
        None => t.name.clone(),
    };
    let mut head = format!("{kind} `{name}`");
    if !t.extends.is_empty() {
        write!(head, " extends {}", t.extends.join(", ")).ok();
    }
    if !t.fields.is_empty() {
        let mut shown: Vec<String> = t
            .fields
            .iter()
            .take(MAX_FIELDS)
            .map(|field| match field.kind {
                FieldKind::Method => format!("{}()", field.name),
                _ if field.optional => format!("{}?", field.name),
                _ => field.name.clone(),
            })
            .collect();
        if t.fields.len() > MAX_FIELDS {
            shown.push(format!("+{} more", t.fields.len() - MAX_FIELDS));
        }
        write!(head, " {{ {} }}", shown.join(", ")).ok();
    }
    finish(head, t.doc.as_deref(), t.location.start_line, t.exported)
}

fn finish(mut line: String, doc: Option<&str>, start_line: usize, exported: bool) -> String {
    if let Some(doc) = doc {
        let short: String = doc.chars().take(MAX_DOC).collect();
        let ellipsis = if doc.chars().count() > MAX_DOC {
            "…"
        } else {
            ""
        };
        write!(line, " — {short}{ellipsis}").ok();
    }
    let local = if exported { "" } else { ", local" };
    write!(line, " (L{start_line}{local})").ok();
    line
}

fn params(params: &[Param]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::typescript;
    use crate::record::Language;
    use tree_sitter::Parser;

    #[test]
    fn renders_functions_and_types() {
        let records = typescript::extract(
            &mut Parser::new(),
            "/** Loads one. */\nexport function load(id: string, force?: boolean): Item {}\ninterface User extends Base { id: string; email?: string; rename(): void }",
            "src/a.ts",
            Language::TypeScript,
        );
        let out = render(&records);
        assert!(out.contains("1 functions and 1 types in 1 files."));
        assert!(out.contains("## src/a.ts"));
        assert!(out.contains("- fn `load(id: string, force?: boolean): Item` — Loads one. (L2)"));
        assert!(
            out.contains("- interface `User` extends Base { id, email?, rename() } (L3, local)")
        );
    }
}
