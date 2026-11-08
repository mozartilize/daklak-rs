// Editors for browser_page.py. Each mounts into a host element and calls
// `onSubmit({model, dom})` on Enter, then clears itself the way a chat
// composer does after sending:
//   model  the editor's own document text (what a chat app would send)
//   dom    the text the user sees in the page
// A difference means the editor and the page went out of sync even if the
// visible text looks right.

import { EditorState, Plugin } from "prosemirror-state";
import { EditorView, Decoration, DecorationSet } from "prosemirror-view";
import { schema } from "prosemirror-schema-basic";
import { keymap } from "prosemirror-keymap";
import { baseKeymap } from "prosemirror-commands";
import { history, undo, redo } from "prosemirror-history";

import {
  createEditor,
  $getRoot,
  $createParagraphNode,
  KEY_ENTER_COMMAND,
  COMMAND_PRIORITY_CRITICAL,
} from "lexical";
import { registerRichText } from "@lexical/rich-text";
import { createEmptyHistoryState, registerHistory } from "@lexical/history";

function domText(el) {
  return el.innerText.replace(/\u00a0/g, " ").replace(/\n+$/, "");
}

// ── ProseMirror: claude.ai (Tiptap) and ChatGPT composers ────────────────
// Paragraph blocks, ProseMirror's trailing <br>, an empty-document
// placeholder decoration, history and the base keymap.

function placeholder(text) {
  return new Plugin({
    props: {
      decorations(state) {
        const doc = state.doc;
        if (doc.childCount !== 1 || doc.firstChild.content.size !== 0) return null;
        return DecorationSet.create(doc, [
          Decoration.node(0, doc.firstChild.nodeSize, {
            class: "is-empty",
            "data-placeholder": text,
          }),
        ]);
      },
    },
  });
}

function mountProseMirror(host, onSubmit, { trace } = {}) {
  const plugins = () => [
    history(),
    keymap({
      Enter: (state, dispatch, view) => {
        onSubmit({
          model: state.doc.textBetween(0, state.doc.content.size, "\n"),
          dom: domText(view.dom),
        });
        // Chat composers start a fresh document after sending.
        view.updateState(EditorState.create({ schema, plugins: plugins() }));
        return true;
      },
      "Mod-z": undo,
      "Mod-Shift-z": redo,
    }),
    keymap(baseKeymap),
    placeholder("Message…"),
    ...(trace ? [new Plugin({
      view: () => ({ update: (view) => trace("prosemirror-update", {
        model: view.state.doc.textBetween(0, view.state.doc.content.size, "\n").slice(0, 1024),
      }) }),
    })] : []),
  ];
  const view = new EditorView(host, {
    state: EditorState.create({ schema, plugins: plugins() }),
  });
  view.focus();
  return {
    root: view.dom,
    ready: Promise.resolve(),
    read: () => ({
      model: view.state.doc.textBetween(0, view.state.doc.content.size, "\n"),
      dom: domText(view.dom),
    }),
  };
}

// ── Lexical: Facebook / Messenger composers ──────────────────────────────
// Rich-text plugin (beforeinput-driven), history, Enter intercepted.

