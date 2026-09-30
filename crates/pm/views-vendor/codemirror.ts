// The CodeMirror half of the pm ticket editor's vendored code (AGT-1403):
// CodeMirror 6 and loro-codemirror, bundled into one module so there is
// exactly one @codemirror/state instance. `loro-crdt` stays an import of
// the sibling `./loro.js` (see build.ts), so the editor and the view share
// one Loro wasm instance and `LoroDoc` class.
export { EditorState } from "@codemirror/state";
export { EditorView, keymap, drawSelection, placeholder } from "@codemirror/view";
export { defaultKeymap, indentWithTab } from "@codemirror/commands";
export { LoroExtensions } from "loro-codemirror";
