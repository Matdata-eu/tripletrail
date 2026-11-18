// The parts of CodeMirror 6 the Workbench uses, bundled into one file (see build.sh).
export { EditorView, keymap, lineNumbers, highlightActiveLine, highlightActiveLineGutter, drawSelection, hoverTooltip, Decoration, ViewPlugin, placeholder } from "@codemirror/view";
export { EditorState, Compartment, StateEffect, StateField } from "@codemirror/state";
export { defaultKeymap, history, historyKeymap, indentWithTab, toggleComment } from "@codemirror/commands";
export { StreamLanguage, syntaxHighlighting, defaultHighlightStyle, HighlightStyle, bracketMatching, indentOnInput, foldGutter, foldKeymap } from "@codemirror/language";
export { autocompletion, completionKeymap, closeBrackets, closeBracketsKeymap, snippetCompletion } from "@codemirror/autocomplete";
export { linter, lintGutter, setDiagnostics, lintKeymap } from "@codemirror/lint";
export { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
export { tags } from "@lezer/highlight";
export { turtle } from "@codemirror/legacy-modes/mode/turtle";
export { sparql } from "@codemirror/legacy-modes/mode/sparql";
export { yaml } from "@codemirror/legacy-modes/mode/yaml";
