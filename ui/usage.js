'use strict';

// Small shared helpers are exported for date, privacy and authenticated-export tests.
const UsageTools = (() => {
  const escape = (v) => String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const date = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  const shift = (value, days) => {
    if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) throw new Error('Choose a valid date.');
    const [y, m, d] = value.split('-').map(Number);
    const at = new Date(Date.UTC(y, m - 1, d));
    if (at.toISOString().slice(0, 10) !== value) throw new Error('Choose a valid date.');
    at.setUTCDate(at.getUTCDate() + days);
    return at.toISOString().slice(0, 10);
  };
  const range = (days, now = new Date()) => {
    const today = date(now);
    return { start: shift(today, 1 - days), end: shift(today, 1), through: today };
  };
  const query = (r, filters, timezone, page) => {
    shift(r.start, 0); shift(r.end, 0);
    if (r.start >= r.end) throw new Error('The end date must be on or after the start date.');
    const q = new URLSearchParams({ start: r.start, end: r.end, timezone });
    for (const k of ['provider', 'model', 'account', 'client', 'source']) if (filters[k]) q.set(k, filters[k]);
    if (page) { q.set('limit', String(page.limit)); q.set('offset', String(page.offset)); }
    return q.toString();
  };
  const privateLabel = (value, hidden) => hidden && value != null && value !== '' ? '••••••' : String(value ?? 'Unknown');
  const money = (nanos) => nanos == null || !Number.isFinite(Number(nanos)) ? 'Unpriced' : new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 4 }).format(Number(nanos) / 1e9);
  const exportRequest = (queryString, format, key, offset = 0, limit = 50) => ({ url: `/api/usage/export?${queryString}&format=${encodeURIComponent(format)}&limit=${limit}&offset=${offset}`, options: { headers: key ? { authorization: `Bearer ${key}` } : {} } });
  return { escape, date, shift, range, query, privateLabel, money, exportRequest };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = UsageTools;

const Usage = {
  preset: '7', range: UsageTools.range(7), timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC',
  filters: { provider: '', model: '', account: '', client: '', source: '' },
  offset: 0, limit: 50, summary: null, observations: null, status: null,
  loading: false, loaded: false, error: '', actionError: '', busy: false,
  sequence: 0, credential: null, confirm: null, roots: {}, collectorDraft: '',
};
const ux = UsageTools.escape;
const usageCount = (value) => value == null ? 'Unknown' : new Intl.NumberFormat('en-US').format(value);
const usageSensitive = (value) => ux(UsageTools.privateLabel(value, S.private));
const usageSource = (source) => ({ proxy: 'Proxy', claude_code: 'Claude Code history', codex: 'Codex history' }[source] || source || 'Unknown source');
const usageTime = (value) => value == null ? 'Not reported' : Number.isFinite(new Date(value).getTime()) ? new Date(value).toLocaleString(undefined, { timeZone: Usage.timezone }) : 'Not reported';
const usageButton = (act, label, extra = '', disabled = false) => `<button class="btn" type="button" data-usage-act="${act}" ${extra}${disabled || Usage.busy ? ' disabled' : ''}>${label}</button>`;

function usageHTML() {
  const u = Usage;
  return `<div class="usage-page">
    <div class="page-head"><div><h1 class="h-page">Usage &amp; costs</h1><p class="meta">Persistent metadata · API-equivalent estimates in USD</p></div><span class="grow"></span>${usageButton('refresh', 'Refresh', '', u.loading)}</div>
    ${usageRangeHTML()}
    <p class="meta usage-boundary">Calendar dates in <span class="mono">${ux(u.timezone)}</span> · ${ux(u.range.start)} 00:00 inclusive → ${ux(u.range.end)} 00:00 exclusive</p>
    ${usageFiltersHTML()}
    ${u.loading ? '<p class="usage-message" role="status">Loading usage metadata…</p>' : ''}
    ${u.error ? `<div class="notice usage-message err" role="alert">${S.private ? 'Usage could not be loaded. Try refreshing.' : ux(u.error)} ${usageButton('refresh', 'Try again')}</div>` : ''}
    <div id="usage-results" aria-busy="${u.loading}">${u.loaded && u.summary ? usageTotalsHTML() + usageTrendHTML() + usageObservationsHTML() : !u.loading && !u.error ? '<div class="card pad"><h2 class="h-sec">No usage loaded</h2><p class="meta">Refresh to read persisted metadata.</p></div>' : ''}</div>
    ${usageAllowancesHTML()}
    ${usageSourcesHTML()}
  </div>`;
}

