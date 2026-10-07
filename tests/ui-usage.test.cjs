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
    metered: () => false, quotaControlsHTML: () => '', statusHTML: () => '', planName: () => '', authName: () => 'OAuth',
    acctLogo: () => '', metersHTML: () => '', provName: (a) => ({ claude: 'Claude', codex: 'Codex' })[a.provider] || a.provider,
    logo: () => '', accountById: () => null, acctLabel: (a) => a.label,
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

test('multiple roots, collector progress and labels stay clear and private in the Collection drawer', () => {
  const d = dashboard();
  d.seed({ loaded: true, drawerOpen: true, summary: { combined: { totals: {}, trend: [], breakdowns: { client: [{ id: 'collector:private-id', observations: 1 }] } }, facets: { clients: [{ id: 'collector:private-id', label: 'Private <laptop>' }] } }, status: { health: { historical_gap: { dropped: 0 } }, imports: [{ source: 'codex', root: '/private/root1', enabled: true }, { source: 'codex', root: '/private/root2', enabled: false }], collectors: [{ id: 'private-id', label: 'Private <laptop>', progress: [{ source: 'codex', state: 'attention', imported: 2, failed: 1 }] }] } });
  let html = d.run('usageHTML()');
  assert.ok(html.includes('/private/root1') && html.includes('/private/root2'));
  assert.ok(html.includes('Private &lt;laptop&gt;') && !html.includes('Private <laptop>'));
  assert.ok(html.includes('total historical coverage unknown'));
  assert.ok(html.includes('keeps the time it happened, not the time it was imported'));
  assert.ok(!html.includes('Earlier writer sessions recorded'));
  d.run('Usage.dim = "client"');
  assert.ok(d.run('usageHTML()').includes('Private &lt;laptop&gt;'));
  d.run('S.private = true'); html = d.run('usageHTML()');
  for (const secret of ['private-id', 'Private &lt;laptop&gt;', '/private/root1', '/private/root2']) assert.ok(!html.includes(secret), secret);
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
  const result = new URLSearchParams(helpers.query({ start: '2026-10-01', end: '2026-10-08' }, { provider: 'anthropic', model: 'model/a & b', account: 'account-1', client: 'collector-2', source: 'codex', key: 'should-not-be-sent' }, 'Europe/Amsterdam', { limit: 50, offset: 100 }, { view: 'combined', stack: 'model', key: 'nope' }));
  assert.equal(result.get('end'), '2026-10-08');
  assert.equal(result.get('timezone'), 'Europe/Amsterdam');
  assert.equal(result.get('model'), 'model/a & b');
  assert.equal(result.get('client'), 'collector-2');
  assert.equal(result.get('limit'), '50');
  assert.equal(result.get('offset'), '100');
  assert.equal(result.has('key'), false);
  assert.equal(result.has('source'), false);
  assert.equal(result.get('view'), 'combined');
  assert.equal(result.get('stack'), 'model');
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
    loaded: true, drawerOpen: true, dim: 'account', openRecord: 1, collectorDraft: 'private draft laptop', roots: { codex: '/private/draft/root' }, credential: 'fbxc_one-time-private-secret', credentialFor: 'private-collector-label',
    summary: { combined: { totals: {}, trend: [], breakdowns: { account: [{ id: 'private-account-id', observations: 1 }] } }, facets: { accounts: [{ id: 'private-account-id', label: 'private-account-label' }], clients: [{ id: 'private-client-id', label: 'private-client-label' }] } },
    filters: { provider: '', model: '', account: 'private-account-id', client: 'private-client-id' },
    observations: { total: 2, items: [{ source: 'proxy', account_id: 'private-account-id', account_label: 'private-account-label', client_id: 'private-client-id', client_label: 'private-client-label', origin_id: 'local', tokens: {} }, { source: 'codex', origin_id: 'collector:private-collector-id', origin_label: 'private-collector-label', tokens: {} }] },
    status: { health: { message: '/private/health/error' }, imports: [{ source: 'codex', root: '/private/saved/root', last_error: '/private/import/error' }], collectors: [{ id: 'private-collector-id', label: 'private-collector-label' }] },
  });
  const html = d.run('usageHTML()');
  for (const sensitive of ['private-account', 'private-client', 'private-collector', '/private/', 'private draft laptop', 'fbxc_one-time-private-secret']) assert.equal(html.includes(sensitive), false, sensitive);
  assert.match(html, /••••••/);
  assert.match(html, /data-usage-act="filter-row" data-index="0"/);
  d.context.S.private = false;
  const visible = d.run('usageHTML()');
  assert.match(visible, /private-account-label/);
  assert.match(visible, /private-collector-label/);
  assert.match(visible, /fbxc_one-time-private-secret/);
});

