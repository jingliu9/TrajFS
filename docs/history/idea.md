> Raw brainstorming transcript (September 2026) that preceded the TrajFS design; it opens mid-conversation and is kept unedited.
> The reviewed conclusions are in `idea-review.md`; the live design documents are `../PLAN.md` and `../PLAN-fuse.md`.

my issues is that the reposotiry contins too many small but duplicate files, what i want is
that when committing to github, use tar with trunked size (friendly for commit speed, tunable, better to measure and design for gits)
and then i still need to be able to read those files later, by using file operations, such as ls, and be opened by vscode editor to individually review them and potentially perform anslysis.
the goal is to make the git commit and push/pull faster while retaining those capability to view them via files, i am not sure if an olap data base



-----
对，我觉得你这个方向其实比“直接用 tar”更合理。**tar 的主要价值是简单、标准、顺序写友好，不是因为它本身是最适合 millions-of-small-files 的格式。**

tar 的好处主要有几个：格式极其简单，几乎就是 `header + payload + padding` 顺序排列；生成时天然 append-friendly；各种语言、工具都能读；作为 Git 里的一个大 blob 也很自然；如果 trajectory 基本 write-once，打包成本很低。所以它很适合当 **V0 / interchange format**。

但它的问题也很明显：它没有真正为 filesystem workload 设计 metadata layout。原始 tar 里找一个文件，本质需要知道 offset；`ls directory` 也没有原生 directory index；压缩整个 tar 后 random access 更麻烦。因此最后还是得额外构建：

```text
path -> offset, length
directory -> children
```

一旦你已经准备自己维护这些 metadata，那么 tar 本身其实就只剩下“payload 的顺序容器”作用了。

你说的：

> 把文件结构直接 cache 下来，每个 directory compress

我觉得更接近正确答案，但我会稍微改成：

> **directory metadata 单独 compact 存储，file data 按 directory / locality 分 chunk 压缩。**

比如一个 trajectory：

```text
/run42/
    traj-001/
        prompt.json
        response.json
        stdout
        files/
            a.py
            b.py
    traj-002/
        ...
```

物理上可以变成：

```text
run42.pack
├── superblock
├── directory index
│
├── dir-block: /
├── dir-block: /traj-001
├── dir-block: /traj-001/files
├── dir-block: /traj-002
│
├── data-chunk-0001.zstd
├── data-chunk-0002.zstd
├── data-chunk-0003.zstd
└── ...
```

其中一个 directory block 可以直接是：

```text
directory inode = 1821

entries:
    prompt.json     file inode 9912
    response.json   file inode 9913
    stdout          file inode 9914
    files           dir  inode 9915
```

于是：

```bash
ls /run42/traj-001
```

只需要读：

```text
directory block 1821
```

而不是遍历 tar。

这和真正 filesystem 的设计就非常接近了。

---

### 我不太建议“整个 directory 压成一个 blob”

例如：

```text
traj-001/
    10000 files
```

直接：

```text
traj-001.zstd
```

有一个问题：

```bash
vim traj-001/foo.json
```

可能需要解压整个 directory blob。

即使目录只有 100 MB，这也很浪费。

更合理的是：

```text
directory = metadata locality
compression chunk = data locality
```

两者不要完全绑定。

例如：

```text
/traj-001/
   10000 files
```

它的 metadata 可以是一整个 compact directory block：

```text
~几百 KB
```

但 contents 分成：

```text
chunk A: 2 MB compressed
chunk B: 2 MB compressed
chunk C: 2 MB compressed
...
```

每个文件记录：

```text
inode 9912:
    chunk = 183
    offset_in_chunk = 81722
    compressed_chunk_size = ...
    file_size = 1829
```

这样随机打开一个小文件：

```text
lookup path
    ↓
directory metadata
    ↓
inode
    ↓
chunk 183
    ↓
decompress 1–4 MB
    ↓
extract 1.8 KB file
```

这其实和很多现代 compressed filesystem 的思路很类似。

---

## 更进一步，你甚至不需要存完整 path

这是很重要的。

最 naive 的 manifest：

```text
/run42/traj-001/step-001/foo.json
/run42/traj-001/step-002/foo.json
...
```

会重复大量字符串。

更好的数据库/filesystem-style representation：

```text
inode 1: /
inode 2: run42
inode 3: traj-001
inode 4: step-001
inode 5: foo.json
```