function usageRangeHTML() {
  const u = Usage;
  return `<form id="usage-range" class="usage-range"><div class="seg" role="group" aria-label="Date range">${[['1', 'Today'], ['7', '7 days'], ['30', '30 days'], ['custom', 'Custom']].map(([v, label]) => `<button type="button" data-usage-act="range" data-value="${v}" aria-pressed="${u.preset === v}">${label}</button>`).join('')}</div>
    ${u.preset === 'custom' ? `<label class="usage-field"><span>From</span><input type="date" name="start" value="${ux(u.range.start)}" required></label><label class="usage-field"><span>Through</span><input type="date" name="through" value="${ux(u.range.through)}" required></label><button class="btn" type="submit">Apply dates</button>` : ''}
  </form>`;
}

function usageFacet(k) {
  const f = Usage.summary?.facets || {};
  if (k === 'source') return f.sources || ['proxy', 'claude_code', 'codex'];
  return f[({ provider: 'providers', model: 'models', account: 'accounts', client: 'clients' })[k]] || [];
}
function usageFiltersHTML() {
  return `<div class="usage-filters" role="group" aria-label="Filter usage">${[['provider', 'Provider'], ['model', 'Actual model'], ['account', 'Account'], ['client', 'Client / collector'], ['source', 'Source']].map(([k, label]) => {
    const list = [...usageFacet(k)];
    const selected = Usage.filters[k];
    if (selected && !list.some((v) => (typeof v === 'string' ? v : v.id) === selected)) list.push({ id: selected, label: selected });
    Usage[`${k}Options`] = list;
    return `<label class="usage-field"><span>${label}</span><select data-usage-filter="${k}"><option value="">All</option>${list.map((v, i) => {
      const id = typeof v === 'string' ? v : v.id;
      const name = typeof v === 'string' ? v : v.label || 'Unknown';
      return `<option value="${i}"${id === selected ? ' selected' : ''}>${k === 'account' || k === 'client' ? usageSensitive(name) : ux(k === 'source' ? usageSource(name) : name)}</option>`;
    }).join('')}</select></label>`;
  }).join('')}${usageButton('clear-filters', 'Clear filters')}</div>`;
}

function usageQualityHTML(t) {
  return `<span class="meta">${usageCount(t.unpriced)} unpriced · ${usageCount(t.partial)} partial · ${usageCount(t.missing_usage)} missing usage${t.conflicts != null ? ` · ${usageCount(t.conflicts)} conflicts` : ''}</span>`;
}
function usageTotalsHTML() {
  const s = Usage.summary, p = s.proxy || {};
  const tokens = p.tokens || {};
  return `<section class="sec" aria-labelledby="usage-totals-title"><div class="sec-head"><h2 class="h-sec" id="usage-totals-title">Proxy accounting</h2></div>
    <div class="card pad"><dl class="usage-stats"><div><dt>Logical requests</dt><dd>${usageCount(p.logical_requests)}</dd></div><div><dt>Upstream attempts</dt><dd>${usageCount(p.attempts)}</dd></div><div><dt>Priced estimate</dt><dd>${ux(UsageTools.money(p.estimated_cost_nanos))}</dd></div><div><dt>Input / output tokens</dt><dd>${usageCount(tokens.input)} / ${usageCount(tokens.output)}</dd></div></dl><p class="meta">Attempts include retries. The estimate covers priced observations only; unknown usage and prices are excluded.</p>${usageQualityHTML(p)}</div>
    <div class="usage-source-totals">${(s.sources || []).filter((t) => t.source !== 'proxy').map((t) => `<article class="card pad"><h3>${ux(usageSource(t.source))}</h3><dl class="usage-stats"><div><dt>Imported observations</dt><dd>${usageCount(t.observations)}</dd></div><div><dt>Priced estimate</dt><dd>${ux(UsageTools.money(t.estimated_cost_nanos))}</dd></div></dl>${usageQualityHTML(t)}<p class="meta">${t.possibly_overlapping === false ? 'Source-separated evidence.' : 'Possibly overlaps proxy or other histories.'} No cross-source grand total.</p></article>`).join('')}</div>
    <p class="meta usage-pricing">Pricing catalogue ${ux(s.pricing?.version || Usage.status?.pricing?.version || 'unknown')} · ${ux(s.pricing?.basis || Usage.status?.pricing?.basis || 'Basis not reported')}. This is an API-equivalent estimate, not a subscription bill.</p>
  </section>`;
}