function mountLexical(host, onSubmit, { trace, error } = {}) {
  host.contentEditable = "true";
  const editor = createEditor({
    namespace: "daklak-e2e",
    onError: (e) => { console.error(e); error?.(e); },
  });
  editor.setRootElement(host);
  registerRichText(editor);
  registerHistory(editor, createEmptyHistoryState(), 300);
  const readModel = () => editor.getEditorState().read(() =>
    $getRoot().getChildren().map((n) => n.getTextContent()).join("\n"));
  if (trace) editor.registerUpdateListener(({ editorState }) => {
    const model = editorState.read(() => $getRoot().getTextContent());
    trace("lexical-update", { model: model.slice(0, 1024) });
  });
  editor.registerCommand(
    KEY_ENTER_COMMAND,
    (event) => {
      if (event) event.preventDefault();
      const model = editor
        .getEditorState()
        .read(() => $getRoot().getChildren().map((n) => n.getTextContent()).join("\n"));
      onSubmit({ model, dom: domText(host) });
      editor.update(() => {
        const root = $getRoot();
        root.clear();
        const p = $createParagraphNode();
        root.append(p);
        p.select();
      });
      return true;
    },
    COMMAND_PRIORITY_CRITICAL
  );
  // Readiness observes the existing initial update's completion; it does not
  // force a synchronous update or insert a warm-up character.
  let resolveReady;
  const ready = new Promise((resolve) => { resolveReady = resolve; });
  editor.update(() => {
    const p = $createParagraphNode();
    $getRoot().append(p);
    p.select();
  }, { onUpdate: resolveReady });
  host.focus();
  return { root: host, ready, read: () => ({ model: readModel(), dom: domText(host) }) };
}

// ── tg-like: hand-written re-rendering composer ──────────────────────────
// Not Telegram's code. Models the class of composer that keeps its own
// text and re-renders the whole field from it after each native edit
// (entity / link highlighting), restoring the caret by character offset.
// Every re-render replaces the text nodes the browser's IME cache refers to.

function caretOffset(root) {
  const sel = window.getSelection();
  if (!sel.rangeCount) return null;
  const r = sel.getRangeAt(0).cloneRange();
  r.selectNodeContents(root);
  r.setEnd(sel.focusNode, sel.focusOffset);
  return r.toString().length;
}

function setCaret(root, offset) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node;
  let left = offset;
  while ((node = walker.nextNode())) {
    if (left <= node.length) {
      const sel = window.getSelection();
      sel.collapse(node, left);
      return;
    }
    left -= node.length;
  }
  const sel = window.getSelection();
  sel.selectAllChildren(root);
  sel.collapseToEnd();
}

function renderEntities(text) {
  const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  // Wrap each word in its own span and links / code in entity elements, so
  // most edits change the node structure.
  return text
    .split(/(\s+)/)
    .map((part) => {
      if (/^\s+$/.test(part)) return esc(part).replace(/ /g, "\u00a0");
      if (/^https?:\/\//.test(part)) return `<a href="#">${esc(part)}</a>`;
      if (/^`.*`$/.test(part) && part.length > 1) return `<code>${esc(part)}</code>`;
      return part ? `<span class="w">${esc(part)}</span>` : "";
    })
    .join("");
}

function mountTgLike(host, onSubmit, { trace } = {}) {
  host.contentEditable = "true";
  let model = "";
  let composing = false;
  let scheduled = false;

  const rerender = () => {
    scheduled = false;
    if (composing) return;
    const caret = caretOffset(host);
    model = host.textContent.replace(/\u00a0/g, " ");
    const html = renderEntities(model);
    if (host.innerHTML !== html) {
      host.innerHTML = html;
      if (caret !== null) setCaret(host, caret);
    }
    trace?.("tg-like-render", { model: model.slice(0, 1024) });
  };
  host.addEventListener("compositionstart", () => (composing = true));
  host.addEventListener("compositionend", () => {
    composing = false;
    rerender();
  });
  host.addEventListener("input", () => {
    // Re-render on the next frame, like a framework batching updates.
    if (!scheduled) {
      scheduled = true;
      requestAnimationFrame(rerender);
    }
  });
  host.addEventListener("keydown", (ev) => {
    if (ev.key !== "Enter" || ev.isComposing) return;
    ev.preventDefault();
    rerender();
    onSubmit({ model, dom: domText(host) });
    model = "";
    host.innerHTML = "";
  });
  host.focus();
  return {
    root: host,
    ready: Promise.resolve(),
    read: () => ({ model, dom: domText(host) }),
  };
}

window.daklakEditors = {
  prosemirror: mountProseMirror,
  lexical: mountLexical,
  "tg-like": mountTgLike,
};
