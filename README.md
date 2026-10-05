# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript, JavaScript and Python are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

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
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, pytest's `test_*.py`, `*_test.py` and `conftest.py`, and directories such as `test/`, `__tests__/`, `mocks/` and `fixtures/`.
- `--exclude <glob>` (repeatable) is relative to `DIR` and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript`, `javascript` or `python` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment as plain text |
| `params` | function | `[{ name, type?, optional? }]`; TypeScript's `this:` and Python's method receiver (`self`, `cls`) are omitted |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `enum`, `union` (literal members become fields) or `alias` (no fields) |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

Overload signatures (TypeScript overloads, Python `@overload`) fold into their implementation, so each function appears once.

### Python

- **Exported:** if the module assigns `__all__` a literal list or tuple of strings, a top-level name is exported exactly when it's listed. Otherwise a name is exported when it doesn't start with `_`; dunder names like `__eq__` count as public. Methods are exported when their class is and their own name passes the same rule.
- **Classes:** fields come from annotated or plain class-body assignments, `self.x = …` assignments in `__init__`, `@property` methods (as properties) and other methods (as methods). This covers dataclasses, pydantic and attrs models, `TypedDict` and `NamedTuple` the same way. `__init__` itself is not a field or a function record.
- **Enums:** subclasses of `Enum`, `IntEnum`, `StrEnum`, `Flag` and `IntFlag` have kind `enum`, and their class-body assignments are the members.
- **Aliases:** `type X = …` and `X: TypeAlias = …`.
- **Scope:** module level and `if TYPE_CHECKING:` blocks; functions nested in functions are skipped.
