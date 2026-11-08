// Node's built-in runner; DOM doubles exercise orchestration, not IME behavior.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture(trace) {
  class Input {
    constructor() {
      this.value = '';
      this.innerHTML = '';
      this.isConnected = true;
      this.selectionStart = this.selectionEnd = 0;
      this.listeners = new Map();
    }
    contains(node) { return node === this; }
    addEventListener(type, listener) {
      if (!this.listeners.has(type)) this.listeners.set(type, []);
      this.listeners.get(type).push(listener);
    }
  }
  const root = new Input();
  const frames = [], timers = [], posts = [];
  let focused = true, ms = 0;
  const document = {
    hasFocus: () => focused,
    activeElement: root,
    addEventListener() {},
  };
  const window = { addEventListener() {} };
  const context = vm.createContext({
    window, document, HTMLInputElement: Input, HTMLTextAreaElement: class {},
    MutationObserver: class { observe() {} },
    performance: { now: () => ++ms, timeOrigin: 1000000 },
    requestAnimationFrame: (callback) => frames.push(callback),
    setTimeout: (callback) => timers.push(callback),
    queueMicrotask,
    fetch: async (url, options) => {
      posts.push({ url, body: JSON.parse(options.body) });
      return { ok: true };
    },
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'browser_harness.js'), 'utf8'), context);
  const harness = window.createDaklakHarness({ trace });
  const api = { root, ready: Promise.resolve(), read: () => ({ model: root.value, dom: root.value }) };
  async function flush() {
    // Drain promises/animation frames without allowing an infinite polling loop.
    for (let i = 0; i < 12; i++) {
      await new Promise(setImmediate);
      while (frames.length) frames.shift()();
    }
  }
  return { harness, api, root, posts, timers, document, flush,
           setFocus: (value) => { focused = value; } };
}

test('readiness waits for initial model completion and real focus; no warm-up', async () => {
  const f = fixture(false);
  let resolve;
  f.api.ready = new Promise((done) => { resolve = done; });
  const mounting = f.harness.mount(f.api);
  await f.flush();
  assert.equal(f.posts.some((p) => p.url === '/status' && p.body.ready), false,
               'focus alone cannot make an uncommitted model ready');
  f.setFocus(false);
  resolve();
  await f.flush();
  assert.equal(f.posts.at(-1).body.ready, false);
  assert.equal(f.root.value, '', 'readiness never types a warm-up');
  f.setFocus(true);
  f.timers.shift()();
  await f.flush();
  await mounting;
  assert.equal(f.posts.at(-1).body.ready, true);
  assert.equal(f.posts.filter((p) => p.url === '/line').length, 0);
});

test('focused read-only input is not ready for typing', async () => {
  const f = fixture(false);
  f.root.readOnly = true;
  const mounting = f.harness.mount(f.api);
  await f.flush();
  assert.equal(f.posts.at(-1).body.ready, false);
  assert.equal(f.posts.at(-1).body.state.focused, true);
  assert.equal(f.posts.at(-1).body.state.editable, false);
  f.root.readOnly = false;
  f.timers.shift()();
  await f.flush();
  await mounting;
  assert.equal(f.posts.at(-1).body.ready, true);
  assert.equal(f.root.value, '');
});

