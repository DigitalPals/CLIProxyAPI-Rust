const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const { execFileSync } = require('node:child_process');
const helpers = require('../ui/usage.js');
const source = fs.readFileSync(path.join(__dirname, '../ui/usage.js'), 'utf8');

function dashboard(extra = {}) {
  const context = {
    Intl, Date, URLSearchParams, setTimeout() {},
    S: { private: false, key: 'management-test-secret', route: 'none', accounts: [] },
    metered: () => false, quotaControlsHTML: () => '',
    acctLogo: () => '', metersHTML: () => '', provName: () => 'Claude',
    ...extra,
  };
  vm.createContext(context);
  vm.runInContext(source, context);
  return {
    run(code) { return vm.runInContext(code, context); }, context,
    seed(data) { context.seed = data; vm.runInContext('Object.assign(Usage, seed)', context); },
  };
}

test('presets use local calendar dates and inclusive today across a year boundary', () => {
  const r = helpers.range(7, new Date(2027, 0, 2, 12));
  assert.deepEqual(r, { start: '2026-12-27', end: '2027-01-03', through: '2027-01-02' });
  assert.deepEqual(helpers.range(1, new Date(2026, 9, 7, 23, 59)), { start: '2026-10-07', end: '2026-10-08', through: '2026-10-07' });
});

test('copied evidence, multiple roots, breakdown labels and collector progress remain clear and private', () => {
  const d = dashboard();
  d.seed({ loaded: true, summary: { proxy: {}, sources: [{source:'codex', source_record_count:2, observations:0}], breakdowns: {client:[{id:'collector:private-id',source:'codex',observations:1}]}, facets: {clients:[{id:'collector:private-id',label:'Private <laptop>'}]} }, status: {health:{historical_gap:{dropped:0}}, imports:[{source:'codex',root:'/private/root1',enabled:true},{source:'codex',root:'/private/root2',enabled:false}], collectors:[{id:'private-id',label:'Private <laptop>',progress:[{source:'codex',state:'attention',imported:2,failed:1}]}]} });
  let html = d.run('usageHTML()');
  assert.ok(html.includes('Source records') && html.includes('Selected accounting entries'));
  assert.ok(html.includes('copied evidence selected under another origin'));
  assert.ok(html.includes('/private/root1') && html.includes('/private/root2'));
  assert.ok(html.includes('Private &lt;laptop&gt;') && !html.includes('Private <laptop>'));
  assert.ok(html.includes('total historical coverage unknown'));
  assert.ok(!html.includes('Historical analytics gaps are recorded'));
  d.run('S.private = true'); html = d.run('usageHTML()');
  for (const secret of ['private-id','Private &lt;laptop&gt;','/private/root1','/private/root2']) assert.ok(!html.includes(secret));
  assert.equal(d.run('usageTime(0)'), 'Not reported');
});

test('DST calendar shifts work in Amsterdam and Los Angeles', () => {
  const script = `const h=require(${JSON.stringify(path.join(__dirname, '../ui/usage.js'))});process.stdout.write(JSON.stringify([h.range(7,new Date(2026,2,29,12)),h.shift('2026-10-25',1)]))`;
  for (const timezone of ['Europe/Amsterdam', 'America/Los_Angeles']) {
    const result = JSON.parse(execFileSync(process.execPath, ['-e', script], { env: { ...process.env, TZ: timezone }, encoding: 'utf8' }));
    assert.deepEqual(result, [{ start: '2026-03-23', end: '2026-03-30', through: '2026-03-29' }, '2026-10-26']);
  }
});

test('custom dates reject invalid days and reversed boundaries', () => {
  assert.equal(helpers.shift('2028-02-29', 1), '2028-03-01');
  assert.throws(() => helpers.shift('2026-02-29', 0), /valid date/);
  assert.throws(() => helpers.shift('2026-04-31', 0), /valid date/);
  assert.throws(() => helpers.query({ start: '2026-10-08', end: '2026-10-08' }, {}, 'UTC'), /end date/);
});

test('filters are allowlisted, encoded, and use exclusive date endpoints', () => {
  const result = new URLSearchParams(helpers.query({ start: '2026-10-01', end: '2026-10-08' }, { provider: 'anthropic', model: 'model/a & b', account: 'account-1', client: 'collector-2', source: 'codex', key: 'should-not-be-sent' }, 'Europe/Amsterdam', { limit: 50, offset: 100 }));
  assert.equal(result.get('end'), '2026-10-08');
  assert.equal(result.get('timezone'), 'Europe/Amsterdam');
  assert.equal(result.get('model'), 'model/a & b');
  assert.equal(result.get('client'), 'collector-2');
  assert.equal(result.get('limit'), '50');
  assert.equal(result.get('offset'), '100');
  assert.equal(result.has('key'), false);
});

test('unknown pricing remains unpriced and small backend estimates retain precision', () => {
  assert.equal(helpers.money(null), 'Unpriced');
  assert.equal(helpers.money(undefined), 'Unpriced');
  assert.equal(helpers.money(0), '$0.00');
  assert.equal(helpers.money(12345000), '$0.0123');
});

