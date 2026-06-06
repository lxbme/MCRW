# MCRW Editor Support (Lua type definitions)

This directory ships type definitions for the MCRW Lua plugin API so your
editor can offer **autocomplete, hover documentation, and type checking**
while you write plugins.

It works with the [Lua Language Server][luals] — the engine behind the VS Code
**"Lua"** extension (by sumneko), and also usable from Neovim, Helix, etc.

| File                  | What it is                                                                 |
|-----------------------|---------------------------------------------------------------------------|
| `mcrw.lua`            | `---@meta` definitions for `Server`, the `wrapper` handle, callbacks, and data types. Documentation only — never executed at runtime. This is the canonical, version-controlled copy; keep it in sync with `src/lua_ctx.rs`. |
| `luarc.example.json`  | A template `.luarc.json` to drop into your plugin workspace.              |

## Setup

1. Install the Lua Language Server (e.g. the VS Code "Lua" extension).
2. Open your **plugins directory** as the editor's workspace root — the folder
   that contains your plugin subdirectories (in this repo, `server/lua_plugins/`).
3. Add a `.luarc.json` to that folder. Copy `luarc.example.json` and set
   `workspace.library` to point at *this* directory (`tools/lua-types`).
   A relative path works and is preferred — from `server/lua_plugins/` that is:

   ```json
   "workspace.library": ["../../tools/lua-types"]
   ```

   (This repo already ships exactly that file at
   `server/lua_plugins/.luarc.json`; it is git-ignored along with the rest of
   `server/`, so it is purely local convenience.)
4. Reload your editor. Typing `wrapper:` now completes `register`,
   `register_cron`, `run_python`, etc., with inline docs.

`wrapper` is typed automatically because `Server:get_context(...)` is annotated
to return the `wrapper` handle:

```lua
local wrapper = Server:get_context(...)  --> typed as mcrw.Wrapper
wrapper:register(...)                     --> completion + signature help
```

## Keeping it current

These definitions mirror the Rust API in `src/lua_ctx.rs`. When you add,
remove, or change a method exposed to Lua, update `mcrw.lua` to match.

[luals]: https://luals.github.io/
