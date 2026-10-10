const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

// Config, Clients: the named-clients list, run from the dashboard's own sources.
const source = fs.readFileSync(path.join(__dirname, '../ui/config.js'), 'utf8');
const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const line = (start) => app.slice(app.indexOf(start), app.indexOf('\n', app.indexOf(start)));
const helpers = [line("const esc = (v) =>"), line("const HIDDEN = '"), line('const secret = (key) =>'), line('const copyBtn = (text, label, note) =>')].join('\n');

function clients(values, { private: hidden = false } = {}) {
  const context = {
    S: { private: hidden, overview: { client_keys: values['api-keys'] || [], named_clients: (values['named-clients'] || []).length }, config: { values, busy: false } },
    ICON: { copy: '' }, document: { addEventListener() {} }, crypto: globalThis.crypto,
  };
  vm.createContext(context);
  vm.runInContext(`${helpers}\n${source}`, context);
  vm.runInContext('S.config.named = namedIdle()', context);
  return {
    named: context.S.config.named,
    html: () => vm.runInContext('namedClientsHTML()', context),
    // What a person sees, and what a screen reader reads out.
    visible: () => {
      const html = vm.runInContext('namedClientsHTML()', context);
      const labels = [...html.matchAll(/aria-label="([^"]*)"/g)].map((m) => m[1]);
      return `${html.replace(/<[^>]*>/g, ' ')} ${labels.join(' ')}`.replace(/\s+/g, ' ');
    },
    run: (code) => vm.runInContext(code, context),
  };
}

const whombat = { id: 'whombat-cybex', label: 'Whombat · Cybex BV', key: 'wbgw_0123456789abcdefghij' };
const laptop = { id: 'laptop', label: 'Laptop', key: 'fbx_abcdefghijklmnopqrstuvwx' };

test('ids come from the label: lowercase, a-z0-9 and dashes, unique, at most 40', () => {
  const { run } = clients({});
  const id = (label, taken = []) => run(`namedClientId(${JSON.stringify(label)}, ${JSON.stringify(taken)})`);
  assert.equal(id('Whombat · Cybex BV'), 'whombat-cybex-bv');
  assert.equal(id('  Café Büro  '), 'cafe-buro');
  assert.equal(id('!!!'), 'client');
  assert.equal(id('Laptop', ['laptop']), 'laptop-2');
  assert.equal(id('Laptop', ['laptop', 'laptop-2']), 'laptop-3');
  const long = 'A very long name for a tool that goes on and on and on';
  for (const taken of [[], [id(long)]]) {
    const value = id(long, taken);
    assert.ok(value.length <= 40, value);
    assert.match(value, /^[a-z0-9]+(-[a-z0-9]+)*$/);
    assert.ok(!taken.includes(value));
  }
});

test('new keys are fbx_ client keys, never collector keys', () => {
  const { run } = clients({});
  for (let i = 0; i < 20; i++) assert.match(run('generateClientKey()'), /^fbx_[a-z0-9]{24}$/);
});

test('keys are masked until shown, and copy buttons carry the real key', () => {
  const page = clients({ 'api-keys': ['fbx_shared'], 'named-clients': [whombat, laptop] });
  assert.ok(page.visible().includes('Whombat · Cybex BV'));
  assert.ok(page.visible().includes('whombat-cybex'));
  assert.ok(page.visible().includes('wbgw_01…ghij'));
  assert.ok(!page.visible().includes(whombat.key));
  assert.ok(page.html().includes(`data-text="${whombat.key}"`));
  page.named.shown['whombat-cybex'] = true;
  assert.ok(page.visible().includes(whombat.key));
  assert.ok(!page.visible().includes(laptop.key));
});

test('privacy hides labels, ids and keys, even a shown key', () => {
  const page = clients({ 'named-clients': [whombat, laptop] }, { private: true });
  page.named.shown['whombat-cybex'] = true;
  const html = page.visible();
  for (const text of [whombat.label, whombat.id, whombat.key, laptop.key, 'wbgw_01']) assert.ok(!html.includes(text), text);
  assert.ok(html.includes('fbx_••••••••'));
  assert.ok(!page.html().includes('named-reveal'));
  assert.ok(page.html().includes(`data-text="${laptop.key}"`));
});

test('removing the last named client without a shared key warns that keys turn off', () => {
  const alone = clients({ 'api-keys': [], 'named-clients': [whombat] });
  alone.named.confirm = 'whombat-cybex';
  assert.ok(alone.visible().includes('Fusebox will accept requests without a key again.'));
  const shared = clients({ 'api-keys': ['fbx_shared'], 'named-clients': [whombat] });
  shared.named.confirm = 'whombat-cybex';
  assert.ok(shared.visible().includes('Remove Whombat · Cybex BV?'));
  assert.ok(!shared.visible().includes('without a key again'));
  const two = clients({ 'api-keys': [], 'named-clients': [whombat, laptop] });
  two.named.confirm = 'laptop';
  assert.ok(!two.visible().includes('without a key again'));
});

test('adding the first key-giving client says Fusebox will ask for keys', () => {
  const open = clients({ 'api-keys': [], 'named-clients': [] });
  open.named.adding = true;
  open.named.label = 'Whombat · Cybex BV';
  assert.ok(open.visible().includes('After this, Fusebox only accepts requests with a key.'));
  assert.ok(open.visible().includes('Id: whombat-cybex-bv'));
  const keyed = clients({ 'api-keys': ['fbx_shared'], 'named-clients': [] });
  keyed.named.adding = true;
  assert.ok(!keyed.visible().includes('only accepts requests with a key'));
});
