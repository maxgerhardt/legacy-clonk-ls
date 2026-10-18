# Legacy Clonk Extension

Language support for making content (definitions, scenarios, folders) for Legacy Clonk. The language server
follows the engine's own parser and runtime checks (LegacyClonk `C4AulParse.cpp` / `C4AulExec.cpp`); its rules
are compared against the real engine with a set of test scenarios.

Set `legacy-clonk-ext.pathToGameExecutable` (and `pathToC4gExecutable`) so the server can index the game's packs
(Objects.c4d, Knights.c4d, ... whatever a scenario loads in `[Definitions]`).

## Features

- **C4Script** (`.c` inside `.c4d/.c4s/.c4f/.c4g`): semantic highlighting, hover with signatures and docs of
  engine and script functions, go to definition / references / symbols across `#include` and `#appendto`,
  autocomplete (`obj->` offers the object's definition's functions, `m.` map keys, `#` directives, `$` string
  table entries) and signature help, Format Document (rules from a `.c4format` file).
- **Diagnostics like the engine's**: syntax and `#strict` level errors, unknown identifiers, too many parameters,
  `obj->F()` that no script defines, argument types the engine can't convert (`arg-type`), operands it rejects
  (`op-type`: `x += 1` on an uninitialized var in #strict 3), reference parameters given a value, missing
  string table entries.
- **Type inference**: hover shows the type of every variable at that point (`var x: int | nil`), script
  functions' return types, `local`/`static` types from all their assignments, array element types, and which
  definition an object is (`CreateObject(FLNT, ...)`, `FindObject2(Find_ID(FLNT))`, `if (GetID(o) == FLNT)`), so
  `obj->F()` jumps to exactly the function that runs and warns when that definition has none.
- **Inlay hints**: the inferred type after each `var x` (`: object<FLNT> | nil`) and a function's return type;
  turn them off with VS Code's `editor.inlayHints.enabled`.
- **Rename** (F2) of a function's variables, parameters and map keys and of private functions (other names may be
  used by other objects, strings or the engine, so they are refused with the reason).
- **Lint** (shown faded): unused variables, parameters, `local`s and private functions, unreachable code.
- **nil checks** (opt-in): `obj->F()` where obj may be nil (from FindObject2, Contained, Contents, GetCursor,
  ...) and no `if (obj)` / `if (!obj) return;` guards it.
- **DefCore.txt / Scenario.txt / ActMap.txt**: completion of sections, keys and values (flags, IDs, procedures,
  actions, materials, packs, the definition's functions) and hover with the value format and default; data
  generated from the engine source.
- Commands for (un)packing c4group folders and running scenarios in developer mode.

## Settings

| Setting | Default | |
|---|---|---|
| `legacy-clonk-ext.typeChecks` | on | type inference and the type errors (the next ones need it) |
| `legacy-clonk-ext.fieldTypes` | on | types of `local`/`static` variables from every assignment |
| `legacy-clonk-ext.objectIds` | on | which definition an object holds; exact `obj->F()` resolution |
| `legacy-clonk-ext.assumeCreateSucceeds` | on | CreateObject & co. give `object`, not `object \| nil` |
| `legacy-clonk-ext.lint` | on | unused variables/functions, unreachable code |
| `legacy-clonk-ext.nilChecks` | off | warn about calls on objects that may be nil |
| `legacy-clonk-ext.callerInference` | off | slower: parameter types / facts of private functions from their callers |
| `legacy-clonk-ext.stringTableLanguages` | DE, US | languages the string tables must cover |

The server reads them at startup; after a change VS Code offers to reload the window.

## Silencing a warning

A comment on the line (or the line before) silences diagnostics; without a list, all of them:

```c
var unused; // NOLINT(unused-var)
// NOLINTNEXTLINE(nil-call)
FindObject2(Find_ID(FLNT))->Hit();
// c4lint: ignore-file(unused-func)
```

`// c4lint: ignore(...)` and `// c4lint: ignore-next-line(...)` work too, also in `/* */` comments. The light
bulb on a diagnostic offers "Suppress ... on this line" (and "Rename to _x" for an unused variable), and
completion inside `NOLINT(` lists the codes.
