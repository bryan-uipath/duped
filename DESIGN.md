# duped design

`duped` finds duplicated functions and types across a codebase and reports them as evidence for a human or an agent to review. It parses syntax only (no compiler, no embeddings), so a whole-monorepo pass should take seconds.

## Goals

- List every top-level function signature and every top-level type in a repo.
- Find likely duplicates. That means same name or similar name, and, more importantly, different names with overlapping properties or signatures.
- Say whether each finding is actionable: can the copy import the original today, or would the type have to move?
- Ship skills that adapt the tool to a repo and help interpret the results.
- Support TypeScript/JavaScript, Python, C# and Rust.

## Non-goals (v1)

- Resolved types from a compiler or language server. That could be an optional add-on later.
- Embeddings or any model download.
- Automatically refactoring code. Output is evidence; a person decides.

## Lessons that shape the design

These come from a manual duplication audit of a large TypeScript monorepo.

1. **Matching by name isn't enough.** Name matching found about 25 candidates. Property overlap found 63 high-confidence pairs, most of them with different names. Both are needed.
2. **Dependency direction decides what to do.** "The copy can import the original today" and "the type would need to move to a lower package" are different jobs. Tagging each finding with that, using the repo's module graph, is the most useful feature.
3. **Same name, different shape is its own finding.** Two examples: a redeclared status union drifted three members behind the original, and two functions with the same name produced different output for the same input. Count union and enum members as fields so subset and superset drift shows up.
4. **The noise is predictable.** It comes from tests and fixtures, tiny shapes like `{ id, name }`, UI prop types, and pairs inside one module. Defaults should filter these, and a per-repo config should tune them. Path filters must be relative to the repo root.
5. **Inherited and inline types matter.** Count `extends` and base-class fields (best-effort, matched by name). Extract inline object types (casts, anonymous parameter shapes) as anonymous shapes keyed by location.
6. **Some duplicates are deliberate.** Comments like "mirrors X" or "structural twin of Y", and an allowlist in the repo config, should mark pairs as acknowledged so they stop being reported.
7. **Every finding needs both sides read.** The skills enforce that verification step.

## Architecture

```
source files ──► language extractor (tree-sitter) ──► shared records (JSON Lines) ──► analyses ──► reports
                                                                    ▲
                                                 duped.toml + module graph
```

### Parsing

