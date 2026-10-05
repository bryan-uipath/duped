# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript, JavaScript and C# are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

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
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, C# `*Tests.cs` / `*Test.cs`, directories such as `test/`, `__tests__/`, `mocks/` and `fixtures/`, and C# test projects ending `.Tests`, `.Test`, `.UnitTests` or `.IntegrationTests`.
- Files are read as UTF-8, or UTF-16 when they start with a byte-order mark. Invalid UTF-8 bytes, as in legacy single-byte files, become `�` rather than skipping the file.
- `--exclude <glob>` (repeatable) is relative to `DIR` and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript`, `javascript` or `csharp` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment as plain text |
| `params` | function | `[{ name, type?, optional? }]`; `this:` is omitted |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `struct`, `record`, `enum`, `union` (literal members become fields) or `alias` (no fields) |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

TypeScript overload signatures fold into their implementation, so each function appears once.

### C#

- Every class, struct, interface, record, enum and delegate is extracted, including nested types. `scope` is the enclosing type chain (`Outer.Inner`); namespaces are not part of it.
- Fields are properties, fields (one per declarator), events and methods. A record's positional parameters are properties. Constructors, finalizers, operators and indexers are left out.
- Function records are methods with a body, including default interface methods; abstract and interface signatures are fields only. Overloads with bodies stay separate.
- `exported` means `public` (or an interface member without an access modifier, or an explicit interface implementation) on an exported type. `internal` is `false`.
- Parameter modifiers stay in the type: `this string`, `out int`, `params int[]`.
- `doc` is the `///` XML comment with tags stripped; `start_line` includes attributes.
- Each `partial` declaration is its own record.
