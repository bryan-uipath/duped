# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript, JavaScript and Rust are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

## Build

```sh
cargo build --release
```

## Usage

```sh
duped extract [DIR] [--out records.jsonl]   # every top-level function and type, as JSON Lines
duped index [DIR] [--out api-index.md]      # greppable markdown summary, one section per file
```

`DIR` can also be a single file.

### What gets scanned

- `.gitignore` rules apply, even outside a git checkout.
- Hidden files and directories (such as `.storybook/`) are skipped.
- `node_modules`, `dist`, `build`, `out`, `coverage` and `target` are always skipped.
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, and directories such as `test/`, `__tests__/`, `mocks/`, `fixtures/` and `benches/`.
- `--exclude <glob>` (repeatable) is relative to `DIR` and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript`, `javascript` or `rust` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment as plain text |
| `params` | function | `[{ name, type?, optional? }]`; `this:` is omitted |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `enum`, `union` (literal members become fields), `alias` (no fields), `struct` or `trait` |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

Overload signatures fold into their implementation, so each function appears once.

### Rust

- **Functions:** free `fn`s, `impl` methods, trait methods with a default body, and `extern` functions. Receivers (`self`, `&mut self`) are left out of `params`.
- **`scope`:** the inline `mod` chain plus the impl or trait type, e.g. `parser.Tokenizer`. `impl<T> Display for Foo<T>` is scoped to `Foo`.
- **Types:** structs and unions (`struct`; tuple fields are named `0`, `1`, …), enums (variants become members), traits (`trait`; methods, associated types and consts become fields; supertraits go in `extends`) and type aliases (`alias`). Impl methods are not added to a struct's fields.
- **`exported`:** plain `pub` only; `pub(crate)` and `pub(super)` count as internal. Methods in `impl Trait for X` take their visibility from the trait, so they're exported unless `X` is a private type defined in the same file. Inherent methods also need `pub`.
- **`doc` and `start_line`:** `///` and `/** */` comments directly above the item, skipping attributes such as `#[derive]`; `start_line` is the first attribute.
- **Skipped:** `#[cfg(test)]` items, `#[test]` and `#[…::test]` functions, and macros.
