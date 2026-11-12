// What the studio page uses of CodeMirror 6, as one module: `npm run build`
// writes it to ../src/studio/codemirror.js, which mtx embeds and serves at
// /studio/codemirror.js. Rebuild after changing this file or the versions.
export { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection, Decoration, GutterMarker, gutter } from "@codemirror/view";
export { EditorState, Compartment, StateEffect, StateField, RangeSet } from "@codemirror/state";
export { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
export { HighlightStyle, syntaxHighlighting, indentOnInput, bracketMatching, foldGutter, indentUnit, StreamLanguage } from "@codemirror/language";
export { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
export { searchKeymap, highlightSelectionMatches, search } from "@codemirror/search";
export { setDiagnostics, lintGutter } from "@codemirror/lint";
export { rust } from "@codemirror/lang-rust";
export { json } from "@codemirror/lang-json";
export { toml } from "@codemirror/legacy-modes/mode/toml";
export { tags } from "@lezer/highlight";
