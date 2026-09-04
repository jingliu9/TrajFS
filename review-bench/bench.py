import os, sys, time, tarfile, subprocess, hashlib, random, io, json
S='/tmp/claude-20098/scratch-trajfs'
RUN='/workspace/farm/onesw-gen-outputs/suite-traj-log-summary-replicas/claude-opus-4.8-678xazw/onesw-generation-20260902T033145Z'
os.chdir(RUN)
paths=[l.rstrip('\n') for l in open(f'{S}/sample-list.txt') if l.strip()]
paths=[p for p in paths if os.path.isfile(p) and not os.path.islink(p)]
R={}
def t(): return time.perf_counter()
# ---------- read everything once (page cache warm for all contestants)
t0=t(); data={p:open(p,'rb').read() for p in paths}; R['read_all_s']=t()-t0
R['files']=len(paths); R['bytes']=sum(len(v) for v in data.values())
# ---------- A. tar + zstd
t0=t()
with tarfile.open(f'{S}/sample.tar','w') as tf:
    for p in paths: tf.add(p, recursive=False)
R['tar_build_s']=t()-t0
t0=t(); subprocess.run(['zstd','-q','-f','-T0','-3',f'{S}/sample.tar','-o',f'{S}/sample.tar.zst'],check=True); R['tar_zstd3_s']=t()-t0
t0=t(); subprocess.run(['zstd','-q','-f','-T0','-19','--long=27',f'{S}/sample.tar','-o',f'{S}/sample.tar.zst19'],check=True); R['tar_zstd19_s']=t()-t0
R['tar_bytes']=os.path.getsize(f'{S}/sample.tar'); R['tar_zst3_bytes']=os.path.getsize(f'{S}/sample.tar.zst'); R['tar_zst19_bytes']=os.path.getsize(f'{S}/sample.tar.zst19')
random.seed(1); probes=random.sample(paths,20)
# point read from tar.zst without index (stream)
t0=t()
for p in probes[:3]:
    subprocess.run(f"zstd -dc {S}/sample.tar.zst | tar -xOf - '{p}' > /dev/null", shell=True, check=True)
R['tarzst_cat_noindex_s_each']=(t()-t0)/3
# ls a directory via tar listing (no index)
d='rounds/round-0037/builder/workspace/logs/round10'
t0=t(); out=subprocess.run(f"zstd -dc {S}/sample.tar.zst | tar -tf - | grep -c '^{d}/'", shell=True, capture_output=True, text=True); R['tarzst_ls_noindex_s']=t()-t0
# point read with an offset index over the UNCOMPRESSED tar (what ratarmount/tarfs style index gives)
t0=t(); idx={}
with tarfile.open(f'{S}/sample.tar') as tf:
    for m in tf: idx[m.name]=(m.offset_data,m.size)
R['tar_index_build_s']=t()-t0
t0=t()
with open(f'{S}/sample.tar','rb') as f:
    for p in probes:
        off,sz=idx[p]; f.seek(off); assert f.read(sz)==data[p]
R['tar_cat_indexed_s_each']=(t()-t0)/len(probes)
# ---------- B. parquet + duckdb
import duckdb, pyarrow as pa, pyarrow.parquet as pq
t0=t()
dirs=[os.path.dirname(p) for p in paths]; names=[os.path.basename(p) for p in paths]
tbl=pa.table({'path':paths,'dir':dirs,'name':names,'size':[len(data[p]) for p in paths],'content':pa.array([data[p] for p in paths],pa.binary())})
pq.write_table(tbl,f'{S}/sample.parquet',compression='zstd',compression_level=3,row_group_size=4096)
R['parquet_build_s']=t()-t0; R['parquet_bytes']=os.path.getsize(f'{S}/sample.parquet')
# dedupe variant: files(path->sha) + blobs(sha->content)
t0=t(); shas=[hashlib.sha256(data[p]).digest() for p in paths]; uniq={}
for p,s in zip(paths,shas): uniq.setdefault(s,data[p])
pq.write_table(pa.table({'path':paths,'dir':dirs,'name':names,'size':[len(data[p]) for p in paths],'sha':pa.array(shas,pa.binary(32))}),f'{S}/files.parquet',compression='zstd',row_group_size=8192)
pq.write_table(pa.table({'sha':pa.array(list(uniq.keys()),pa.binary(32)),'content':pa.array(list(uniq.values()),pa.binary())}),f'{S}/blobs.parquet',compression='zstd',compression_level=3,row_group_size=2048)
R['parquet_dedup_build_s']=t()-t0; R['parquet_files_bytes']=os.path.getsize(f'{S}/files.parquet'); R['parquet_blobs_bytes']=os.path.getsize(f'{S}/blobs.parquet'); R['unique_blobs']=len(uniq); R['unique_bytes']=sum(len(v) for v in uniq.values())
con=duckdb.connect()
con.execute(f"create view f as select * from '{S}/sample.parquet'")
con.execute(f"create view files as select * from '{S}/files.parquet'"); con.execute(f"create view blobs as select * from '{S}/blobs.parquet'")
def timed(q,n=5,params=None):
    ts=[]
    for _ in range(n):
        t0=t(); con.execute(q,params or []).fetchall(); ts.append(t()-t0)
    return min(ts)
R['duck_ls_s']=timed("select name,size from f where dir=?",params=[d])
R['duck_ls_rows']=len(con.execute("select name from f where dir=?",[d]).fetchall())
R['duck_cat_s']=timed("select content from f where path=?",params=[probes[0]])
R['duck_cat_dedup_s']=timed("select b.content from files x join blobs b using(sha) where x.path=?",params=[probes[0]])
R['duck_find_s']=timed("select count(*) from f where name='COMPLETE'")
R['duck_grep_s']=timed("select count(*) from f where name like '%.stdout' and contains(content::varchar,'Traceback')")
R['duck_grep_rows']=con.execute("select count(*) from f where name like '%.stdout' and contains(content::varchar,'Traceback')").fetchone()[0]
R['duck_dirsizes_s']=timed("select split_part(path,'/',3) as top, count(*), sum(size) from f group by 1")
# grep baseline on the real files
t0=t(); subprocess.run(f"grep -rl --include='*.stdout' Traceback rounds/round-0037 > /dev/null", shell=True); R['grep_r_files_s']=t()-t0
t0=t(); subprocess.run(f"find rounds/round-0037 -name COMPLETE | wc -l > /dev/null", shell=True); R['find_files_s']=t()-t0
t0=t(); subprocess.run(f"ls {d} > /dev/null", shell=True); R['ls_files_s']=t()-t0
# ---------- C. squashfs (mksquashfs, zstd)
t0=t(); subprocess.run(f"mksquashfs rounds/round-0037 {S}/sample.sqfs -comp zstd -Xcompression-level 3 -b 128K -noappend -quiet -no-progress -ef <(grep -v -x -F -f {S}/sample-list.txt /dev/null) > /dev/null 2>&1", shell=True, executable='/bin/bash')
R['squashfs_build_s']=t()-t0; R['squashfs_bytes']=os.path.getsize(f'{S}/sample.sqfs') if os.path.exists(f'{S}/sample.sqfs') else None
json.dump(R,open(f'{S}/bench.json','w'),indent=1); print(json.dumps(R,indent=1))
