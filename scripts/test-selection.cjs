const { readFileSync } = require('node:fs');
const { runInNewContext } = require('node:vm');
const assert = require('node:assert/strict');
const html = readFileSync(require('node:path').join(__dirname, '../ui/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
new Function(script); // Check the entire embedded script's syntax.
const elements = new Map();
const $ = id => {
  if (!elements.has(id)) elements.set(id, { lastChild: {}, onclick: null });
  return elements.get(id);
};
const app = {
  selected: new Set(), anchor: null, visible: ['a', 'b'],
  torrents: new Map([['a', { id: 'a', state: 'downloading' }], ['b', { id: 'b', state: 'paused' }], ['c', { id: 'c', state: 'downloading' }]])
};
let calls;
const context = { app, $, render() {}, act(ids, action) { calls = { ids: Array.from(ids), action }; } };
runInNewContext(script.slice(script.indexOf('function targets()'), script.indexOf('// ---------- panel de detalle')), context);
runInNewContext(script.slice(script.indexOf('function select(id'), script.indexOf("$('rows').onclick")), context);
runInNewContext(script.slice(script.indexOf('function bulk(action)'), script.indexOf("$('resumeBtn').onclick")), context);
context.select('a', { ctrlKey: true });
context.select('b', { ctrlKey: true });
assert.deepEqual([...app.selected], ['a', 'b']);
context.renderToolbar();
assert.equal($('selectAll').checked, true);
assert.equal($('pauseBtn').lastChild.textContent, 'Pausar (2)');
assert.equal($('removeBtn').disabled, false);
context.bulk('pause');
assert.deepEqual(calls, { ids: ['a'], action: 'pause' });
context.bulk('resume');
assert.deepEqual(calls, { ids: ['b'], action: 'resume' });
context.select('a', { ctrlKey: true });
context.renderToolbar();
assert.equal($('selectAll').indeterminate, true);
// Selecting visible results must preserve selections outside the filter.
app.selected.add('c');
$('selectAll').onchange({ target: { checked: true } });
assert.deepEqual([...app.selected].sort(), ['a', 'b', 'c']);
$('selectAll').onchange({ target: { checked: false } });
assert.deepEqual([...app.selected], ['c']);
context.clearSelection();
context.select('a');
context.select('b', { shiftKey: true });
assert.deepEqual([...app.selected], ['a', 'b']);
context.clearSelection();
app.visible = [];
context.renderToolbar();
assert.equal($('selectAll').disabled, true);
assert.equal($('selectAll').checked, false);
assert.equal($('clearSelection').hidden, true);
// Removal must confirm the whole selection and default to keeping downloaded files.
let dialogOpened = false;
$('dlgRemove').showModal = () => { dialogOpened = true; };
$('rmNo').focus = () => {};
runInNewContext(script.slice(script.indexOf('let rmIds = []'), script.indexOf("$('rmNo').onclick")), context);
$('rmFiles').checked = true;
context.confirmRemove(['a', 'b']);
assert.equal(dialogOpened, true);
assert.equal($('rmTitle').textContent, 'Quitar 2 torrents');
assert.equal($('rmFiles').checked, false);
console.log('Selection, toolbar, filtered selection, bulk actions and removal confirmation passed.');