function usageTrendHTML() {
  const trend = Usage.summary.trend || [];
  const groups = [...new Set(trend.map((t) => t.source))];
  return `<section class="sec" aria-labelledby="usage-trend-title"><div class="sec-head"><h2 class="h-sec" id="usage-trend-title">Daily trend</h2><span class="meta">Sources shown separately</span></div>${!trend.length ? '<div class="card pad"><p class="meta">No observations in this date range. Change dates or configure an import below.</p></div>' : groups.map((source) => {
    const rows = trend.filter((r) => r.source === source).sort((a, b) => a.date.localeCompare(b.date));
    const max = Math.max(1, ...rows.map((r) => r.observations || 0));
    return `<div class="card pad usage-trend"><h3>${ux(usageSource(source))}</h3><div class="usage-trend-scroll"><table class="usage-table"><caption class="sr-only">${ux(usageSource(source))} daily observations and estimates</caption><thead><tr><th scope="col">Local date</th><th scope="col">Observations</th><th scope="col">Priced estimate</th><th scope="col">Unpriced</th></tr></thead><tbody>${rows.map((r) => `<tr><th scope="row" class="mono">${ux(r.date)}</th><td><span class="usage-spark" aria-hidden="true"><i style="width:${Math.max(0, (r.observations || 0) / max * 100)}%"></i></span>${usageCount(r.observations)}</td><td class="mono">${ux(UsageTools.money(r.estimated_cost_nanos))}</td><td>${usageCount(r.unpriced)}</td></tr>`).join('')}</tbody></table></div></div>`;
  }).join('')}</section>`;
}

function usageTokenHTML(t = {}) {
  return ['input', 'output', 'cache_read', 'cache_write', 'write_5m', 'write_1h', 'reasoning'].map((k) => `<span>${({ input: 'Input', output: 'Output', cache_read: 'Cache read', cache_write: 'Cache write', write_5m: 'Write 5m', write_1h: 'Write 1h', reasoning: 'Reasoning' })[k]} <span class="mono">${usageCount(t[k])}</span></span>`).join('');
}
function usageObservationsHTML() {
  const o = Usage.observations || { items: [], total: 0 }, items = o.items || [];
  const count = o.total ?? 0, end = Math.min(Usage.offset + items.length, count);
  return `<section class="sec" aria-labelledby="usage-observations-title"><div class="sec-head"><h2 class="h-sec" id="usage-observations-title">Observations</h2><span class="grow"></span>${usageButton('export-csv', 'CSV · this page', '', !items.length || Usage.loading)}${usageButton('export-json', 'JSON · this page', '', !items.length || Usage.loading)}</div>
    <p class="meta">Metadata only. Input excludes cache reads and writes; reasoning is already included in output. Cache TTL counts are subsets of cache writes. Unknown is not zero.</p>
    <div class="card usage-observations">${!items.length ? '<div class="pad"><p class="meta">No matching observations.</p></div>' : `<div class="usage-table-scroll" tabindex="0" role="region" aria-label="Usage observations table"><table class="usage-table"><thead><tr><th scope="col">Time / source</th><th scope="col">Provider / actual model</th><th scope="col">Account / client</th><th scope="col">Reported tokens</th><th scope="col">Estimate / coverage</th></tr></thead><tbody>${items.map((r) => {
      const collector = r.collector_label || (r.origin_id && r.origin_id !== 'local' ? r.origin_id : null);
      return `<tr><td><span class="mono">${ux(usageTime(r.event_at_ms))}</span><span class="meta">${ux(usageSource(r.source))}</span></td><td><span>${ux(r.provider || 'Unknown')}</span><span class="mono">${ux(r.actual_model || 'Unknown')}</span></td><td><span>${usageSensitive(r.account_label || r.account_id)}</span><span class="meta">${usageSensitive(r.client_label || r.client_id || collector)}</span></td><td><details><summary>Input ${usageCount(r.tokens?.input)} · output ${usageCount(r.tokens?.output)}</summary><div class="usage-tokens">${usageTokenHTML(r.tokens)}</div></details></td><td><span class="mono">${ux(UsageTools.money(r.estimated_cost_nanos))}</span><span class="meta">${ux(r.completeness || 'Unknown coverage')} · ${ux(r.state || 'Unknown state')}</span><span class="meta">${ux(r.pricing_basis || 'Pricing basis not reported')}</span></td></tr>`;
    }).join('')}</tbody></table></div>`}</div>
    <div class="usage-pagination"><span class="meta" role="status">${count ? `${Usage.offset + 1}–${end} of ${usageCount(count)}` : '0 observations'} · ${Usage.limit} per page · exports contain this page only</span><span class="grow"></span>${usageButton('previous', 'Previous', '', Usage.offset === 0 || Usage.loading)}${usageButton('next', 'Next', '', end >= count || Usage.loading)}</div>
  </section>`;
}