directory table：

```text
(parent_inode, name) -> child_inode
```

例如：

```text
(1, "run42")    -> 2
(2, "traj-001") -> 3
(3, "step-001") -> 4
(4, "foo.json") -> 5
```

然后 inode table：

```text
inode 5:
    type = file
    chunk_id = 827
    offset = 12891
    size = 3817
```

这样 metadata 会非常 compact。

而：

```bash
ls /run42/traj-001
```

直接做：

```text
children(parent_inode=3)
```

根本没有 full-path hashing。

---

# 如果你的目录天然对应 trajectory，这就更漂亮

我猜你实际上可能是：

```text
run
 ├── trajectory
 │    ├── messages
 │    ├── environment
 │    ├── tool outputs
 │    └── files
```

那么可以让 **trajectory 本身成为 locality / compression unit**。

比如：

```text
run.pack

trajectory 0001
 ├── metadata
 ├── dir tree
 └── compressed chunks

trajectory 0002
 ├── metadata
 ├── dir tree
 └── compressed chunks
```

top-level index：

```text
trajectory_id -> region in pack
```

然后：

```text
open("/run/traj001/foo/bar")
```

变成：

```text
traj001 index
   ↓
directory tree
   ↓
chunk offset
```

这样每个 trajectory 在物理上也是 clustered 的。

对于 agent workload，这可能比通用 filesystem 更好，因为你的访问通常也有 locality：

```text
load one trajectory
inspect its messages
read several artifacts
```

而不是随机在 10M trajectories 之间跳。

---

## Compression granularity 是最关键的 knob

你其实会有一个很经典的 tradeoff：

### 每个 file 独立压缩

```text
file A → zstd
file B → zstd
file C → zstd
```

优点：

* random read 极好；
* 解压放大最小；
* 文件修改/重写独立。

缺点：

* tiny files compression ratio 差；
* 每文件 framing overhead；
* 很难利用跨文件重复内容。

---

### 每个 directory 整体压缩

```text
directory → zstd
```

优点：

* compression ratio 很好；
* metadata/data locality 好。

缺点：

* random file read amplification 很大。

---

### 我更推荐中间态

比如：

```text
directory-aware 1–4 MB chunks
```

生成时：

```text
for each directory:
    group nearby small files
    until uncompressed size ~= 2 MB
    compress chunk
```

例如：

```text
traj42/
├── 700 tiny JSON files
│
├── chunk 100: first ~2 MB
├── chunk 101: next ~2 MB
└── chunk 102
```

大文件单独：

```text
screenshot.png
video.mp4
large html
```

甚至可以不压缩，因为它们本身已经压缩。

---

# Metadata 完全可以全部 memory-map

假设：

```text
10 million files
```

每个 inode metadata 如果压到大概：

```text
32–64 bytes
```

那就是：

```text
320–640 MB
```

这在现代机器上其实完全可以接受。

甚至不一定全部 load，只需要：

```text
mmap(index)
```

然后依靠 OS page cache。

结构可以设计成非常 cache-friendly：

```c
struct inode {
    uint32_t parent;
    uint32_t name_offset;
    uint32_t chunk;
    uint32_t data_offset;
    uint32_t size;
    uint16_t mode;
    uint16_t flags;
};
```

大概二三十 bytes。

再配一个 string table：

```text
prompt.json\0
response.json\0
stdout\0
...
```

大量重复文件名甚至还能 intern：

```text
"stdout"
"response.json"
"metadata.json"
```

只存一次。

对于 agent trajectory，这个收益可能很大，因为目录结构往往高度重复。

---

## 甚至目录 tree schema 都可以 dedup

如果你的每个 trajectory 结构高度相似：

```text
traj-X/
    messages.json
    stdout
    metadata.json
    artifacts/
```

那你甚至可以：

```text
directory schema #17
```

只存一次：

```text
schema #17:
    messages.json
    stdout
    metadata.json
    artifacts/
```

每个 trajectory 只存：

```text
schema = 17
+ actual inode/data mapping
```

这就开始比普通 filesystem 更适合 agent data 了。

---

# 所以 tar 最终可能只适合作为 ingestion/export format

我会这样看：

```text
                 portable format
                     TAR
                      │
              import / export
                      │
                      ▼
              trajectory pack
```