test('all server supplied strings are escaped and token subsets remain separate', () => {
  const d = dashboard();
  d.seed({ openRecord: 0, observations: { total: 1, items: [{ actual_model: '<script>alert("x")</script>', account_label: '<img src=x onerror=evil()>', account_id: 'x', source: 'codex', origin_id: 'local', origin_label: 'This server', pricing_snapshot: { backdated: true }, estimated_cost_nanos: 1000, tokens: { input: 100, output: 20, cache_read: 300, cache_write: 40, write_5m: 10, write_1h: 30, reasoning: 5 } }] }, summary: { pricing: { verified_at: '2026-10-07' }, combined: {} } });
  const html = d.run('usageObservationsHTML()');
  assert.equal(html.includes('<script>'), false);
  assert.equal(html.includes('<img src=x'), false);
  assert.match(html, /&lt;script&gt;/);
  for (const label of ['Write 5m', 'Write 1h', 'Reasoning', 'Cache read', 'Cache write']) assert.match(html, new RegExp(label));
  assert.match(html, /Input excludes cache reads and writes/);
  assert.match(html, /Codex history · This server/);
  assert.match(html, /from Codex history, This server · not seen by the proxy/);
  assert.match(html, /Priced at today&#39;s rate \(catalogue 2026-10-07\)/);
  assert.match(html, /exports contain this page only/);
});

test('a proxy record matched to imported history says it is counted once', () => {
  const d = dashboard();
  d.seed({ openRecord: 0, observations: { total: 1, items: [{ source: 'proxy', provider: 'anthropic', actual_model: 'claude-opus-5-5', matched_sources: ['claude_code'], tokens: {}, estimated_cost_nanos: 5 }] } });
  assert.match(d.run('usageObservationsHTML()'), /also in Claude Code history with the same message ID, counted once/);
});

test('the combined total is the headline and coverage leaves out zero parts', () => {
  const d = dashboard();
  d.seed({ preset: '7', range: { start: '2026-10-01', end: '2026-10-08', through: '2026-10-07' }, summary: { pricing: { version: 'cat-1' }, combined: { totals: { observations: 1000, estimated_cost_nanos: 7000000000, unpriced: 8, matched: 870, weak_identity: 12, history_only: 300, partial: 0, tokens: { input: 1500, output: 8, cache_read: null, cache_write: null }, missing_token_counts: {} }, trend: [] } } });
  const html = d.run('usageCostHTML()');
  assert.match(html, /\$7\.00/);
  assert.match(html, /LAST 7 DAYS/);
  assert.match(html, /Avg \$1\.00 a day · priced calls only/);
  assert.match(html, /99\.2% of calls priced · 870 matched across sources, counted once · 12 history records without a stable ID · 8 unpriced/);
  assert.equal(html.includes('partial ·'), false);
  assert.match(html, /From history only/);
  assert.match(html, /Unknown<\/dd><dt>Cache write</);
  assert.match(html, /catalogue cat-1/);
  assert.equal(html.includes('cross-source'), false);
});

test('chart keeps the top five groups and merges the rest into Other', () => {
  const days = ['2026-10-06', '2026-10-07'];
  const trend = ['a', 'b', 'c', 'd', 'e', 'f', 'g'].flatMap((g, i) => days.map((date) => ({ date, group: g, observations: 10 - i, estimated_cost_nanos: (10 - i) * 1e9 })));
  const c = helpers.chart(trend, days, 'cost');
  assert.deepEqual(c.keys.slice(0, 5), ['a', 'b', 'c', 'd', 'e']);
  assert.equal(c.keys[5], helpers.OTHER);
  assert.equal(c.colors[0], 'oklch(0.74 0.09 30)');
  assert.equal(c.colors[5], '#57534b');
  assert.equal(c.series[0].parts[5], 9); // f (5) + g (4)
  const six = helpers.chart(trend.filter((r) => r.group !== 'g'), days, 'calls');
  assert.deepEqual(six.keys, ['a', 'b', 'c', 'd', 'e', 'f']);
  assert.equal(six.colors[5], 'oklch(0.74 0.09 330)');
});

test('readout shows the hovered day with each group, and history-only days before the proxy', () => {
  const d = dashboard();
  const trend = [{ date: '2026-10-05', group: 'anthropic', observations: 4, estimated_cost_nanos: 3e9, unpriced: 1 }, { date: '2026-10-06', group: 'openai', observations: 9, estimated_cost_nanos: 9e9 }];
  d.seed({ range: { start: '2026-10-05', end: '2026-10-07', through: '2026-10-06' }, timezone: 'Europe/Amsterdam', summary: { combined: { trend, proxy_first_event_at_ms: Date.UTC(2026, 9, 5, 23, 30), totals: {} } } });
  let html = d.run('usageChartHTML()');
  assert.match(html, /Peak day · Tue Oct 6/);
  assert.equal(html.includes('Before the proxy was set up'), false);
  d.run('Usage.hoverDay = "2026-10-05"');
  html = d.run('usageChartHTML()');
  assert.match(html, /Mon Oct 5/);
  assert.match(html, /4 calls/);
  assert.match(html, /Claude<\/span> <span class="mono">\$3\.00/);
  assert.match(html, /3 priced · 1 unpriced/);
  assert.match(html, /1 unpriced/);
  assert.match(html, /Before the proxy was set up · history only/);
  assert.equal(helpers.beforeProxy('2026-10-06', Date.UTC(2026, 9, 5, 23, 30), 'Europe/Amsterdam'), false);
  assert.equal(helpers.beforeProxy('2026-10-05', null, 'UTC'), false);
});

test('daily priced subset is distinct from the range breakdown and Fable is visible initially', () => {
  const d = dashboard();
  const trend = [
    { date: '2026-10-01', group: 'anthropic', observations: 2446, estimated_cost_nanos: 280000, unpriced: 2444 },
    { date: '2026-10-01', group: 'openai', observations: 13323, estimated_cost_nanos: 2111131084260, unpriced: 5 },
  ];
  const models = Array.from({ length: 15 }, (_, i) => ({ id: i === 10 ? 'claude-fable-5-1' : `model-${i}`, provider: 'anthropic', observations: 10, estimated_cost_nanos: 1e9 }));
  models[10] = { ...models[10], observations: 1896, estimated_cost_nanos: 6900000, unpriced: 1895, partial: 1880, missing_usage: 16 };
  d.seed({ preset: '30', range: { start: '2026-09-08', end: '2026-10-08', through: '2026-10-07' }, summary: { combined: { trend, totals: { estimated_cost_nanos: 14568080000000 }, breakdowns: { model: models } } } });
  const chart = d.run('usageChartHTML()');
  assert.match(chart, /Peak day · Thu Oct 1/);
  assert.match(chart, /Priced calls only/);
  assert.match(chart, /Claude<\/span> <span class="mono">\$0\.0003<\/span><span class="dim">\(2 priced · 2,444 unpriced\)/);
  const breakdown = d.run('usageBreakdownHTML()');
  assert.match(breakdown, /BREAKDOWN · LAST 30 DAYS/);
  assert.match(breakdown, /claude-fable-5-1/);
  assert.match(breakdown, /\$0\.0069/);
  assert.match(breakdown, /1,895 unpriced/);
  assert.match(breakdown, /title="[^"]*1,880 partial · 16 missing usage/);
  assert.equal(d.run('Usage.rows.length'), 15);
  assert.equal(d.run('Usage.rows[10]'), 'claude-fable-5-1');
  assert.equal(breakdown.includes('Show all 15'), false);
  d.run('Usage.hoverDay = "2026-10-01"');
  assert.match(d.run('usageChartHTML()'), /Day · Thu Oct 1/);
  assert.match(d.run('usageBreakdownHTML()'), /BREAKDOWN · LAST 30 DAYS/);
});

