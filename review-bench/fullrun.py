import os, sys, time, hashlib, json
from multiprocessing import Pool
import pyarrow as pa, pyarrow.parquet as pq
S='/tmp/claude-20098/scratch-trajfs'
RUN='/workspace/farm/onesw-gen-outputs/suite-traj-log-summary-replicas/claude-opus-4.8-678xazw/onesw-generation-20260902T033145Z'
os.chdir(RUN)
paths=[l.rstrip('\n') for l in open('/tmp/claude-20098/list_claude-opus-4.8-678xazw.txt') if l.strip()]
def h(p):
    try:
        if os.path.islink(p) or not os.path.isfile(p): return None
        b=open(p,'rb').read(); return (p,len(b),hashlib.sha256(b).digest())
    except Exception: return None
t0=time.time()
with Pool(16) as pool: rows=[r for r in pool.imap(h,paths,chunksize=2000) if r]
t_hash=time.time()-t0
paths=[r[0] for r in rows]; sizes=[r[1] for r in rows]; shas=[r[2] for r in rows]
t0=time.time()
pq.write_table(pa.table({'path':paths,'dir':[os.path.dirname(p) for p in paths],'name':[os.path.basename(p) for p in paths],'size':sizes,'sha':pa.array(shas,pa.binary(32))}),f'{S}/rank1-files.parquet',compression='zstd',row_group_size=65536)
t_files=time.time()-t0
uniq={}
for p,s,sha in rows: uniq.setdefault(sha,(p,s))
usize=sum(s for _,s in uniq.values())
# write unique blobs into ~256MB-raw shards
t0=time.time(); shard=0; buf_s=[]; buf_c=[]; acc=0; shard_bytes=[]
def flush():
    global shard,buf_s,buf_c,acc
    if not buf_s: return
    fn=f'{S}/rank1-blobs-{shard:04d}.parquet'
    pq.write_table(pa.table({'sha':pa.array(buf_s,pa.binary(32)),'content':pa.array(buf_c,pa.binary())}),fn,compression='zstd',compression_level=3,row_group_size=2048)
    shard_bytes.append(os.path.getsize(fn)); shard+=1; buf_s=[]; buf_c=[]; acc=0
for sha,(p,s) in uniq.items():
    buf_s.append(sha); buf_c.append(open(p,'rb').read()); acc+=s
    if acc>=256<<20: flush()
flush(); t_blobs=time.time()-t0
R=dict(files=len(rows),bytes=sum(sizes),hash_s=t_hash,unique_blobs=len(uniq),unique_bytes=usize,files_parquet_bytes=os.path.getsize(f'{S}/rank1-files.parquet'),files_parquet_s=t_files,blob_shards=shard,blob_shard_bytes_total=sum(shard_bytes),blobs_s=t_blobs)
json.dump(R,open(f'{S}/fullrun.json','w'),indent=1); print(json.dumps(R,indent=1))
