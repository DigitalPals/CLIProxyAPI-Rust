const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const section = (start, end) => source.slice(source.indexOf(start), source.indexOf(end));
const code = [
  section('function onRequest(log) {', '// ---------------------------------------------------------------- render'),
  section('function figuresHTML() {', 'function ovFaultsHTML() {'),
  section('const statusCls =', 'function recentRowHTML(r) {'),
  section('function drawerSinceHTML() {', 'function drawerRequestsHTML() {'),
  section('function detLoadHTML() {', 'function detRequestsHTML() {'),
  section('function detSessionsHTML() {', '// connect, sign-in and API key panels'),
  section('function whySentence(r) {', 'function routeHTML(r,'),
  section('function errLineText(r) {', 'function reqItemHTML(r) {'),
  section('function reqItemHTML(r) {', '// A live arrival goes on top'),
].join('\n');

function dashboard() {
  const S = {
    requests: [], fresh: new Map(), activity: {}, route: 'none', openReq: 1,
    overview: { active: 0, totals: { requests: 0, ok: 0, failed: 0 }, series: [] },
  };
  const context = {
    S, setTimeout() {}, renderFaults() {},
    fmt: (n) => String(n ?? 0),
    esc: (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])),
    plural: (n, label) => `${n} ${label}${n === 1 ? '' : 's'}`,
    isErr: (r) => r.status >= 400 && r.status !== 499,
    isFresh: () => false, accountOf: () => null, accountById: () => context.account,
    hideEmails: (s) => s, clock: () => '12:00', hm: () => '12:00', ago: () => 'now', ms: (n) => n == null ? '—' : `${n}ms`,
    routeHTML: () => '', acctCellHTML: () => '', reqAcctName: () => '', noteHTML: () => '',
    mob: () => false, logo: () => '',
    CLIENT: { responses: 'Responses' }, ENDPOINT: { responses: '/v1/responses' }, PROVIDER: { codex: 'Codex' },
    STRATEGY_PICK: {}, COUNT: {},
  };
  vm.createContext(context);
  vm.runInContext(code, context);
  return {
    S, context,
    run(expression) { return vm.runInContext(expression, context); },
    record(status, usage_completeness, input_tokens = 0, output_tokens = 0, cache_tokens = 0) {
      const r = { id: S.requests.length + 1, ts: new Date().toISOString(), status, usage_completeness, input_tokens, output_tokens, cache_tokens, client: 'responses', model: 'test', attempts: 1 };
      context.onRequest(r);
      return r;
    },
  };
}