而真正 storage：

```text
┌──────────────────────────────────┐
│ superblock                       │
├──────────────────────────────────┤
│ inode table                      │
├──────────────────────────────────┤
│ directory index                 │
├──────────────────────────────────┤
│ string table                    │
├──────────────────────────────────┤
│ chunk index                     │
├──────────────────────────────────┤
│ compressed data chunk           │
│ compressed data chunk           │
│ compressed data chunk           │
│ ...                             │
└──────────────────────────────────┘
```

Git 只看到：

```text
run42.pack
```

或者：

```text
run42/
  metadata.pack
  data-0001.pack
  data-0002.pack
  ...
```

你 mount 后看到：

```text
10 million normal POSIX files
```

---

### 我尤其推荐 metadata 和 data 分开

例如：

```text
run42.meta
run42.data.000
run42.data.001
...
```

原因是 metadata 可能只有几百 MB，而 data 几百 GB。

那么：

```bash
ls
find
stat
```

基本只碰：

```text
run42.meta
```

完全不需要碰 data。

只有：

```bash
cat
vim
open/read
```

才碰 data shards。

这个分离会让性能非常漂亮：

```text
metadata operation:

VFS
 ↓
mmap'ed metadata
 ↓
done
```

而 data path：

```text
read()
 ↓
inode → chunk
 ↓
pread data shard
 ↓
zstd decompress
```

---

所以我现在反而觉得你最开始说的 **“tar + FUSE”只是一个很好的 prototype**。

如果真正要做一个适合 agent trajectory 的 storage，我会做：

> **cached directory tree + immutable compressed data chunks + POSIX projection**

而不是严格坚持 TAR。

你这个 workload 还有一个非常好的特性：**目录结构和文件名高度重复、数据 immutable、按 trajectory 访问有很强 locality**。这三个条件意味着你完全可以做得比通用 tar/squashfs 更高效，而且 implementation 仍然可以很小。


解决了，而且他们把这个问题叫得很直白：**small files problem**。你现在遇到的，本质上和 OLAP / data lake 里几百万个小 Parquet/object 的问题是同一个，只是你额外要求了 **POSIX file namespace**。

大数据这边的主流答案不是“让几百万个小文件变快”，而是：

> **把很多 logical records / objects compact 成少量大 immutable files，再用 metadata/index 恢复逻辑结构。**

例如 Delta Lake 的 `OPTIMIZE` 就明确做 bin-packing，把大量 small files 合并成大文件；Iceberg 也有 `rewriteDataFiles`，原因就是小文件会带来 metadata overhead 和 file-open cost。([Delta Lake][1])

Arrow 的文档甚至直接建议避免 `<20 MB` 的文件，也避免极细粒度 partition，因为 recursive listing 本身就会成为严重开销。([Apache Arrow][2])

所以他们已经把这个问题研究得很成熟了。

---

你可以把 OLAP 世界的做法抽象成三层：

```text
logical objects
     │
     ▼
metadata / manifest
     │
     ▼
large immutable files
```

比如 Iceberg：

```text
Table snapshot
      │
      ▼
manifest list
      │
      ▼
manifests
      │
      ▼
Parquet data files
```

所以假设逻辑上有：

```text
100,000,000 rows
```

物理上可能只有：

```text
1000 × 512MB Parquet
```

而不是 100M files。

这和你想做：

```text
10,000,000 logical files
         │
         ▼
directory metadata
         │
         ▼
500 × compressed blobs
```

其实几乎完全一样。

区别仅仅是：

```text
OLAP:
row / column / partition
```

vs

```text
你:
path / directory / file
```

---

## Parquet 本身其实已经给你一个非常值得借鉴的 design

Parquet 一个 file 内部不是一个巨型 compressed stream。

它是：

```text
Parquet file
│
├── Row Group 0
│    ├── Column Chunk A
│    │     ├── Page
│    │     ├── Page
│    │     └── Page
│    ├── Column Chunk B
│    └── ...
│
├── Row Group 1
│
└── Footer metadata
```

而且 metadata 里直接记录：

```text
column chunk → offset
```

reader 首先读 footer，然后直接定位需要的数据。([Parquet][3])

Compression 也不是整个文件一起 compress，而是到 **page** 这个粒度；Parquet 把 page 当作 encoding/compression 的基本单位。([Parquet][4])