test('trace is bounded, reports truncation, and includes before/after clear', async () => {
  const f = fixture(true);
  const mounting = f.harness.mount(f.api);
  await f.flush();
  await mounting;
  f.root.value = 'a'.repeat(2000);
  f.root.selectionStart = f.root.selectionEnd = 2000;
  for (let i = 0; i < 300; i++) f.harness.trace('marker', { tag: i });
  const beforeClear = f.harness.beforeClear();
  f.harness.submit({ model: f.root.value, dom: f.root.value }, beforeClear);
  f.root.value = '';
  f.root.selectionStart = f.root.selectionEnd = 0;
  f.harness.trace('editor-cleared');
  await f.flush();
  const first = f.posts.find((p) => p.url === '/line').body;
  assert.equal(first.iteration, 0);
  assert.equal(first.trace.timeOrigin, 1000000, 'events can be aligned to wall-clock logs');
  assert.equal(first.model.length, 2000, 'actual result is not truncated');
  assert.ok(first.trace.events.length > 0 && first.trace.events.length <= 256);
  assert.ok(first.trace.dropped > 0);
  assert.ok(first.trace.events.every((event) => event.iteration === 0));
  assert.ok(first.trace.events.some((event) => event.tag === 299));
  assert.ok(!first.trace.events.some((event) => event.tag === 0));
  assert.ok(first.trace.beforeClear.model.length > 0 && first.trace.beforeClear.model.length <= 1024);
  assert.equal(first.trace.beforeClear.modelLength, 2000);
  assert.equal(first.trace.afterClear.model, '');
  assert.ok(first.trace.events.some((event) => event.type === 'editor-cleared'));

  f.root.value = 'next';
  f.harness.trace('next-line');
  f.harness.submit({ model: 'next', dom: 'next' });
  f.root.value = '';
  await f.flush();
  const second = f.posts.filter((p) => p.url === '/line')[1].body;
  assert.equal(second.iteration, 1);
  assert.ok(second.trace.events.every((event) => event.iteration === 1));
  assert.ok(!second.trace.events.some((event) => event.type === 'marker'));
});

test('disabled tracing numbers submissions without adding trace payloads', async () => {
  const f = fixture(false);
  const mounting = f.harness.mount(f.api);
  await f.flush();
  await mounting;
  assert.equal(f.harness.trace, undefined);
  f.harness.submit({ model: 'first', dom: 'first' });
  await f.flush();
  f.harness.submit({ model: 'second', dom: 'second' });
  await f.flush();
  const reports = f.posts.filter((p) => p.url === '/line');
  assert.equal(reports.length, 2);
  assert.equal(reports[0].body.iteration, 0);
  assert.equal(reports[1].body.iteration, 1);
  assert.equal('trace' in reports[0].body, false);
});

test('event observers never cancel input, and record editor preventDefault separately', async () => {
  const f = fixture(true);
  const mounting = f.harness.mount(f.api);
  await f.flush();
  await mounting;
  let prevented = 0;
  const event = { inputType: 'insertText', data: 'ư', defaultPrevented: false,
                  preventDefault: () => prevented++ };
  f.root.listeners.get('beforeinput')[0](event);
  // Simulate the editor's own handler, after the capture-phase observer.
  event.defaultPrevented = true;
  f.root.value = 'ư';
  await f.flush();
  f.harness.submit({ model: 'ư', dom: 'ư' });
  f.root.value = '';
  await f.flush();
  const events = f.posts.find((p) => p.url === '/line').body.trace.events;
  assert.equal(prevented, 0);
  assert.equal(events.find((e) => e.type === 'beforeinput').defaultPrevented, false);
  assert.equal(events.find((e) => e.type === 'beforeinput:after').defaultPrevented, true);
});

test('a locations-only Firefox stack does not replace the error message', async () => {
  const f = fixture(false);
  f.harness.error({ message: 'editor failed', stack: '@http://localhost/editor.js:1:7' });
  await f.flush();
  const error = f.posts.find((p) => p.url === '/error').body;
  assert.match(error.message, /editor failed/);
  assert.match(error.message, /editor\.js:1:7/);
});

test('initialization errors are reported without needing a readable model', async () => {
  const f = fixture(true);
  f.api.ready = Promise.reject(new Error('initialization failed'));
  f.api.read = () => { throw new Error('model unavailable'); };
  await f.harness.mount(f.api).catch(f.harness.error);
  await f.flush();
  const error = f.posts.find((p) => p.url === '/error').body;
  assert.match(error.message, /initialization failed/);
  assert.equal(error.state.mounted, true);
  assert.equal(error.state.settled, false);
  assert.equal(f.posts.some((p) => p.url === '/status' && p.body.ready), false);
});