function usageAllowancesHTML() {
  const accounts = (S.accounts || []).filter(metered);
  return `<section class="sec" aria-labelledby="usage-allowance-title"><div class="sec-head"><h2 class="h-sec" id="usage-allowance-title">Provider allowances</h2><span class="grow"></span>${quotaControlsHTML(false, false, false)}</div>
    <p class="meta">Provider-reported subscription windows. These percentages are independent of API cost estimates.</p><div class="usage-allowances">${accounts.map((a) => `<article class="card pad"><div class="usage-allowance-head">${acctLogo(a, 16)}<h3>${usageSensitive(a.label)}</h3></div>${metersHTML(a)}<p class="meta">Source: ${ux(provName(a))} account quota · updated ${ux(usageTime(a.quota?.updated_at))}</p></article>`).join('') || '<div class="card pad"><p class="meta">No provider allowances reported. Connected API keys do not report subscription limits.</p></div>'}</div>
  </section>`;
}

function usageSourcesHTML() {
  const status = Usage.status, health = status?.health;
  return `<section class="sec" aria-labelledby="usage-sources-title"><div class="sec-head"><h2 class="h-sec" id="usage-sources-title">Sources &amp; collection</h2></div>
    ${Usage.actionError ? `<p class="usage-message err" role="alert">${S.private ? 'The action failed. Retry or check server logs.' : ux(Usage.actionError)}</p>` : ''}
    <div class="card pad usage-health"><h3>Analytics health</h3><p>${ux(health?.state || 'Not loaded')}${health?.message ? ` · ${S.private ? 'See server status for details' : ux(health.message)}` : ''}</p><p class="meta">Queue ${usageCount(health?.queue_depth)} · dropped ${usageCount(health?.dropped)} · writer errors ${usageCount(health?.writer_errors)} · last commit ${ux(usageTime(health?.last_commit_at_ms))}</p><p class="meta">Enqueued proxy observations become durable after commit. Gaps and writer errors can make totals incomplete.</p></div>
    <div class="usage-management"><div class="card pad"><h3>Local history imports</h3><p class="meta">Opt in with a root on the Fusebox server. Imports read usage metadata from Claude Code or Codex histories. No transcript content is stored.</p>${['claude_code', 'codex'].map((source) => {
      const item = (status?.imports || []).find((r) => r.source === source);
      return `<form class="usage-import" data-usage-import="${source}"><h4>${ux(usageSource(source))}</h4><p class="meta">${ux(item?.state || 'Not configured')} · ${item?.enabled ? 'Enabled' : 'Disabled'} · imported ${usageCount(item?.imported)} · last scan ${ux(usageTime(item?.last_scan_at_ms))}</p>${item?.root ? `<p class="mono usage-path">${usageSensitive(item.root)}</p>` : ''}${item?.last_error ? `<p class="err meta">${S.private ? 'Import error; details hidden' : ux(item.last_error)}</p>` : ''}<label class="usage-field"><span>History root on server</span><input name="root" type="${S.private ? 'password' : 'text'}" autocomplete="off" spellcheck="false" placeholder="${S.private ? 'Path hidden · enter a root to replace it' : source === 'claude_code' ? '/path/to/.claude/projects' : '/path/to/.codex/sessions'}" value="${S.private ? '' : ux(Usage.roots[source] ?? item?.root ?? '')}"${Usage.busy ? ' disabled' : ''}></label><div class="usage-actions"><button class="btn" type="submit"${Usage.busy ? ' disabled' : ''}>${item?.enabled ? 'Save root' : 'Enable import'}</button>${usageButton('toggle-import', item?.enabled ? 'Disable' : 'Enable saved root', `data-source="${source}"`, !item?.root)}${usageButton('scan', 'Scan now', `data-source="${source}"`, !item?.enabled)}</div></form>`;
    }).join('')}</div>
    <div class="card pad"><h3>Collectors</h3><p class="meta">A collector sends metadata from another machine. Each credential is shown once. Last contact is a health signal; last sync reports durable ingestion.</p>
    <form id="usage-enroll" class="usage-enroll"><label class="usage-field"><span>Friendly label</span><input name="label" type="${S.private ? 'password' : 'text'}" value="${S.private ? '' : ux(Usage.collectorDraft)}" placeholder="${S.private ? 'Label hidden' : 'Work laptop'}" maxlength="120" required autocomplete="off"${Usage.busy ? ' disabled' : ''}></label><button class="btn" type="submit"${Usage.busy ? ' disabled' : ''}>Enroll collector</button></form>
    ${usageCredentialHTML()}
    <div class="usage-collectors">${(status?.collectors || []).map((c, i) => `<article class="usage-collector"><div class="usage-collector-head"><h4>${usageSensitive(c.label || c.id)}</h4><span class="meta">${c.revoked ? 'Revoked' : ux(c.state || 'Not yet contacted')}</span></div><p class="meta">Last contact ${ux(usageTime(c.last_contact_at_ms))}<br>Last sync ${ux(usageTime(c.last_sync_at_ms))} · pending ${usageCount(c.pending)}</p><div class="usage-actions">${usageButton('rotate', 'Rotate credential', `data-index="${i}"`, !!c.revoked)}${usageButton('revoke', 'Revoke', `data-index="${i}"`, !!c.revoked)}</div></article>`).join('') || '<p class="meta">No collectors enrolled.</p>'}</div>
    ${Usage.confirm ? `<div class="usage-confirm" role="alert"><h4>${Usage.confirm.action === 'revoke' ? 'Revoke' : 'Rotate credential for'} ${usageSensitive(Usage.confirm.label)}?</h4><p>${Usage.confirm.action === 'revoke' ? 'Revoke this collector? New uploads will be rejected; stored metadata remains.' : 'Rotate this credential? The old credential stops working immediately. Update the collector with the replacement.'}</p><div class="usage-actions">${usageButton('confirm', Usage.confirm.action === 'revoke' ? 'Confirm revoke' : 'Confirm rotation')}${usageButton('cancel', 'Cancel')}</div></div>` : ''}
    </div></div>
  </section>`;
}
function usageCredentialHTML() {
  if (!Usage.credential) return '';
  return `<div class="usage-credential" role="status"><h4>One-time enrollment credential</h4><p class="meta">Copy now and configure the collector. Closing this message discards the credential from the dashboard.</p><p class="mono usage-secret">${S.private ? '••••••••••••' : ux(Usage.credential)}</p><div class="usage-actions">${usageButton('copy-credential', 'Copy credential')}${usageButton('dismiss-credential', 'I saved it · dismiss')}</div></div>`;
}

