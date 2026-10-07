const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

// Exercise the dashboard's actual live request handler without a browser/DOM.
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const handler = source.slice(source.indexOf('function onRequest(log) {'), source.indexOf('// ---------------------------------------------------------------- render'));

function dashboard() {
  const S = {
    requests: [], fresh: new Map(), activity: {}, route: 'none',
    overview: { totals: { requests: 0, ok: 0, failed: 0 }, series: [] },
  };
  const context = { S, setTimeout() {}, renderFaults() {} };
  vm.createContext(context);
  vm.runInContext(handler, context);
  return {
    S,
    record(minute, input = 100) {
      context.onRequest({ id: S.requests.length, ts: new Date(minute * 60000).toISOString(), status: 200, input_tokens: input, output_tokens: 2, cache_tokens: 3 });
    },
  };
}

test('out-of-order stream completions update one bucket per start minute', () => {
  const now = Math.floor(Date.now() / 60000);
  const { S, record } = dashboard();
  [now - 2, now, now - 2, now - 1, now - 2].forEach((minute) => record(minute));
  assert.equal(S.overview.series.length, 3);
  const first = S.overview.series.find((bucket) => bucket.minute === now - 2);
  assert.equal(first.requests, 3);
  assert.equal(first.input_tokens, 300);
  assert.equal(first.tokens, 315);
  assert.equal(S.overview.series.reduce((sum, bucket) => sum + bucket.requests, 0), 5);
  assert.ok(S.overview.series.every((bucket, index, series) => !index || series[index - 1].minute < bucket.minute));
});

test('a very long stream finishing late preserves the current hour', () => {
  const now = Math.floor(Date.now() / 60000);
  const { S, record } = dashboard();
  for (let ago = 59; ago >= 0; ago--) record(now - ago);
  record(now - 65, 1000);
  assert.equal(S.overview.series.length, 60);
  assert.equal(S.overview.series.reduce((sum, bucket) => sum + bucket.input_tokens, 0), 6000);
  assert.equal(S.overview.totals.requests, 61);
});