这正好回答了你前面问：

> 每个 directory compress 是不是最好？

OLAP 世界给出的经验基本是：

> **不要 compression unit = 整个 logical partition/directory。**
>
> 要有一个中等粒度的 physical chunk/page。

也就是：

```text
directory
   │
   ├── metadata
   │
   └── files
         │
         ▼
      ~1-4MB chunks
```

非常像：

```text
Parquet:
Row group
   │
   └── pages

你的 FS:
Directory / trajectory
   │
   └── compressed chunks
```

---

# Iceberg 其实更值得你看

因为 Iceberg 解决的不只是 data packing，它还专门解决了：

> **我有海量 immutable data files，如何管理 namespace / versions / snapshots？**

它引入：

```text
metadata file
manifest list
manifest file
data file
```

这非常像你可以做：

```text
run.meta
directory manifests
data chunks
```

例如：

```text
snapshot
   │
   ▼
root manifest
   │
   ├── trajectory group A
   ├── trajectory group B
   └── trajectory group C
           │
           ▼
      data chunks
```

而不是做一个：

```text
10 GB global SQLite index
```

这种 hierarchical manifest 的优势就是：

```text
打开 /run42/traj123
```

不需要 load 整个 run 的 metadata。

可以：

```text
root
 ↓
run42 manifest
 ↓
traj123 manifest
 ↓
files
```

这和 filesystem directory hierarchy 可以高度一致。

---

## 这也刚好对应你刚刚说的“把 directory structure cache 下来”

我觉得你可以直接借 Iceberg/Parquet 的思想：

```text
                superblock
                    │
                    ▼
              directory index
                    │
          ┌─────────┼──────────┐
          ▼         ▼          ▼
        dir A      dir B      dir C
          │
          ▼
   chunk descriptor
          │
      ┌───┴────┐
      ▼        ▼
   chunk 17  chunk 18
```

其中一个 directory manifest：

```text
dir inode 3921

entries:
------------------------------------------------
name            type      inode/chunk
prompt.json     file      chunk=18 offset=0
response.json   file      chunk=18 offset=2817
stdout          file      chunk=18 offset=9182
artifacts       dir       inode=3922
```

压缩则类似 Parquet page：

```text
chunk18.zstd
≈ 1–4 MB uncompressed
```

而不是：

```text
directory.tar.zst
```

---

# OLAP 还有一个非常重要的 lesson：partition ≠ file

这个也是你现在容易自然想到、但值得避免的：

```text
one directory
    =
one physical compressed file
```

OLAP 很早就发现：

```text
partition = file
```

很危险。

因为 partition 很可能：

```text
有的 4KB
有的 100GB
```

所以现代 lakehouse 会把：

```text
logical partition
```

和：

```text
physical data files
```

分开。

例如逻辑：

```text
date=2026-09-04
```

下面可能有：

```text
part-000.parquet
part-001.parquet
...
```

然后 compaction 控制 physical file size。

Delta 的 optimized writes 和 auto compaction 本质上就是在控制这个物理粒度。([Delta Lake][1])

所以对你也应该是：

```text
logical:
trajectory / directory

physical:
fixed-ish sized chunks
```

而不是强绑定。

---

# 为什么 OLAP 通常选择 100MB–1GB，而你应该更小

Parquet 官方建议很大的 row group，例如 512MB–1GB，因为它主要优化的是：

```text
analytical scan
```

即：

```text
scan 10 billion rows
```

大 sequential IO 非常好。([Parquet][5])

但是你的是：

```bash
vim one/file
cat one/file
python open(one/file)
```

所以 random-access latency 更重要。

你的 chunk 应该小很多，比如：

```text
256 KB
1 MB
2 MB
4 MB
```

大概需要 benchmark。

这是 workload 差异：

```text
                    OLAP             trajectory FS

primary op          scan             point read
logical object      row              file
physical group      row group        chunk
compression unit    page             chunk
index key           column stats     pathname/inode
target chunk        MB–GB            KB–few MB
```

---

## 另外，OLAP 对 metadata 也做了“cache file structure”

例如 Parquet footer 存：

```text
row groups
column chunks
offsets
statistics
```

所以 reader 不需要扫描整个文件来了解结构。([Parquet][3])

甚至还有 page index：

```text
metadata
  ↓
直接决定要读哪个 page
```

