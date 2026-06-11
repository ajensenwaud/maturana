# Vendored frontend bundles

Prebuilt ESM bundles committed to the repo so the cockpit needs **no Node
toolchain at build or run time** — everything embeds into the Rust binary via
`include_dir`. Versions are pinned in `VENDOR-VERSIONS`.

- `codemirror/codemirror.bundle.js` — CodeMirror 6 (`basicSetup` + `EditorView`),
  `@codemirror/lang-markdown`, `@codemirror/state` (`Compartment` for the Vim
  toggle), and `@replit/codemirror-vim`.
- `marked/marked.esm.js` — markdown renderer for streamed turn output (copied
  verbatim from the `marked` package's prebuilt ESM).

## Rebuilding the CodeMirror bundle

One-time, in a scratch directory (never committed):

```sh
npm init -y
npm install rollup @rollup/plugin-node-resolve \
  codemirror @codemirror/lang-markdown @codemirror/state @replit/codemirror-vim marked
cat > entry.js <<'EOF'
export { EditorView, basicSetup } from "codemirror";
export { EditorState, Compartment } from "@codemirror/state";
export { keymap } from "@codemirror/view";
export { markdown } from "@codemirror/lang-markdown";
export { vim, getCM, Vim } from "@replit/codemirror-vim";
EOF
npx rollup entry.js --format es --plugin @rollup/plugin-node-resolve \
  --file codemirror.bundle.js
```

Copy `codemirror.bundle.js` here, copy
`node_modules/marked/lib/marked.esm.js` to `marked/`, and update
`VENDOR-VERSIONS` with the exact versions `npm ls --depth=0` reports.
