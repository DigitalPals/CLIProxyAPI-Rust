#!/usr/bin/env python3
"""Synthetic import/query and local HTTP proxy measurements. No paid upstreams."""
import argparse, datetime, http.server, json, os, pathlib, platform, socket, statistics, subprocess, tempfile, threading, time, urllib.request

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--binary',default='target/release/fusebox');ap.add_argument('--records',type=int,default=10000);ap.add_argument('--requests',type=int,default=200);args=ap.parse_args();binary=str(pathlib.Path(args.binary).resolve())
    with tempfile.TemporaryDirectory(prefix='fusebox-usage-bench-') as d:
        root=pathlib.Path(d);(root/'auth').mkdir();history=root/'history';history.mkdir()
        now=datetime.datetime.now(datetime.timezone.utc).isoformat();file=history/'generated.jsonl'
        with file.open('w') as f:
            for i in range(args.records):f.write(json.dumps({'type':'assistant','uuid':f'row-{i}','sessionId':'synthetic-bench','timestamp':now,'message':{'id':f'msg-bench-{i}','model':'claude-sonnet-5-5','content':[{'type':'text','text':'synthetic private content excluded'}],'usage':{'input_tokens':1000,'cache_read_input_tokens':500,'cache_creation_input_tokens':0,'output_tokens':200,'service_tier':'standard'}}})+'\n')
        class Provider(http.server.BaseHTTPRequestHandler):
            counter=0
            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length',0)));Provider.counter+=1
                data=json.dumps({'id':f'resp-bench-{Provider.counter}','model':'gpt-6.1-sol','object':'response','status':'completed','service_tier':'default','output':[],'usage':{'input_tokens':100,'input_tokens_details':{'cached_tokens':10,'cache_write_tokens':0},'output_tokens':20}}).encode()
                self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
            def log_message(self,*_):pass
        upstream=http.server.ThreadingHTTPServer(('127.0.0.1',0),Provider);threading.Thread(target=upstream.serve_forever,daemon=True).start()
        def call(base,path,body=None):
            req=urllib.request.Request(base+path,data=json.dumps(body).encode() if body is not None else None,headers={'Content-Type':'application/json'})
            with urllib.request.urlopen(req,timeout=120) as r:return json.loads(r.read()) if path!='/healthz' else r.read()
        def start(enabled):
            with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
            cfg=root/'config.yaml';cfg.write_text(f'host: 127.0.0.1\nport: {port}\nauth-dir: "{root}/auth"\nproxy-url: direct\nusage:\n  enabled: {str(enabled).lower()}\n  database: "{root}/usage.sqlite3"\ncodex-api-key:\n  - api-key: synthetic-only\n    base-url: http://127.0.0.1:{upstream.server_port}/v1\n')
            p=subprocess.Popen([binary,'--config',str(cfg)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);base=f'http://127.0.0.1:{port}'
            for _ in range(200):
                if p.poll() is not None:raise RuntimeError('benchmark server exited')
                try:call(base,'/healthz');break
                except OSError:time.sleep(.05)
            return p,base
        def stop(p):p.terminate();p.wait(timeout=20)
        results={'environment':{'system':platform.platform(),'cpu_count':os.cpu_count(),'binary':binary},'records':args.records,'fixture_bytes':file.stat().st_size,'http_requests_per_sample':args.requests,'http_samples':{}}
        for enabled in [False,True]:
            p,base=start(enabled)
            try:
                for _ in range(20):call(base,'/v1/responses',{'model':'gpt-6.1-sol','input':[]})
                samples=[]
                for _ in range(3):
                    started=time.perf_counter()
                    for _ in range(args.requests):call(base,'/v1/responses',{'model':'gpt-6.1-sol','input':[]})
                    samples.append((time.perf_counter()-started)*1000/args.requests)
                results['http_samples']['analytics_on' if enabled else 'analytics_off']={'mean_ms_each_sample':samples,'median_ms':statistics.median(samples)}
                if enabled:
                    call(base,'/api/usage/imports',{'source':'claude_code','root':str(history),'enabled':True});started=time.perf_counter();scans=0
                    while scans<args.records//1000+10:
                        call(base,'/api/usage/imports/scan',{'source':'claude_code'});scans+=1
                        summary=call(base,'/api/usage/summary');source=next((s for s in summary['sources'] if s['source']=='claude_code'),{})
                        if source.get('observations')==args.records:break
                    assert source.get('observations')==args.records,source
                    results['import']={'seconds':time.perf_counter()-started,'scan_calls':scans,'observations':source['observations']}
                    lat=[]
                    for _ in range(10):t=time.perf_counter();call(base,'/api/usage/summary');lat.append(1000*(time.perf_counter()-t))
                    results['summary_query']={'samples':10,'median_ms':statistics.median(lat),'max_ms':max(lat)}
                    status=call(base,'/api/usage/status');assert status['health']['dropped']==0,status;results['health']=status['health']
            finally:stop(p)
        upstream.shutdown();results['sqlite_bytes']=(root/'usage.sqlite3').stat().st_size
        off=results['http_samples']['analytics_off']['median_ms'];on=results['http_samples']['analytics_on']['median_ms'];results['observed_proxy_difference_ms']=on-off
        results['methodology']='Sequential local HTTP through loopback synthetic provider; 20 warmup requests then 3 samples per mode; local OS/process noise included. Import repeated bounded scans including API query overhead. No external model calls.'
        print(json.dumps(results,indent=2))
if __name__=='__main__':main()