避免碰无关 page。([Parquet][6])

你完全可以对应成：

```text
filesystem metadata
  ↓
directory → inode
  ↓
inode → chunk + offset
```

本质完全一样。

---

# 还有个领域甚至比 OLAP 更接近你：object/table format

如果从相似度来看，我会排：

```text
Iceberg / Delta
    ↓
LSM / SSTable
    ↓
Parquet
    ↓
SquashFS / EROFS
```

分别给你不同的启发：

**Iceberg / Delta**

解决：

```text
很多 immutable objects
snapshot
manifest
compaction
version
```

特别适合借鉴你怎么组织 run / trajectory metadata。

**LSM/SSTable**

解决：

```text
很多小 key/value
→ packed immutable sorted files
→ indexed lookup
```

非常像：

```text
path → file bytes
```

**Parquet**

解决：

```text
一个大文件里面怎么做：
metadata
offset index
compression blocks
random/selective IO
```

**EROFS/SquashFS**

解决：

```text
怎么把以上这些东西真正 expose 成 POSIX FS
```

---

# 所以我觉得你真正要找的不是“有没有 tar 的替代品”

而是把几个已经成熟的思想拼起来：

```text
                OLAP / Lakehouse
                      │
      ┌───────────────┼───────────────┐
      ▼               ▼               ▼
 manifest         compaction       immutable
 metadata          chunks           snapshots
      │               │
      └───────┬───────┘
              ▼
      trajectory storage
              │
              ▼
      POSIX projection layer
```

最后可能长成：

```text
run.pack/

superblock
   │
   ▼
directory manifests
   │
   ▼
inode/file descriptors
   │
   ▼
compressed chunks
```

而 Git commit 的只是：

```text
run.meta
chunk-000001
chunk-000002
...
```

几百到几千个 immutable files。

---

所以答案是：**OLAP 不仅解决过，而且你这个设计几乎应该直接借鉴他们 20 年解决 small-file problem 的经验。**

只是他们最终放弃了：

```text
logical object == POSIX file
```

而你现在要做的真正 novel/特殊部分，是：

> **保留 OLAP/LSM 那种 packed immutable storage，同时重新 projection 成一个完整 POSIX namespace。**

我觉得这个 framing 比“tar filesystem”准确很多。它其实是 **lakehouse-style storage + filesystem semantics**。

[1]: https://docs.delta.io/optimizations-oss/ "Optimizations | Delta Lake"
[2]: https://arrow.apache.org/docs/cpp/dataset.html "Tabular Datasets — Apache Arrow v25.0.1"
[3]: https://parquet.apache.org/docs/file-format/ "File Format | Parquet"
[4]: https://parquet.apache.org/docs/concepts/ "Concepts | Parquet"
[5]: https://parquet.apache.org/docs/file-format/configurations/ "Configurations | Parquet"
[6]: https://parquet.apache.org/docs/file-format/pageindex/ "Page Index | Parquet"


对，我觉得**很可能 OLAP-native 比 POSIX-native 更适合你的 trajectory 数据**，尤其是你现在给的前提：

> 分析主要由 AI agents 完成；人偶尔需要看某个 artifact 时，再把它 materialize/extract 成普通文件。

这会让我把推荐从 “FUSE + packed filesystem” 往 **DuckDB + Parquet/blob shards** 明显推。

最简单的版本甚至可以完全不跑 database server：

```text
trajectory store/
├── runs.parquet
├── events.parquet
├── files.parquet
└── blobs/
    ├── blob-0001.pack
    ├── blob-0002.pack
    └── ...
```

然后直接用 **DuckDB** query Parquet。DuckDB 是 embedded OLAP，不需要起服务，而且可以直接扫描 Parquet，支持 filter/projection pushdown。([DuckDB][1])

比如数据模型可以是：

```text
files:

run_id
trajectory_id
step_id
path
type
mime_type
size
content_text
blob_shard
blob_offset
blob_length
sha256
```

对于这些：

```text
prompt.json
response.json
stdout
stderr
tool call
tool result
agent messages
metrics
```

我甚至会直接存成 structured columns，而不是“文件”。

例如：

```sql
SELECT
    trajectory_id,
    step_id,
    tool_name,
    tool_result
FROM events
WHERE run_id = 'run-42'
  AND tool_name = 'bash'
  AND exit_code != 0;
```