[tree-sitter](https://tree-sitter.github.io/) has maintained grammars for every target language. Each language is a set of tree-sitter queries plus a small mapping into the shared records:

| Language | Functions | Types and their fields |
| --- | --- | --- |
| TypeScript / JavaScript | function declarations, exported arrow functions, class methods | interfaces, object type aliases, classes, string-literal unions, enums |
| Python | `def`, methods | dataclasses, `TypedDict`, pydantic models, `NamedTuple`, `Enum`, classes with annotated attributes |
| C# | methods, local functions | classes, records, structs and interfaces (properties and fields), enums |
| Rust | `fn`, impl and trait methods | structs (fields), enums (variants), traits (methods) |

### Shared records

One JSON object per line. Every analysis reads these, so only the extractors are language-specific.

- **function:** name, enclosing scope, parameters (name and type text; a destructured object also lists its keys), return type text, visibility, file, line span, doc comment, and body tokens (in memory only, not in the JSON).
- **type:** name, kind, fields (name, type text, optional), extends/implements, visibility, file, line span, doc comment.

### Analyses

- **types:** Jaccard similarity on field names, plus a stricter score that also compares field type text. Pairs are clustered with union-find.
- **fns:** Jaccard similarity of parameter names, weighted by how rare each name is, plus a stricter score that also compares parameter and return types. Conventions (a shared name set on many functions), implementations of one base, and wrappers are filtered.
- **names:** file pairs that declare several of the same top-level names, scored by name rarity, export and matching signature, with each name's shape or signature marked as matching or diverging.
- **bodies:** functions whose bodies are near-copies: Jaccard similarity of token shingles, with identifiers abstracted and literals kept, and MinHash + LSH banding to find candidate pairs. Exact hashes miss ported code with small edits. Pairs are ranked by shared shingles. File pairs linked through two or more functions on each side are reported as copied modules.
- **imports:** files whose imports overlap on rare items (IDF-weighted Jaccard over `specifier#name`), for ported copies whose declarations were all renamed. It reads import statements itself rather than adding a record kind: imports aren't declarations, and no other analysis needs them. TypeScript/JavaScript only.

### Module graph and actionability

Modules are detected from manifests under the project root: pnpm/npm/yarn workspaces and `package.json` files, Cargo workspaces and crates, `pyproject.toml` (PEP 621, Poetry, uv workspaces), and `.csproj` files with their `ProjectReference`s. Every dependency kind counts, including dev and peer dependencies. `duped.toml` can define, relocate, rewire or ignore modules. Each pair is tagged:

- **importable:** the copy's module already depends on the original's module.
- **importable-indirect:** it depends on it only through other modules, so a direct dependency is needed first.
- **move-down:** both depend on a common lower module, so the shared type could live there.
- **boundary:** no dependency path either way (for example, a host-injected package). Often deliberate.
- **same-module:** both in one module; hidden by default.

Each cluster also gets a suggested home: a member's module that every other member can import, or else the lowest module they all depend on.

## CLI

| Command | Output |
| --- | --- |
| `duped extract [PATH]` | the shared records for all top-level functions and types |
| `duped index [PATH]` | a greppable markdown summary for agents |
| `duped types [PATH]` | type pairs or clusters by property overlap |
| `duped fns [PATH]` | function pairs by signature similarity |
| `duped names [PATH]` | same-name collisions, including ones with divergent shapes |
| `duped bodies [PATH]` | functions with near-copy bodies, and files that share several |
| `duped imports [PATH]` | file pairs that import the same rare things |
| `duped report [PATH]` | `bodies`, `imports`, `names` and `types` pairs grouped by file pair, ranked by how many analyses agree |

All but `fns` are implemented. Every analysis supports `--json`. `report --base <ref>` keeps only file pairs with a side the diff since the merge base touches; the other commands don't take `--base`.

## Per-repo config: `duped.toml`

- Module overrides: `path`, `deps`, `ignore`.
- Include/exclude globs.
- Thresholds: minimum fields, minimum shared fields, similarity.
- Acknowledged pairs, each with a reason. Doc comments that say a type mirrors or is a structural twin of the other side count too.
- Per-language rules: conventional members that don't count toward similarity (such as `toString`), and test-file and test-directory patterns.

## Skills

Shipped in `skills/`, installable as a Claude Code plugin or copied into any agent's skills directory.

- **duped:** how to use the CLI, check for prior art before writing code, read a finding, and record deliberate copies.
- **duped-audit:** whole-repo scan; reads both sides of each finding and ranks a cleanup list by payoff.
- **duped-review:** for a branch or pull request, flags changed files that copy existing code, and copies that have drifted.

Audit and review run `duped report`. For now `duped-audit` suggests `duped.toml` entries; a separate setup skill can follow.

## Roadmap

Each step ships on its own.

1. Scaffold, shared records, TypeScript/JavaScript extractor, `extract` and `index`.
2. `types`, with noise filters.
3. Module graph, actionability tags, `duped.toml`.
4. Python extractor.
5. C# extractor.
6. Rust extractor.
7. `fns` (prototype: signatures only) and `names` (file pairs, prototype), including divergent-shape reporting.
8. `bodies`: fuzzy matching for TypeScript/JavaScript and Rust (prototype). Next: Python and C# tokenizers and a `[bodies]` section in `duped.toml`.
9. Skills.
10. Regression suite: reproduce the findings of the original monorepo audit.
11. `imports` (prototype): rare-import fingerprints for TypeScript/JavaScript files.
12. `report`: every analysis grouped by file pair, diff-filtered with `--base`.
