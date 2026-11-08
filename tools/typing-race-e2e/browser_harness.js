// Observational harness only: never inserts text, moves the caret, or warms
// up an editor. Reports use iteration IDs rather than HTTP arrival order.
window.createDaklakHarness = function ({ trace = false } = {}) {
  const limit = 256;
  const clip = (value) => String(value ?? "").slice(0, 1024);
  let api;
  let settled = false;
  let iteration = 0;
  let version = 0;
  let finishing = false;
  let events = [];
  let dropped = 0;

  function snapshot(includeText = trace) {
    if (!api) return { mounted: false, settled: false };
    const root = api.root;
    const active = document.activeElement;
    const native = root instanceof HTMLInputElement || root instanceof HTMLTextAreaElement;
    const facts = {
      mounted: root.isConnected,
      settled,
      editable: !!(root.isContentEditable || (native && !root.disabled && !root.readOnly)),
      focused: !!(document.hasFocus() && (native ? active === root :
        root.contains(active) && active?.isContentEditable)),
    };
    // A pending model may not be readable yet. Still expose mount/focus facts
    // so a startup timeout is distinguishable from an unloaded page.
    if (!settled) return facts;
    let selection;
    if (native) {
      selection = { anchor: root.selectionStart, focus: root.selectionEnd };
    } else {
      const sel = window.getSelection();
      const offset = (node, end) => {
        if (!node || !root.contains(node)) return null;
        const range = document.createRange();
        range.selectNodeContents(root);
        range.setEnd(node, end);
        return range.toString().length;
      };
      selection = {
        anchor: offset(sel?.anchorNode, sel?.anchorOffset),
        focus: offset(sel?.focusNode, sel?.focusOffset),
      };
    }
    const state = { ...facts, selection };
    if (!includeText) return state;
    const field = api.read();
    return {
      ...state,
      model: clip(field.model),
      dom: clip(field.dom),
      html: clip(root.innerHTML),
      modelLength: field.model.length,
      domLength: field.dom.length,
    };
  }

  function record(type, detail = {}) {
    if (!trace) return;
    if (events.length === limit) {
      events.shift();
      dropped++;
    }
    events.push({ iteration, ms: performance.now(), type, ...detail, state: snapshot() });
  }

  function post(path, body) {
    return fetch(path, { method: "POST", body: JSON.stringify(body) }).then((response) => {
      if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
    });
  }

  function error(error) {
    // Firefox can provide a stack containing locations but no message.
    const message = [error?.message || String(error), error?.stack].filter(Boolean).join("\n");
    // Do not recursively try reporting a failed diagnostics request.
    let state;
    try { state = snapshot(); }
    catch (failure) { state = { diagnosticError: String(failure) }; }
    post("/error", { iteration, message, state, events, dropped,
                     timeOrigin: performance.timeOrigin }).catch(() => {});
  }
  window.addEventListener("error", (event) => error(event.error || event.message ||
    `failed to load ${event.target?.src || "browser script"}`), true);
  window.addEventListener("unhandledrejection", (event) => error(event.reason));

  const frame = () => new Promise((resolve) => requestAnimationFrame(resolve));
  async function status() {
    const state = snapshot(false);
    const ready = state.mounted && state.settled && state.editable && state.focused &&
      state.selection.anchor !== null && state.selection.focus !== null;
    await post("/status", { version: version++, ready: !!ready, state });
    return ready;
  }

  async function mount(editor) {
    api = editor;
    status().catch(error);
    if (trace) {
      for (const type of ["keydown", "keyup", "beforeinput", "input",
                          "compositionstart", "compositionupdate", "compositionend",
                          "focusin", "focusout"]) {
        api.root.addEventListener(type, (event) => {
          const detail = {
            key: event.key, inputType: event.inputType, data: clip(event.data),
            isComposing: event.isComposing, defaultPrevented: event.defaultPrevented,
          };
          record(type, detail);
          // Observe the settled outcome after editor handlers; never cancel an event.
          queueMicrotask(() => record(`${type}:after`, {
            ...detail, defaultPrevented: event.defaultPrevented,
          }));
        }, true);
      }
      document.addEventListener("selectionchange", () => record("selectionchange"));
      new MutationObserver(() => record("dom-mutation"))
        .observe(api.root, { subtree: true, childList: true, characterData: true });
    }
    await api.ready;
    await frame();
    await frame();
    settled = true;
    record("mounted");
    // The window may not have focus yet. Never grab focus ourselves to hide that.
    while (!await status()) await new Promise((resolve) => setTimeout(resolve, 100));
    api.root.addEventListener("focusout", () => queueMicrotask(() => status().catch(error)));
    api.root.addEventListener("focusin", () => queueMicrotask(() => status().catch(error)));
  }

  function submit(report, beforeClear) {
    if (finishing) {
      error(new Error("overlapping Enter submissions"));
      return;
    }
    finishing = true;
    record("submit");
    const payload = { ...report, iteration };
    const before = trace ? (beforeClear || snapshot()) : undefined;
    const finish = () => {
      if (trace) {
        record("after-clear");
        payload.trace = { limit, dropped, events, beforeClear: before,
                          afterClear: snapshot(), timeOrigin: performance.timeOrigin };
      }
      iteration++;
      events = [];
      dropped = 0;
      finishing = false;
      post("/line", payload).catch(error);
    };
    // Editor clearing runs at its original time. Only diagnostics delivery is
    // deferred so the trace includes the asynchronous clear and resulting caret.
    if (trace) frame().then(frame).then(finish).catch(error);
    else finish();
  }

  return { mount, submit, beforeClear: trace ? snapshot : () => undefined,
           trace: trace ? record : undefined, error };
};