这对 agent 来说比：

```bash
find ...
grep ...
cat ...
jq ...
```

好用太多。

Parquet 本身就是按照 row group / column chunk / page 来组织和压缩，而不是百万个 object 一对象一文件；compression 的基本单位也是 page。([Parquet][2])

---

### 我尤其不建议把所有东西都当 BLOB 塞进 OLAP

这里最好分两类。

**结构化/文本型 trajectory data：**

```text
messages
prompt
response
tool calls
tool results
stdout
metadata
reward
token usage
timing
```

直接：

```text
Parquet columns
```

这样才能利用 OLAP：

```text
predicate pushdown
projection
compression
vectorized execution
```

DuckDB 查询 Parquet 时可以自动只读取需要的 columns，并把 predicates push 到 Parquet scan。([DuckDB][3])

**真正 binary / arbitrary files：**

```text
PNG
PDF
checkpoint
tar
arbitrary generated source tree
large HTML
binary tool artifacts
```

不要全塞进 Parquet。

用：

```text
blob shards
```

然后 table 里只放：

```text
(shard, offset, length)
```

DuckDB 本身虽然有 `BLOB` 类型，但官方也明确说 very large objects 一般更适合存外部，只在数据库里保存引用。([DuckDB][4])

---

## 所以我觉得最好的架构是这样的

```text
                    Agent
                      │
                      ▼
              trajectory writer
                      │
          ┌───────────┴────────────┐
          │                        │
          ▼                        ▼
 structured events             artifacts
          │                        │
          ▼                        ▼
   Parquet dataset        append-only blob shards
          │                        │
          └───────────┬────────────┘
                      │
                      ▼
                    Git
```

Git commit：

```text
run-42/
├── events-000.parquet
├── events-001.parquet
├── files.parquet
├── blobs-000.pack
├── blobs-001.pack
└── manifest.json
```

可能一个 run 最终：

```text
5,000,000 logical artifacts
```

但 Git 只看到：

```text
几十 / 几百个 physical files
```

这就很好了。

---

## AI agent 的 interface 可以特别舒服

实际上你甚至不需要让 agent 知道 blob layout。

给它两个 primitive 就够了：

```python
query(sql)
materialize(path, dst)
```

比如 agent 想分析失败：

```sql
SELECT trajectory_id, step_id, stderr
FROM events
WHERE exit_code != 0
ORDER BY timestamp;
```

然后发现：

```text
trajectory=9182
path=/workspace/foo.py
```

需要看原文件，再：

```text
materialize(
    run="42",
    trajectory="9182",
    path="/workspace/foo.py",
    dst="/tmp/foo.py"
)
```

你再真正：

```text
blob shard
  ↓
offset/length
  ↓
decompress
  ↓
/tmp/foo.py
```

然后：

```bash
vim /tmp/foo.py
```

即可。

甚至可以做：

```bash
traj extract run42/traj9182 --to /tmp/t9182
```

一次恢复整个 trajectory 的 filesystem：

```text
/tmp/t9182/
├── workspace/
├── logs/
├── screenshots/
└── ...
```

这比一直维持一个 millions-of-files FUSE tree 简单太多。

---

# 对 AI agent 来说，OLAP representation 还有一个巨大优势

假设 agent 问：

> 找所有调用 `pytest` 后出现 timeout，并且 reward < 0.2 的 trajectories，然后比较它们之前三个 tool calls。

filesystem representation 下它得做：

```text
find
open JSON
parse
filter
join
sort
```

几百万次。

OLAP 下可能就是：

```sql
WITH failures AS (...)
SELECT ...
FROM events e
JOIN failures f
  ON ...
ORDER BY ...
```

而 columnar store 对这种 workload 恰恰极其擅长。

所以你实际上是在把：

```text
AI agent
  ↓
filesystem traversal
```

改成：

```text
AI agent
  ↓
declarative data access
```

我认为这是一个明显更好的 abstraction。

---

## 而且 AI agents 特别适合 SQL

以前一个反对 database interface 的理由是：

> 人不会 SQL / schema 太难记。

但现在消费者就是 LLM agent，这个问题反而基本消失。

你给它：

```text
DESCRIBE trajectories
DESCRIBE events
DESCRIBE files
```

agent 就可以自己 query。

