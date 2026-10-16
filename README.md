# legacy-clonk-ls
Language service for Legacy Clonk

This add semantic syntax highlighting via lsp.
The language server uses tree-sitter to parse the source code.



## C4Script analysis (diagnostics and navigation)

`server/src/analysis` adds semantic support for scripts (`.c`):

- **Diagnostics** modelled on the engine's parser (`C4AulParse.cpp`): syntax errors, `unknown identifier`
  for direct calls and variables that aren't visible from the script (taking `#include`, `#appendto`,
  `global func` and System.c4g into account), the engine's `passing N parameters, but only M are used`
  warning, `#strict` level rules (arrays, `inherited`, `nil`, maps, `.` access, constants called like
  functions), plus lint warnings the engine only notices at runtime (unknown definition IDs, `->` calls
  that no script defines).
- **Hover** with signatures and English descriptions of all engine functions/constants, script functions
  (with their doc comments) and definition IDs.
- **Go to definition**, **references**, **document symbols** and **workspace symbols** across all
  scripts below the workspace root(s) (`initializationOptions.extraRoots` / env `C4LS_EXTRA_ROOTS` add more).
- `c4check`: the same diagnostics on the command line (`c4check --root <dir> [paths]`), e.g. for CI.

Engine data (`server/src/core/engine_api.json`, `system_scripts.json`) is generated from the
LegacyClonk source and lcdocs (English via `lcdocs/en.po`):

    python tools/gen_engine_api.py <path to LegacyClonk/src>

The diagnostics were checked against the engine itself: every script of the original Objects.c4d,
Melees.c4f and System.c4g produces no errors, and a set of test cases compiled by the real engine
reports the same errors/warnings as `c4check`.

### Game packs

Scenarios usually load packs from the game folder (`[Definitions]` in Scenario.txt). With the game folder
configured, the server also indexes `Objects.c4d` and every pack a scenario in the workspace names there
(or all packs with `allPacks`). Packed groups are unpacked once with the game's `c4group` into a cache
(`%LOCALAPPDATA%/legacy-clonk-ls/packs`, re-unpacked when the pack changes); the originals are not touched.

- VS Code: set `legacy-clonk-ext.pathToGameExecutable` (the game folder is taken from it).
- Other clients: `initializationOptions: { "gameDir": "<game folder>", "allPacks": false }` or env `C4LS_GAME_DIR`.
- `c4check --game-dir <game folder> [--all-packs]`.

### Installing the VS Code extension

The GitHub Action `Build VS Code extension (.vsix)` builds the server for Windows and Linux and packages the
extension; download `legacy-clonk-ext-vsix` from the run's artifacts (or the release for `v*` tags) and install
it with `code --install-extension legacy-clonk-ext-<version>.vsix`.
