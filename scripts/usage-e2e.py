#!/usr/bin/env python3
"""Isolated real-process collector acceptance; uses only standard-library tooling.
Run: python3 scripts/usage-e2e.py --binary target/debug/fusebox
No personal history, provider calls or existing installations are used.
"""
import argparse, concurrent.futures, datetime, json, os, pathlib, shutil, socket, sqlite3, subprocess, tempfile, time, urllib.error, urllib.parse, urllib.request, zoneinfo

MARKER = 'PRIVATE_SYNTHETIC_PROMPT_AND_TOOL_SECRET'
KEY = 'synthetic-management-only'

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--binary',default='target/debug/fusebox');args=ap.parse_args()
    binary=str(pathlib.Path(args.binary).resolve());root=pathlib.Path(tempfile.mkdtemp(prefix='fusebox-usage-e2e-'))
    with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    base=f'http://127.0.0.1:{port}';process=None;log=root/'server.log';checks=[]
    config=root/'config.yaml';config.write_text(f'host: 127.0.0.1\nport: {port}\nauth-dir: "{root}/empty-auth"\nmanagement-key: {KEY}\napi-keys: [synthetic-inference]\nproxy-url: direct\nusage:\n  database: "{root}/usage.sqlite3"\n  retention-days: 3650\n')
    (root/'empty-auth').mkdir()
    def request(path,body=None,key=KEY,raw=None,expected=200):
        data=raw if raw is not None else json.dumps(body).encode() if body is not None else None
        req=urllib.request.Request(base+path,data=data,headers={'Authorization':'Bearer '+key,'Content-Type':'application/json'})
        try:
            with urllib.request.urlopen(req,timeout=30) as r: status=r.status;data=r.read()
        except urllib.error.HTTPError as e:status=e.code;data=e.read()
        assert status==expected,(path,status,data[:100])
        try:return json.loads(data)
        except ValueError:return data.decode()
    def start():
        nonlocal process
        handle=log.open('ab');process=subprocess.Popen([binary,'--config',str(config),'serve'],stdout=handle,stderr=handle);handle.close()
        for _ in range(200):
            if process.poll() is not None:raise RuntimeError('isolated server stopped: '+log.read_text()[-1000:])
            try:request('/healthz');return
            except (OSError,AssertionError):time.sleep(.05)
        raise RuntimeError('isolated server did not start')
    def stop():
        nonlocal process
        if process and process.poll() is None:
            process.terminate()
            try:process.wait(timeout=15)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        process=None
    def cli(*parts,expected=0):
        r=subprocess.run([binary,*map(str,parts)],capture_output=True,text=True,timeout=60)
        assert r.returncode==expected,(parts,r.returncode,r.stderr[-400:])
        assert MARKER not in r.stdout+r.stderr
        return json.loads(r.stdout) if r.stdout.strip().startswith('{') else r.stdout
    def stamp():return datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00','Z')
    def claude(path,msg,at=None):
        row={'type':'assistant','uuid':'row-'+msg,'sessionId':'session-'+msg,'timestamp':at or stamp(),'cwd':'/PRIVATE_PROJECT_PATH','requestId':'req-'+msg,'message':{'id':msg,'model':'claude-sonnet-5-5','role':'assistant','content':[{'type':'text','text':MARKER},{'type':'tool_use','input':{'secret':MARKER}}],'usage':{'input_tokens':100,'output_tokens':50,'cache_read_input_tokens':20,'cache_creation_input_tokens':0,'cache_creation':{'ephemeral_5m_input_tokens':0,'ephemeral_1h_input_tokens':0},'service_tier':'standard'}}}
        with path.open('a') as f:f.write(json.dumps(row)+'\n')
    try:
        start();states=[];tokens=[];ids=[];histories=[]
        for i in range(2):
            h=root/f'history-{i}';h.mkdir();history=h/'session.jsonl';claude(history,f'msg-{i}');histories.append(history)
            enrollment=request('/api/usage/collectors',{'label':f'Test collector {i} <safe>'});token=enrollment['credential'];tokens.append(token);ids.append(enrollment['collector']['id']);credential=root/f'credential-{i}';credential.write_text(token);credential.chmod(0o600)
            state=root/f'collector-{i}';states.append(state)
            cli('collector','enroll','--state-dir',state,'--destination',base+'/api/usage-ingest','--credential-file',credential,'--claude-root',h)
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:list(pool.map(lambda s:cli('collector','run','--once','--state-dir',s),states))
        dashboard=request('/api/usage/dashboard?source=claude_code');assert dashboard['combined']['totals']['observations']==2,dashboard
        assert request('/api/usage/observations?source=proxy')['total']==0;checks.append('two isolated collector identities/processes durably synchronized without inventing proxy requests')
        filtered=request('/api/usage/dashboard?source=claude_code&client=collector:'+ids[0]);assert filtered['combined']['totals']['observations']==1,filtered
        assert filtered['combined']['breakdowns']['client'][0]['id']=='collector:'+ids[0]
        assert filtered['facets']['clients'][0]['label']=='Test collector 0 <safe>'
        assert request('/api/usage/observations?client=collector:'+ids[0])['total']==1
        checks.append('collector identity filtering, breakdown and friendly label')
        for state in states:cli('collector','run','--once','--state-dir',state)
        assert request('/api/usage/dashboard?source=claude_code')['combined']['totals']['observations']==2;checks.append('replay does not inflate combined imported totals')
        stop();claude(histories[0],'msg-offline');cli('collector','run','--once','--state-dir',states[0],expected=1)
        offline=cli('collector','status','--state-dir',states[0]);assert offline['collector']['outbox']['pending']==1, offline;start();cli('collector','run','--once','--state-dir',states[0]);assert request('/api/usage/dashboard?source=claude_code')['combined']['totals']['observations']==3;checks.append('offline outbox survives separate process restart and reconnect')
        # Copied native event on another machine is source-idempotent, not a new request.
        shutil.copyfile(histories[0],histories[1].parent/'copied.jsonl');cli('collector','run','--once','--state-dir',states[1]);assert request('/api/usage/dashboard?source=claude_code')['combined']['totals']['observations']==3;checks.append('copied histories across collectors deduplicate using message evidence')
        retired=request('/api/usage/summary?timezone=invalid',expected=410)
        assert retired['code']=='usage_summary_retired' and retired['replacement']=='/api/usage/dashboard',retired
        request('/api/usage/summary',key='synthetic-inference',expected=401);request('/api/usage/dashboard',key='synthetic-inference',expected=401);request('/api/accounts',key=tokens[0],expected=401);request('/v1/models',key=tokens[0],expected=401)
        checks.append('retired summary returns 410 without query work and remains management authenticated')
        request('/api/usage-ingest',{'version':1,'observations':[]},key=KEY,expected=401)
        request('/api/usage-ingest',raw=b'x'*(600*1024),key=tokens[0],expected=413);checks.append('management/inference separation, wrong credential and oversized batch rejection')
        request('/api/usage/collectors/'+ids[0]+'/revoke',{});claude(histories[0],'msg-revoked');cli('collector','run','--once','--state-dir',states[0],expected=1);request('/api/usage-ingest',{'version':1,'observations':[]},key=tokens[0],expected=401);checks.append('revocation rejects upload and retains offline data')
        rotated=request('/api/usage/collectors/'+ids[1]+'/rotate',{});request('/api/usage-ingest',{'version':1,'observations':[]},key=tokens[1],expected=401);credential=root/'rotated';credential.write_text(rotated['credential']);credential.chmod(0o600);cli('collector','enroll','--state-dir',states[1],'--destination',base+'/api/usage-ingest','--credential-file',credential);cli('collector','run','--once','--state-dir',states[1]);checks.append('credential rotation and local re-enrollment preserve idempotency')
        # Server-side history import dated 10 days ago lands priced on its own local date.
        history=root/'server-history';history.mkdir();tz=zoneinfo.ZoneInfo('Europe/Amsterdam')
        old=(datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta(days=10)).replace(hour=22,minute=30,second=0,microsecond=0)
        claude(history/'old.jsonl','msg-ten-days-ago',old.isoformat().replace('+00:00','Z'));day=old.astimezone(tz).date().isoformat()
        request('/api/usage/imports',{'source':'claude_code','root':str(history),'enabled':True});request('/api/usage/imports/scan',{'source':'claude_code'})
        today=datetime.datetime.now(tz).date();query=urllib.parse.urlencode({'start':(today-datetime.timedelta(days=29)).isoformat(),'end':(today+datetime.timedelta(days=1)).isoformat(),'timezone':'Europe/Amsterdam'})
        combined=request('/api/usage/dashboard?'+query)['combined'];entries=[t for t in combined['trend'] if t['date']==day]
        assert entries and all(t['unpriced']==0 and (t['estimated_cost_nanos'] or 0)>0 for t in entries) and sum(t['observations'] for t in entries)==1,(day,combined['trend'])
        assert combined['totals']['history_only']>=1 and combined['proxy_first_event_at_ms'] is None,combined['totals']
        records=request('/api/usage/observations?view=combined&limit=50&offset=0&'+query);assert records['total']==combined['totals']['observations'] and any(i['origin_label']=='This server' for i in records['items']),records['total']
        checks.append('imported history from 10 days ago is priced on its local date in the combined trend')
        local=cli('collector','status','--state-dir',states[1]);assert local['collector']['outbox']['storage']['journal_mode']=='delete';assert local['collector']['outbox']['storage']['database_limit_bytes']==64*1024*1024
        status=request('/api/usage/status');assert isinstance(status['imports'],list) and isinstance(status['collectors'],list)
        assert all(c['last_contact_at_ms'] and c['last_sync_at_ms'] for c in status['collectors']);assert all(any(p['source']=='claude_code' and p['imported']>=1 and p['last_scan_at_ms'] for p in c['progress']) for c in status['collectors']);assert all(t not in json.dumps(status) for t in tokens)
        for format in ['json','csv']:
            export=request('/api/usage/export?format='+format+'&limit=50&offset=0&timezone=Europe%2FAmsterdam&source=claude_code');assert MARKER not in str(export) and 'PRIVATE_PROJECT_PATH' not in str(export)
        stop();assert cli('usage','reprice','--database',root/'usage.sqlite3')=={'repriced':0};checks.append('usage reprice CLI is idempotent on an already priced database')
        for database in root.rglob('*.sqlite3'):
            db=sqlite3.connect(database);dump='\n'.join(db.iterdump());db.close();assert MARKER not in dump and 'PRIVATE_PROJECT_PATH' not in dump
        assert MARKER not in log.read_text();checks.append('marker transcript/tool/project content absent from databases, exports and logs')
        print(json.dumps({'passed':len(checks),'checks':checks,'isolated_root':str(root)},indent=2))
    finally:stop();shutil.rmtree(root)
if __name__=='__main__':main()
