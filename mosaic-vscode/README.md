# Mosaic for VS Code

VS Code extension that surfaces [Mosaic](../README.md) inside the editor.

## What it does

- Launches the `mosaic-lsp` language server in the background so every
  open file is tracked in a Mosaic CRDT shadow.
- Adds five commands to the palette:
  - **Mosaic: Commit current file** — prompts for intent, saves the
    file, and writes one signed atomic Change.
  - **Mosaic: Show change log** — recent changes on the default branch.
  - **Mosaic: List branches** — branch frontiers.
  - **Mosaic: Show repository status** — identity + branch state + open
    files.
  - **Mosaic: Open web dashboard** — jumps to your `mosaic-serve`.
- Status bar entry (`$(git-commit) Mosaic`) acts as a quick status
  shortcut.
- Hovering anywhere in a file shows repo metadata (branches, change
  count on `main`) coming from the LSP.

## Install for local development

```bash
cd mosaic-vscode
npm install
npm run build
```

In VS Code: open this folder and press F5 (the "Extension Development
Host" launches a new window with the extension loaded).

For non-developers: package with `npx vsce package` (produces a `.vsix`
file) and install via "Extensions: Install from VSIX...".

## Settings

| Setting                | Default        | Effect                                          |
| ---------------------- | -------------- | ----------------------------------------------- |
| `mosaic.lspPath`       | `mosaic-lsp`   | Path to the LSP binary (PATH-resolved).         |
| `mosaic.server`        | `""`           | URL of a `mosaic-serve` instance for "Open web dashboard". |
| `mosaic.defaultBranch` | `main`         | Branch used by Commit / Push.                   |

## How it talks to Mosaic

The extension uses `vscode-languageclient/node` to spawn `mosaic-lsp`
over stdio. Every command is a thin wrapper around an LSP
`workspace/executeCommand` request (the same surface other LSP-aware
editors can drive). Nothing VS-Code-specific lives in the protocol; the
editor is just one of many possible front-ends.