function usageFocusSelector(el) {
  if (!el || !el.closest('.usage-page')) return null;
  if (el.dataset.usageFilter) return `[data-usage-filter="${el.dataset.usageFilter}"]`;
  if (el.dataset.usageAct) return `[data-usage-act="${el.dataset.usageAct}"]${el.dataset.value ? `[data-value="${el.dataset.value}"]` : ''}${el.dataset.source ? `[data-source="${el.dataset.source}"]` : ''}${el.dataset.index ? `[data-index="${el.dataset.index}"]` : ''}`;
  const form = el.closest('form');
  if (form && el.name) return `${form.id ? `#${form.id}` : `[data-usage-import="${form.dataset.usageImport}"]`} [name="${el.name}"]`;
  return null;
}
function usageRender(focusSelector) {
  if (S.route !== 'usage' || S.locked) return;
  const selector = focusSelector || usageFocusSelector(document.activeElement) || Usage.focus;
  Usage.focus = selector;
  render();
  if (selector) document.querySelector(selector)?.focus({ preventScroll: true });
}
async function usageLoad() {
  const u = Usage, sequence = ++u.sequence;
  u.loading = true; u.error = ''; u.loaded = true;
  usageRender();
  try {
    const query = UsageTools.query(u.range, u.filters, u.timezone);
    const results = await Promise.allSettled([api(`/usage/summary?${query}`), api(`/usage/observations?${query}&limit=${u.limit}&offset=${u.offset}`), api('/usage/status')]);
    if (sequence !== u.sequence) return;
    if (results[0].status === 'fulfilled') u.summary = results[0].value;
    else u.summary = null;
    if (results[1].status === 'fulfilled') u.observations = results[1].value;
    else u.observations = null;
    if (results[2].status === 'fulfilled') u.status = results[2].value;
    const failure = results.find((r) => r.status === 'rejected');
    if (failure) throw failure.reason;
  } catch (e) { if (sequence === u.sequence) u.error = e.message === 'locked' ? 'Management authentication required.' : e.message; }
  finally { if (sequence === u.sequence) { u.loading = false; usageRender(); } }
}
async function usageMutate(path, body, after) {
  Usage.busy = true; Usage.actionError = ''; usageRender();
  try {
    const r = await api(path, { method: 'POST', ...(body ? { body: JSON.stringify(body) } : {}) });
    if (after) after(r);
    await usageLoad();
  } catch (e) { Usage.actionError = e.message; }
  finally { Usage.busy = false; usageRender(); }
}
async function usageExport(format) {
  Usage.busy = true; Usage.actionError = ''; usageRender();
  let url;
  try {
    const q = UsageTools.query(Usage.range, Usage.filters, Usage.timezone);
    const request = UsageTools.exportRequest(q, format, S.key, Usage.offset, Usage.limit);
    const res = await fetch(request.url, request.options);
    if (res.status === 401 || res.status === 403) {
      S.locked = res.status === 401 ? 'key' : 'remote'; render();
      throw new Error('Management authentication required.');
    }
    if (!res.ok) throw new Error(`Export failed (${res.status}).`);
    url = URL.createObjectURL(await res.blob());
    const a = document.createElement('a'); a.href = url; a.download = `fusebox-usage-${Usage.range.start}-page-${Math.floor(Usage.offset / Usage.limit) + 1}.${format}`;
    document.body.append(a); a.click(); a.remove();
    // Let the browser consume the download before releasing its object URL.
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    toast(`Downloaded metadata ${format.toUpperCase()} · this page`);
  } catch (e) { Usage.actionError = e.message; }
  finally { Usage.busy = false; usageRender(); }
}
function bindUsage() {
  if (!Usage.loaded && !Usage.loading) usageLoad();
}