test('cost charts preserve wholly unpriced groups, known zero and empty days', () => {
  const d = dashboard();
  const trend = [
    { date: '2026-10-05', group: 'anthropic', observations: 4, estimated_cost_nanos: null, unpriced: 4 },
    { date: '2026-10-06', group: 'openai', observations: 1, estimated_cost_nanos: 0, unpriced: 0 },
  ];
  const chart = helpers.chart(trend, ['2026-10-05', '2026-10-06', '2026-10-07'], 'cost');
  assert.deepEqual(chart.keys, ['anthropic', 'openai']);
  assert.equal(chart.max, 0);
  assert.equal(chart.peak, 0);
  assert.equal(chart.series[0].groups[0].priced, false);
  assert.equal(chart.series[1].groups[1].priced, true);
  d.seed({ range: { start: '2026-10-05', end: '2026-10-08', through: '2026-10-07' }, hoverDay: '2026-10-05', summary: { combined: { trend } } });
  let html = d.run('usageChartHTML()');
  assert.match(html, /Claude<\/span> <span class="mono">Unpriced/);
  assert.match(html, /0 priced · 4 unpriced/);
  assert.match(html, /aria-label="Mon Oct 5: Unpriced · 0 priced calls · 4 unpriced"/);
  assert.match(html, /aria-label="Tue Oct 6: \$0\.00 · 1 priced calls"/);
  assert.match(html, /aria-label="Wed Oct 7: \$0\.00 · 0 priced calls"/);
  d.run('Usage.hoverDay = null');
  html = d.run('usageChartHTML()');
  assert.match(html, /Busiest day · Mon Oct 5/);
  assert.match(html, /Claude<\/span> <span class="mono">Unpriced/);
  d.run('Usage.metric = "calls"');
  html = d.run('usageChartHTML()');
  assert.match(html, /Claude<\/span> <span class="mono">4 calls/);
  assert.match(html, /\(4 unpriced\)/);
});

