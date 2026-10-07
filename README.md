# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript, JavaScript, Rust, Python and C# are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

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
- `node_modules`, `dist`, `build`, `out`, `coverage`, `target`, `venv` and `site-packages` are always skipped.
- A `.pyi` stub next to a `.py` module of the same name is skipped, since both describe one API.
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, pytest's `test_*.py`, `*_test.py` and `conftest.py` (and `.pyi` equivalents), Rust `tests.rs` / `test.rs` module files, C# `*Tests.cs`, `*Mock.cs`, `*Fake.cs` and `Fake*.cs`, directories such as `test/`, `__tests__/`, `mocks/`, `fixtures/`, `benches/` and `UnitTests/` (directory names match case-insensitively), and C# test projects ending `.Tests`, `.Test`, `.UnitTests` or `.IntegrationTests` (the C# rules apply to `.cs` files only).
- C# files under `obj/` or `bin/` (build output, such as generated `*.g.cs`) are skipped.
- Files are read as UTF-8, or UTF-16 when they start with a byte-order mark. Invalid UTF-8 bytes, as in legacy single-byte files, become `�` rather than skipping the file.
- `--exclude <glob>` (repeatable) is relative to `DIR` and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript`, `javascript`, `rust`, `python` or `csharp` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment (TypeScript) or the docstring (Python), with whitespace collapsed and escapes left as written |
| `params` | function | `[{ name, type?, optional? }]`; TypeScript's `this:` and Python's method receiver (`self`, `cls`) are omitted |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `enum`, `union` (literal members become fields), `alias` (no fields), `struct`, `trait` or `record` |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

Overload signatures (TypeScript overloads, Python `@overload`) fold into their implementation, so each function appears once.

### Rust

- **Functions:** free `fn`s, `impl` methods, trait methods with a default body, and `extern` functions. Receivers (`self`, `&mut self`) are left out of `params`.
- **`scope`:** the inline `mod` chain plus the impl or trait type, e.g. `parser.Tokenizer`. `impl<T> Display for Foo<T>` is scoped to `Foo`. Impls without a named target are scoped to the trait: `impl<T> Shout for T`, `for (A, B)` and `for [u8; 4]` give `Shout`, and `dyn Any` gives `Any`.
- **Types:** structs and unions (`struct`; tuple fields are named `0`, `1`, …), enums (variants become members), traits (`trait`; methods, associated types and consts become fields; supertraits go in `extends`) and type aliases (`alias`). Impl methods are not added to a struct's fields.
- **`exported`:** plain `pub` only; `pub(crate)` and `pub(super)` count as internal. Methods in `impl Trait for X` have no `pub` of their own: they're exported unless `X` or the trait is private and defined in the same file (`super::`, `self::` and `crate::` paths are resolved within the file). Inherent methods also need `pub`.
- **`doc` and `start_line`:** `///` and `/** */` comments above the item, skipping attributes such as `#[derive]`, blank lines and plain `//` comments, as rustc does; `start_line` is the first attribute.
- **Skipped:** files starting with `#![cfg(test)]`, items under `#[cfg(test)]` or `#[cfg(all(test, …))]`, `#[test]` and `#[…::test(…)]` functions, and macros. These are skipped even with `--include-tests`, which only affects which files are walked.

### Python

- **Exported:** if the module assigns `__all__` a literal list or tuple of strings, and doesn't change it later (`+=`, `.extend()`, …), a top-level name is exported exactly when it's listed. Otherwise a name is exported when it doesn't start with `_`; dunder names like `__eq__` count as public. Methods are exported when their class is and their own name passes the same rule.
- **Classes:** fields come from annotated or plain class-body assignments, `self.x = …` assignments in `__init__`, `@property` / `@cached_property` methods (as properties, typed by the getter's return or else the setter's parameter) and other methods (as methods). Dunder names (`__slots__`, `__repr__`) are not fields, though dunder methods still get function records. This covers dataclasses, pydantic and attrs models, `TypedDict` and `NamedTuple` the same way. `__init__` itself is not a field or a function record.
- **Enums:** subclasses of `Enum`, `IntEnum`, `StrEnum`, `Flag` and `IntFlag` have kind `enum`, and their class-body assignments are the members, except sunder (`_order_`) and private (`__x`) names. Subclasses of enums defined earlier in the same file are enums too.
- **Aliases:** `type X = …` and `X: TypeAlias = …`.
- **Annotations:** quoted forward references are unquoted, so `"Base"` and `Base` match.
- **Scope:** module level and `if TYPE_CHECKING:` blocks; functions nested in functions are skipped.

### C#

- Every class, struct, interface, record, enum and delegate is extracted, including nested types. `scope` is the enclosing type chain (`Outer.Inner`); namespaces are not part of it.
- Fields are properties, fields (one per declarator), events and methods; all but methods have `kind: "property"`. A record's positional parameters are properties. Constructors, finalizers, operators and indexers are left out.
- Function records are methods with a body, including default interface methods; abstract and interface signatures are fields only. Overloads with bodies stay separate.
- `exported` means `public` (or an interface member without an access modifier, or an explicit interface implementation) on an exported type. `internal` is `false`.
- Parameter modifiers stay in the type: `this string`, `out int`, `params int[]`.
- `doc` is the `///` XML comment with tags stripped (`<see cref="T:X"/>` keeps `X`); plain `//` lines in between are skipped and `////` is not a doc comment. `start_line` includes attributes.
- Enum members inside `#if` / `#else` are all kept. An enum's storage type (`enum E : byte`) is not listed in `extends`.
- Each `partial` declaration is its own record.