而且 SQL 对这种 exploratory analysis 很自然：

```sql
SELECT ...
GROUP BY ...
QUALIFY ...
JOIN ...
WINDOW ...
```

所以你甚至可能**不需要再建立复杂的专用 trajectory API**。

这点我觉得非常重要。

---

# DuckDB 可能特别适合你

因为如果你说：

> OLAP 但不要 heavy

那 DuckDB 基本就是第一选择。

它不需要：

```text
database server
cluster
coordinator
ZooKeeper
service deployment
```

可以就是：

```bash
duckdb trajectories.db
```

或者甚至根本没有 `.db`：

```sql
SELECT *
FROM 'runs/**/*.parquet'
WHERE ...
```

DuckDB 可以直接 query Parquet，并行扫描并做 filter/projection pushdown。([DuckDB][3])

你的 Git repo 可以根本只有：

```text
.parquet
.pack
```

没有任何数据库 state。

也就是：

```text
Git repo
  ↓
Parquet files
  ↓
DuckDB = query engine
```

我反而非常喜欢这个 architecture，因为：

> **DuckDB 是 computation layer，不是 persistent storage owner。**

persistent format 是标准 Parquet。

哪天不想用 DuckDB：

```text
Polars
Spark
Arrow
DataFusion
ClickHouse
```

都还能读。

---

## Schema 可以天然支持 nested trajectories

Parquet 不是只能 flat table；它支持 nested/repeated structures。([Parquet][5])

所以可以有：

```text
trajectory:
    id
    reward
    messages: LIST<STRUCT<
        role,
        content,
        timestamp
    >>
    tools: LIST<STRUCT<
        name,
        args,
        result
    >>
```

但我个人可能还是会做 normalized-ish：

```text
runs
trajectories
events
files
```

因为 agent query/join 会更方便。

例如：

```text
runs
────
run_id
model
config
git_commit
created_at


trajectories
────────────
trajectory_id
run_id
reward
status


events
──────
trajectory_id
seq
type
role
content
tool_name
args
result
timestamp


files
─────
trajectory_id
path
size
mime
blob_ref
```

非常干净。

---

# Git 这时候也舒服很多

我还会避免：

```text
one giant trajectories.parquet
```

因为 Git incremental commit 不友好。

而用 immutable shards：

```text
runs/42/
  events-00000.parquet
  events-00001.parquet
  ...
  blobs-00000.pack
  blobs-00001.pack
```

比如每：

```text
128–512 MB Parquet
512 MB–2 GB blob shard
```

seal 一次。

然后：

```text
git commit
```

主要就是 adding immutable files。

下一次 commit 不会重写之前 100 GB 的 database。

这其实和 OLAP/lakehouse 的 immutable data-file 思路一致。

---

## 甚至可能不需要 filesystem projection 了

你之前花很多时间在考虑：

```text
FUSE
eBPF
EROFS
tarfs
```

如果实际 requirement 变成：

> 偶尔我需要 `vim` 一个东西。

那么完全没必要让 **5M files 永远存在于 POSIX namespace**。

做：

```bash
traj cat run42 traj123 /workspace/a.py
traj extract run42 traj123 /workspace/a.py
traj checkout run42 traj123 /tmp/t
```

就够了。

比如：

```bash
traj edit run42 traj123 /workspace/a.py
```

甚至内部：

```text
extract → tempfile → $EDITOR
```

用户体验一样：

```text
vim
```

但系统简单一个数量级。

---

### 我会这样做 V1

不先做 FUSE。

```text
Storage:

events/*.parquet
files/*.parquet
blobs/*.pack
```

然后一个很薄的 CLI：

```bash
traj sql "SELECT ..."

traj ls run42/traj123/workspace

traj cat run42/traj123/workspace/foo.py

traj extract run42/traj123/workspace/foo.py .

traj checkout run42/traj123 /tmp/traj123
```

其中：

```text
traj ls
```

其实：

```sql
SELECT path
FROM files
WHERE ...
```

`traj cat`：

```text
query blob_ref
→ pread shard
→ stdout
```

`traj checkout`：

```text
bulk extract selected subtree
```

如果以后真的发现：

> 我每天都需要 VS Code 直接打开整个虚拟 tree。

再把这个 same backend 接到 FUSE。

这样 FUSE 只是：

```text
optional projection layer
```