test('live counters and charts distinguish cancellations, failures and incomplete usage', () => {
  const d = dashboard();
  for (const row of [[200, 'complete', 100, 20, 30], [200, 'missing'], [499, 'missing'], [499, 'partial', 10, 2, 3], [499, 'complete'], [502, 'missing'], [502, 'partial', 5, 1, 2]]) d.record(...row);
  const totals = d.S.overview.totals;
  assert.deepEqual([totals.requests, totals.ok, totals.failed, totals.cancelled, totals.usage_missing, totals.usage_partial], [7, 2, 2, 3, 3, 2]);
  assert.deepEqual([totals.input_tokens, totals.output_tokens, totals.cache_tokens], [115, 23, 35]);
  const [bucket] = d.S.overview.series;
  assert.deepEqual([bucket.requests, bucket.failed, bucket.cancelled, bucket.usage_missing, bucket.usage_partial, bucket.tokens], [7, 2, 3, 3, 2, 173]);
  const figures = d.run('figuresHTML()');
  assert.match(figures, /non-cancelled requests">50\.0%/);
  assert.match(figures, /<dt>Cancelled<\/dt><dd>3/);
  assert.match(figures, /<dt>Usage missing<\/dt><dd>3/);
  assert.match(figures, /<dt>Usage partial<\/dt><dd>2/);
  assert.match(figures, /≥115/);
  assert.match(d.run('barsHTML()'), /2 failed, 3 cancelled.*≥173 tokens.*3 missing · 2 partial/);
});

test('cancellation-only traffic has no success rate or failure bar and no invented zero usage', () => {
  const d = dashboard();
  d.record(499, 'missing');
  assert.equal(d.S.overview.totals.failed, 0);
  assert.equal(d.S.overview.totals.ok, 0);
  const figures = d.run('figuresHTML()');
  assert.match(figures, /non-cancelled requests">—/);
  assert.match(figures, /Unknown/);
  const bars = d.run('barsHTML()');
  assert.match(bars, /1 cancelled.*Unknown tokens/);
  assert.doesNotMatch(bars, /class="[^"]*fail/);
  d.record(200, 'complete', 10);
  assert.match(d.run('figuresHTML()'), /non-cancelled requests">100%/);
});

test('request rows and expanded desktop/mobile details show missing usage for 499, 502 and 200', () => {
  const d = dashboard();
  for (const status of [499, 502, 200]) {
    d.context.request = { ...d.record(status, 'missing'), id: 1 };
    assert.match(d.run("tokensText(request, 'input_tokens')"), />Unknown</);
    assert.match(d.run('reqItemHTML(request)'), /Unknown · provider usage not reported/);
    assert.match(d.run('reqMobileHTML(request)'), /Unknown · provider usage not reported/);
    assert.doesNotMatch(d.run('reqItemHTML(request)'), />0<\/span>/);
  }
});

test('reported zeros stay zero; partial usage stays visible without asserting a final count', () => {
  const d = dashboard();
  d.context.request = d.record(499, 'complete');
  assert.match(d.run("tokensText(request, 'input_tokens')"), />0</);
  assert.doesNotMatch(d.run('requestTokensHTML(request)'), /Unknown|partial|≥/);
  d.context.request = d.record(499, 'partial', 12, 3, 40);
  assert.match(d.run('requestTokensHTML(request)'), /≥12.*≥3.*≥40.*partial usage/);
  assert.match(d.run("tokensText(request, 'output_tokens')"), /total consumption is unknown/);
});

test('499 timeout and connection errors describe the observed cause without implying user intent', () => {
  const d = dashboard();
  d.context.request = { ...d.record(499, 'missing'), failure_kind: 'downstream_write_timeout', routing_reason: 'session_reused' };
  for (const call of ['statusCell(request)', 'errLineText(request)', 'whySentence(request)', 'reqItemHTML(request)', 'reqMobileHTML(request)']) {
    const text = d.run(call);
    assert.match(text, /Timed out sending the response to the client\./);
    assert.doesNotMatch(text, /Cancelled by client|[Cc]lient closed the connection/);
  }
  for (const [kind, expected] of [
    ['downstream_disconnect', 'Connection closed before completion.'],
    ['downstream_write_error', 'Connection error while sending the response to the client.'],
    ['downstream_read_error', 'Connection error while reading from the client.'],
    ['downstream_queue_limit', 'Connection ended because the pending request limit was reached.'],
    [null, 'Connection ended before completion.'],
  ]) {
    d.context.request.failure_kind = kind;
    assert.equal(d.run('errLineText(request)'), expected);
  }
});

test('account drawer and activity use cancellation counts and report incomplete token sums', () => {
  const d = dashboard();
  d.record(499, 'missing');
  d.context.account = { id: 'test-account', counters: { ...d.S.overview.totals, failures: 0 } };
  d.S.drawer = 'test-account';
  d.S.sub = 'test-account';
  d.S.activity['test-account'] = { series: d.S.overview.series, sessions: [] };
  const drawer = d.run('drawerSinceHTML()');
  assert.match(drawer, /<b>0<\/b><span>Failed/);
  assert.match(drawer, /<b>1<\/b><span>Cancelled/);
  assert.match(drawer, /Unknown/);
  assert.match(drawer, /Usage missing/);
  const load = d.run('detLoadHTML()');
  assert.match(load, /<dt>Failed<\/dt><dd>0/);
  assert.match(load, /<dt>Cancelled<\/dt><dd>1/);
  assert.match(load, /Unknown/);
  assert.match(load, /Usage missing/);
});

test('pinned session cards show unknown usage and partial cached totals', () => {
  const d = dashboard();
  d.context.account = { id: 'test-account' };
  d.S.sub = 'test-account';
  d.S.activity['test-account'] = { sessions: [
    { requests: 1, cache_tokens: 0, usage_missing: 1, usage_partial: 0 },
    { requests: 2, cache_tokens: 40, usage_missing: 1, usage_partial: 1 },
    { requests: 1, cache_tokens: 0, usage_missing: 0, usage_partial: 0 },
    { requests: 0, cache_tokens: 0, usage_missing: 0, usage_partial: 0 },
  ] };
  const html = d.run('detSessionsHTML()');
  assert.match(html, />Unknown<\/span> cached · usage: 1 missing/);
  assert.match(html, />≥40<\/span> cached · usage: 1 missing · 1 partial/);
  assert.match(html, />0<\/span> cached<\/span>/);
  assert.match(html, /2 recent req/);
  assert.match(html, /No requests in the last 300/);
  // Only the explicitly reported complete zero renders as zero cached tokens.
  assert.equal((html.match(/>0<\/span> cached/g) || []).length, 1);
});