if (typeof document !== 'undefined') {
  document.addEventListener('change', (event) => {
    const el = event.target.closest('[data-usage-filter]');
    if (!el) return;
    const k = el.dataset.usageFilter, v = Usage[`${k}Options`][Number(el.value)];
    Usage.filters[k] = el.value === '' ? '' : typeof v === 'string' ? v : v.id;
    Usage.offset = 0; usageLoad();
  });
  document.addEventListener('input', (event) => {
    const form = event.target.closest('[data-usage-import]');
    if (form && event.target.name === 'root') Usage.roots[form.dataset.usageImport] = event.target.value;
    if (event.target.closest('#usage-enroll')) Usage.collectorDraft = event.target.value;
  });
  document.addEventListener('submit', (event) => {
    const form = event.target;
    if (!form.matches('#usage-range, #usage-enroll, [data-usage-import]')) return;
    event.preventDefault();
    if (Usage.busy) return;
    const values = new FormData(form);
    if (form.id === 'usage-range') {
      try {
        const start = values.get('start'), through = values.get('through'), end = UsageTools.shift(through, 1);
        UsageTools.query({ start, end }, {}, Usage.timezone);
        Usage.range = { start, through, end }; Usage.offset = 0; usageLoad();
      } catch (e) { Usage.error = e.message; usageRender(); }
    } else if (form.id === 'usage-enroll') {
      const label = String(values.get('label') || Usage.collectorDraft).trim();
      if (!label) return;
      if (Usage.credential) { Usage.actionError = 'Save and dismiss the current credential before enrolling another collector.'; usageRender(); return; }
      usageMutate('/usage/collectors', { label }, (r) => { Usage.credential = r.credential || null; Usage.collectorDraft = ''; });
    } else {
      const source = form.dataset.usageImport;
      const current = (Usage.status?.imports || []).find((r) => r.source === source);
      const root = String(values.get('root') || Usage.roots[source] || current?.root || '').trim();
      if (!root) { Usage.actionError = 'Enter a history root on the server.'; usageRender(); return; }
      usageMutate('/usage/imports', { source, root, enabled: true }, () => { delete Usage.roots[source]; });
    }
  });
  document.addEventListener('click', (event) => {
    const el = event.target.closest('[data-usage-act]');
    if (!el || el.disabled) return;
    const act = el.dataset.usageAct;
    if (act === 'refresh') { if (Usage.preset !== 'custom') Usage.range = UsageTools.range(Number(Usage.preset)); return usageLoad(); }
    if (act === 'range') {
      Usage.preset = el.dataset.value;
      if (Usage.preset === 'custom') return usageRender('#usage-range [name="start"]');
      Usage.range = UsageTools.range(Number(Usage.preset)); Usage.offset = 0; return usageLoad();
    }
    if (act === 'clear-filters') { Object.keys(Usage.filters).forEach((k) => { Usage.filters[k] = ''; }); Usage.offset = 0; return usageLoad(); }
    if (act === 'previous' || act === 'next') { Usage.offset = Math.max(0, Usage.offset + (act === 'next' ? Usage.limit : -Usage.limit)); return usageLoad(); }
    if (act.startsWith('export-')) return usageExport(act.slice(7));
    if (act === 'scan') return usageMutate('/usage/imports/scan', { source: el.dataset.source });
    if (act === 'toggle-import') {
      const item = (Usage.status?.imports || []).find((r) => r.source === el.dataset.source);
      if (item) return usageMutate('/usage/imports', { source: item.source, root: item.root, enabled: !item.enabled });
    }
    if (act === 'revoke' || act === 'rotate') {
      if (Usage.credential && act === 'rotate') { Usage.actionError = 'Save and dismiss the current credential before rotating another.'; return usageRender(); }
      const collector = Usage.status?.collectors?.[Number(el.dataset.index)];
      if (collector) Usage.confirm = { action: act, id: collector.id, label: collector.label || collector.id };
      return usageRender('[data-usage-act="confirm"]');
    }
    if (act === 'cancel') { Usage.confirm = null; return usageRender('#usage-enroll input'); }
    if (act === 'confirm' && Usage.confirm) {
      const c = Usage.confirm; Usage.confirm = null;
      return usageMutate(`/usage/collectors/${encodeURIComponent(c.id)}/${c.action}`, null, (r) => { if (c.action === 'rotate') Usage.credential = r.credential || null; });
    }
    if (act === 'dismiss-credential') { Usage.credential = null; return usageRender('#usage-enroll input'); }
    if (act === 'copy-credential') navigator.clipboard.writeText(Usage.credential).then(() => toast('Enrollment credential copied')).catch(() => { Usage.actionError = 'Clipboard unavailable. Turn privacy off and select the credential to copy it.'; usageRender(); });
  });
}

// Refresh persistent analytics while the page is open; never interrupts a source action.
if (typeof document !== 'undefined') setInterval(() => {
  if (S.route !== 'usage' || S.locked || Usage.loading || Usage.busy || document.hidden) return;
  if (Usage.preset !== 'custom') Usage.range = UsageTools.range(Number(Usage.preset));
  usageLoad();
}, 60000);
