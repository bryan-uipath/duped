# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript and JavaScript are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

## Build

```sh
cargo build --release
```

## Usage

```sh
duped extract [DIR] [--out records.jsonl]   # every top-level function and type, as JSON Lines
duped index [DIR] [--out api-index.md]      # greppable markdown summary, one section per file
```

Both skip test, mock and fixture files unless you pass `--include-tests`, and both accept `--exclude <glob>` (repeatable, relative to `DIR`). The walk respects `.gitignore` and skips `node_modules`, `dist`, `build`, `out`, `coverage` and `target`.

Each JSON Lines row is a `function` (name, scope, params, return type, exported, location, doc) or a `type` (name, kind, fields, extends, exported, location, doc). Type kinds are `interface`, `type`, `class`, `enum`, `union` (literal members become fields) and `alias`.
