/* Run in the collaborative browser against an ISOLATED Fusebox backend containing
 * only synthetic histories. No frontend responses are mocked.
 * await runUsageAcceptance({ fixtureRoot: '/tmp/...', managementKey: 'test-only' })
 * The fixture root must contain claude/ and codex/ with the committed fixtures.
 */
'use strict';
async function runUsageAcceptance({ fixtureRoot, managementKey }) {
  const checks = [];
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const wait = async (condition) => {
    const deadline = Date.now() + 15000;
    while (!condition()) {
      if (Date.now() > deadline) throw new Error('Browser acceptance timed out');
      await new Promise((resolve) => setTimeout(resolve, 30));
    }
  };
  const click = (selector) => {
    const element = document.querySelector(selector);
    assert(element && !element.disabled, `Unavailable control: ${selector}`);
    element.click();
  };
  const input = (selector, value) => {
    const element = document.querySelector(selector);
    assert(element, `Missing input: ${selector}`);
    element.value = value; element.dispatchEvent(new Event('input', { bubbles: true }));
  };
  const idle = () => wait(() => !Usage.busy && !Usage.loading);
  if (S.locked) {
    input('#mk', managementKey); document.querySelector('#mk').form.requestSubmit();
    await wait(() => !S.locked);
  }
  location.hash = '#/usage'; await wait(() => !!document.querySelector('.usage-page'));
  await idle();
  if (S.private) click('[data-privacy]');
  click('[data-usage-act="clear-filters"]'); await idle();
  for (const [source, folder] of [['claude_code', 'claude'], ['codex', 'codex']]) {
    input(`[data-usage-import="${source}"] input`, `${fixtureRoot}/${folder}`);
    document.querySelector(`[data-usage-import="${source}"]`).requestSubmit(); await idle();
    click(`[data-usage-act="scan"][data-source="${source}"]`); await idle();
    assert(Usage.summary.sources.find((s) => s.source === source).observations >= 1, `${source} backfill`);
  }
  assert(document.body.innerText.includes('No cross-source grand total'), 'Overlap warning absent');
  assert(!document.body.innerText.includes('PRIVATE-PROMPT-MARKER'), 'Transcript leak');
  assert(!document.body.innerText.includes('PRIVATE-PROJECT-MARKER'), 'Project leak');
  checks.push('native imports, source warnings and metadata privacy');
  const filter = document.querySelector('[data-usage-filter="source"]');
  filter.value = String(Usage.sourceOptions.findIndex((s) => s === 'codex'));
  filter.dispatchEvent(new Event('change', { bubbles: true })); await idle();
  assert(Usage.observations.items.length > 0 && Usage.observations.items.every((r) => r.source === 'codex'), 'Source filter');
  checks.push('server-backed source filtering');
  click('[data-usage-act="range"][data-value="custom"]');
  input('#usage-range input[name="start"]', Usage.range.start);
  input('#usage-range input[name="through"]', Usage.range.through);
  document.querySelector('#usage-range').requestSubmit(); await idle();
  checks.push('custom date range');
  if (Usage.credential) click('[data-usage-act="dismiss-credential"]');
  const label = `Acceptance laptop <img src=x> ${Date.now()}`;
  input('#usage-enroll input', label); document.querySelector('#usage-enroll').requestSubmit(); await idle();
  assert(Usage.credential, 'No enrollment credential');
  assert(!document.querySelector('.usage-collectors img'), 'Collector label interpreted as HTML');
  const credential = Usage.credential;
  const upload = await fetch('/api/usage-ingest', { method: 'POST', headers: { authorization: `Bearer ${credential}`, 'content-type': 'application/json' }, body: JSON.stringify({ version: 1, observations: [], pending: 0 }) });
  assert(upload.ok, 'Collector contact failed'); await usageLoad();
  let index = Usage.status.collectors.findIndex((c) => c.label === label);
  assert(Usage.status.collectors[index].last_contact_at_ms && Usage.status.collectors[index].last_sync_at_ms, 'Collector health');
  click('[data-privacy]');
  const html = document.querySelector('.usage-page').innerHTML;
  assert(!html.includes(fixtureRoot) && !html.includes(label) && !html.includes(credential), 'Privacy markup leak');
  click('[data-privacy]'); click('[data-usage-act="dismiss-credential"]');
  click(`[data-usage-act="rotate"][data-index="${index}"]`); click('[data-usage-act="confirm"]'); await idle();
  assert(Usage.credential && Usage.credential !== credential, 'Credential rotation');
  const old = await fetch('/api/usage-ingest', { method: 'POST', headers: { authorization: `Bearer ${credential}`, 'content-type': 'application/json' }, body: '{"version":1,"observations":[]}' });
  assert(old.status === 401, 'Old collector credential accepted');
  click('[data-usage-act="dismiss-credential"]');
  index = Usage.status.collectors.findIndex((c) => c.label === label);
  click(`[data-usage-act="revoke"][data-index="${index}"]`); click('[data-usage-act="confirm"]'); await idle();
  assert(Usage.status.collectors.find((c) => c.label === label).revoked, 'Revocation');
  checks.push('collector enrollment, health, safe labels, privacy, rotation and revocation');
  const blobs = [], original = URL.createObjectURL;
  URL.createObjectURL = function (blob) { blobs.push(blob); return original.call(URL, blob); };
  try {
    click('[data-usage-act="export-json"]'); await idle();
    click('[data-usage-act="export-csv"]'); await idle();
  } finally { URL.createObjectURL = original; }
  assert(blobs.length === 2, 'Both exports did not download');
  for (const blob of blobs) {
    const text = await blob.text();
    assert(!text.includes('PRIVATE-PROMPT-MARKER') && !text.includes(managementKey), 'Export leak');
  }
  checks.push('authenticated JSON and CSV downloads from actual backend');
  const toggle = document.querySelector('[data-usage-act="toggle-import"]');
  const rootIndex = Number(toggle.dataset.index), before = Usage.status.imports[rootIndex].enabled;
  toggle.click(); await idle();
  assert(Usage.status.imports[rootIndex].enabled !== before, 'Import toggle');
  checks.push('import enable/disable');
  return { passed: checks.length, checks };
}