test('a range with only unknown costs has an unpriced headline while an empty range is zero', () => {
  const d = dashboard();
  d.seed({ summary: { combined: { totals: { observations: 4, estimated_cost_nanos: null, unpriced: 4 } } } });
  assert.match(d.run('usageCostHTML()'), /u-total-v">Unpriced/);
  d.seed({ summary: { combined: { totals: { observations: 0, estimated_cost_nanos: null, unpriced: 0 } } } });
  assert.match(d.run('usageCostHTML()'), /u-total-v">\$0\.00/);
});

test('Other retains the priced and unpriced coverage of every merged group', () => {
  const trend = Array.from({ length: 8 }, (_, i) => ({ date: '2026-10-05', group: `model-${i}`, observations: 10, estimated_cost_nanos: i < 6 ? (8 - i) * 1e9 : null, unpriced: i < 6 ? 1 : 10 }));
  const c = helpers.chart(trend, ['2026-10-05'], 'cost');
  assert.equal(c.keys[5], helpers.OTHER);
  assert.deepEqual(c.series[0].groups[5], { calls: 30, cost: 3e9, priced: true, unpriced: 21 });
  assert.equal(c.series[0].calls, 80);
  assert.equal(c.series[0].unpriced, 26);
  assert.equal(c.series[0].cost, 33e9);
});

test('model rows beyond the initial twenty can expand and coverage explains unknown costs', () => {
  const d = dashboard();
  const rows = Array.from({ length: 21 }, (_, i) => ({ id: `model-${i}`, observations: 2, estimated_cost_nanos: i ? 1e9 : null, unpriced: i ? 0 : 2 }));
  d.seed({ summary: { combined: { totals: { estimated_cost_nanos: 20e9 }, breakdowns: { model: rows } } } });
  let html = d.run('usageBreakdownHTML()');
  assert.equal(d.run('Usage.rows.length'), 20);
  assert.match(html, /Show all 21/);
  assert.match(html, /u-bd-cost dim">Unpriced/);
  assert.equal(html.includes('model-20'), false);
  d.run('Usage.showAll = true');
  html = d.run('usageBreakdownHTML()');
  assert.equal(d.run('Usage.rows.length'), 21);
  assert.match(html, /Show top 20/);
  assert.match(html, /model-20/);
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

test('counts render immediately while status and records are still pending', async () => {
  let finishStatus, finishRecords;
  const status = new Promise((resolve) => { finishStatus = resolve; });
  const records = new Promise((resolve) => { finishRecords = resolve; });
  let renders = 0;
  const d = dashboard({ api: (endpoint) => endpoint === '/usage/status' ? status : endpoint.includes('/observations?') ? records : Promise.resolve({ marker: 'counts-ready' }) });
  d.context.markRender = () => { renders++; };
  d.run('usageRender = markRender');
  const loading = d.run('usageLoad()');
  await new Promise(setImmediate);
  assert.equal(d.run('Usage.summary.marker'), 'counts-ready');
  assert.equal(d.run('Usage.loading'), true);
  assert.equal(d.run('Usage.recordsLoading'), true);
  assert.ok(renders >= 2);
  finishRecords({ items: [], total: 0 });
  await new Promise(setImmediate);
  assert.equal(d.run('Usage.recordsLoading'), false);
  finishStatus({ health: {} });
  await loading;
  assert.equal(d.run('Usage.loading'), false);
});

test('paging reads only records and cannot replace results from newer filters', async () => {
  let finishOld;
  const oldPage = new Promise((resolve) => { finishOld = resolve; });
  const calls = [];
  const d = dashboard({ api: (endpoint) => {
    calls.push(endpoint);
    return calls.length === 1 ? oldPage : Promise.resolve({ marker: 'new-filter', items: [], total: 0 });
  } });
  d.seed({ summary: { marker: 'existing-counts' }, status: { marker: 'existing-status' }, offset: 50 });
  const paging = d.run('usageLoadRecords()');
  assert.equal(calls.length, 1);
  assert.match(calls[0], /\/usage\/observations\?.*&offset=50/);
  assert.equal(d.run('Usage.summary.marker'), 'existing-counts');
  assert.equal(d.run('Usage.status.marker'), 'existing-status');
  d.run('Usage.filters.provider = "openai"');
  await d.run('usageReload()');
  finishOld({ marker: 'old-page', items: [], total: 0 });
  await paging;
  assert.equal(d.run('Usage.summary.marker'), 'new-filter');
  assert.equal(d.run('Usage.observations.marker'), 'new-filter');
  assert.equal(d.run('Usage.offset'), 0);
  assert.equal(d.run('Usage.recordsLoading'), false);
});

test('chart stacking uses the loaded snapshot without another request, including during initial load', async () => {
  let finishSummary;
  let calls = 0;
  const response = { combined: { stack: 'provider', trend: [{ marker: 'provider' }], trends: { provider: [{ marker: 'provider' }], model: [{ marker: 'model' }] } } };
  const d = dashboard({ api: (endpoint) => {
    calls++;
    return endpoint.includes('/dashboard?') ? new Promise((resolve) => { finishSummary = resolve; }) : Promise.resolve({ items: [], total: 0 });
  } });
  const loading = d.run('usageLoad()');
  d.run('Usage.stack = "model"; usageLoadStack()');
  finishSummary(response);
  await loading;
  assert.equal(d.run('Usage.summary.combined.trend[0].marker'), 'model');
  assert.equal(d.run('Usage.summary.combined.stack'), 'model');
  d.run('Usage.stack = "provider"; usageLoadStack()');
  assert.equal(d.run('Usage.summary.combined.trend[0].marker'), 'provider');
  assert.equal(calls, 3);
});