test('exports authenticate only by header and explicitly include current pagination', () => {
  const r = helpers.exportRequest('start=2026-10-01&end=2026-10-08&timezone=UTC', 'csv', 'fbx_sensitive-management-key', 100, 50);
  assert.equal(r.options.headers.authorization, 'Bearer fbx_sensitive-management-key');
  assert.equal(r.url.includes('fbx_sensitive-management-key'), false);
  const q = new URL('http://localhost' + r.url).searchParams;
  assert.equal(q.get('format'), 'csv');
  assert.equal(q.get('limit'), '50');
  assert.equal(q.get('offset'), '100');
});

test('privacy hides all labels, IDs, paths, credentials and draft values in rendered markup', () => {
  const d = dashboard();
  d.context.S.private = true;
  d.seed({
    loaded: true, collectorDraft: 'private draft laptop', roots: { codex: '/private/draft/root' }, credential: 'fbxc_one-time-private-secret',
    summary: { proxy: {}, sources: [], trend: [], facets: { accounts: [{ id: 'private-account-id', label: 'private-account-label' }], clients: [{ id: 'private-client-id', label: 'private-client-label' }] } },
    observations: { total: 1, items: [{ account_id: 'private-account-id', account_label: 'private-account-label', client_id: 'private-client-id', client_label: 'private-client-label', origin_id: 'private-collector-id', collector_label: 'private-collector-label', tokens: {} }] },
    status: { health: { message: '/private/health/error' }, imports: [{ source: 'codex', root: '/private/saved/root', last_error: '/private/import/error' }], collectors: [{ id: 'private-collector-id', label: 'private-collector-label' }] },
  });
  const html = d.run('usageHTML()');
  for (const sensitive of ['private-account', 'private-client', 'private-collector', '/private/', 'private draft laptop', 'fbxc_one-time-private-secret']) assert.equal(html.includes(sensitive), false, sensitive);
  assert.match(html, /••••••/);
  assert.match(html, /value="0"/);
  d.context.S.private = false;
  const visible = d.run('usageHTML()');
  assert.match(visible, /private-account-label/);
  assert.match(visible, /fbxc_one-time-private-secret/);
});

test('all server supplied strings are escaped and token subsets remain separate', () => {
  const d = dashboard();
  d.seed({ observations: { total: 1, items: [{ actual_model: '<script>alert("x")</script>', account_label: '<img src=x onerror=evil()>', source: 'codex', tokens: { input: 100, output: 20, cache_read: 300, cache_write: 40, write_5m: 10, write_1h: 30, reasoning: 5 } }] } });
  const html = d.run('usageObservationsHTML()');
  assert.equal(html.includes('<script>'), false);
  assert.equal(html.includes('<img src=x'), false);
  assert.match(html, /&lt;script&gt;/);
  assert.match(html, /Write 5m/);
  assert.match(html, /Write 1h/);
  assert.match(html, /Reasoning/);
  assert.match(html, /Input 100 · output 20/);
  assert.match(html, /exports contain this page only/);
});

test('source estimates remain separate and unknown tokens are visible', () => {
  const d = dashboard();
  d.seed({ summary: { proxy: { logical_requests: 2, attempts: 3, estimated_cost_nanos: 1000000000, tokens: { input: null, output: 8 } }, sources: [{ source: 'codex', observations: 6, estimated_cost_nanos: 2000000000, possibly_overlapping: true }] } });
  const html = d.run('usageTotalsHTML()');
  assert.match(html, /Logical requests/);
  assert.match(html, /Upstream attempts/);
  assert.match(html, /Possibly overlaps/);
  assert.match(html, /No cross-source grand total/);
  assert.match(html, /Unknown \/ 8/);
  assert.equal(html.includes('$3.00'), false);
});

test('late responses from a superseded filter do not replace current results', async () => {
  let resolveFirst;
  let call = 0;
  const first = new Promise((resolve) => { resolveFirst = resolve; });
  const d = dashboard({ api: () => { call++; return call <= 3 ? first : Promise.resolve({ marker: 'new-result', items: [], total: 0 }); } });
  const oldLoad = d.run('usageLoad()');
  const newLoad = d.run('usageLoad()');
  await newLoad;
  resolveFirst({ marker: 'old-result' });
  await oldLoad;
  assert.equal(d.run('Usage.summary.marker'), 'new-result');
  assert.equal(d.run('Usage.loading'), false);
});

test('a status failure preserves successful summary and observation reads with visible error', async () => {
  const d = dashboard({ api: (endpoint) => endpoint === '/usage/status' ? Promise.reject(new Error('database unavailable')) : Promise.resolve({ marker: 'loaded', items: [], total: 0 }) });
  await d.run('usageLoad()');
  assert.equal(d.run('Usage.summary.marker'), 'loaded');
  assert.equal(d.run('Usage.observations.marker'), 'loaded');
  assert.match(d.run('Usage.error'), /database unavailable/);
  assert.equal(d.run('Usage.loading'), false);
});