而不是 storage architecture。

---

**所以按照你现在描述的 usage，我会选 OLAP-first，而不是 filesystem-first。**

更具体地说：

> **Parquet + DuckDB 用于 trajectory/event/file metadata 和绝大多数文本内容；immutable blob shards 用于较大的 arbitrary artifacts；需要 POSIX 时按需 checkout/materialize。**

这样你同时解决：

```text
millions of tiny files
Git commit overhead
storage compression
agent analysis
cross-run query
schema evolution
selective extraction
```

而且系统其实比自己造一个 fast FUSE filesystem 还要轻很多。

[1]: https://duckdb.org/docs/current/guides/performance/file_formats "File Formats – DuckDB"
[2]: https://parquet.apache.org/docs/file-format/data-pages/compression/ "Compression | Parquet"
[3]: https://duckdb.org/docs/current/data/parquet/overview "Reading and Writing Parquet Files – DuckDB"
[4]: https://duckdb.org/docs/current/sql/data_types/blob "Blob Type – DuckDB"
[5]: https://parquet.apache.org/docs/file-format/nestedencoding/ "Nested Encoding | Parquet"

有关系，但**不会否定 OLAP-first**。真正的问题是：你的 output schema 会不会频繁演化到“每一版 agent 都长得不一样”。如果会，就不要把所有字段一开始都 rigidly 展开成固定 Parquet columns。

更稳的做法是把数据分成两层：

```text
stable envelope
+
flexible payload
```

比如稳定列只放这些：

```text
run_id
trajectory_id
step_id
timestamp
event_type
agent_version
schema_version
path
mime_type
payload_ref / payload_json
```

而具体 agent 输出：

```json
{
  "planner_state": ...,
  "tool_trace": ...,
  "whatever_new_field": ...
}
```

可以先作为：

* JSON/string
* nested `MAP`
* blob reference
* semi-structured payload

存下来。

这样 agent 今天输出：

```text
thought
tool_call
tool_result
```

明天变成：

```text
plan
subagents
critic
environment_delta
```

你都不需要立刻 migration 整个历史 dataset。

### 我会避免这种设计

```text
events.parquet:
  planner_score
  critic_score
  verifier_score
  browser_state
  shell_state
  ...
```

如果这些字段每周都变，很快 schema 会失控。

更推荐：

```text
events.parquet

run_id
trajectory_id
seq
event_type
schema_version
payload_json
blob_ref
```

然后对**真正长期稳定、经常分析的字段**逐渐 promote 成 column：

```text
reward
latency_ms
token_count
tool_name
exit_code
```

这其实是很多 data lake 常见的演化策略：raw 层尽量 lossless + permissive，上面再构建 normalized/derived tables。

你可以做成：

```text
raw/
  immutable events
       ↓
derived/
  normalized tables
  feature tables
  summaries
```

例如：

```text
raw_events.parquet
    payload_json

        ↓ agent / DuckDB transform

tool_calls.parquet
failures.parquet
trajectory_summary.parquet
```

这样 output format 改了，影响的是新的 parser/transform，而不是 storage substrate。

还有一个非常适合你场景的点：**保留原始 artifact 永远不要丢**。比如 agent 输出原本是：

```text
output/foo.json
```

那 raw layer 可以存：

```text
logical_path
content bytes / blob_ref
content_type
producer_version
```

之后你想重新理解历史数据，AI agent 可以重新 parse。

所以我会把原则定成：

> **storage schema 稳定，agent schema 可以乱变。**

比如底层永远只保证：

```text
object {
  run
  trajectory
  path/event_id
  timestamp
  type
  producer_version
  bytes/json
}
```

上层分析 schema 随时重建。

这甚至比传统 OLAP 更适合 AI-agent workload，因为现在 parsing/normalization 本身也可以交给 agent 做。

如果 output format 经常变化，我反而会更推荐这个三层模型：

```text
Raw immutable layer
    JSON / text / arbitrary blobs

        ↓

Catalog layer
    stable metadata in Parquet

        ↓

Derived analytical layer
    Parquet tables generated on demand
```

这样你不会因为 schema evolution 把系统搞得很重。

一句话：**format 经常改没关系；不要让 agent 的 output schema 等于你的 storage schema。** 底层存一个稳定 envelope + 原始 payload，上层需要分析时再 schema-on-read / derive。
