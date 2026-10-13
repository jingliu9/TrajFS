# Lossless compaction and storage verification

TrajFS keeps every logical path while storing identical file content once per
store. Names, inodes, executable bits and timestamps do not create a second copy
of the same payload. A thousand paths referencing a 47 MB FASTA are a thousand
catalog rows and one blob, before compression.

Different files can also share identical blocks. New packs reuse identical
uncompressed frames within the batch. `traj compact` extends that sharing across
all retained batches and recompresses frames into a new store. It preserves
catalog history, blob hashes, paths, modes, timestamps, attributes, symlinks,
exclusions, deletion markers and derived tables. The original store is retained.
The output uses the existing pack/index format and remains readable by older
TrajFS readers.

```bash
traj verify --deep --storage --json -S original.trajstore
traj compact -S original.trajstore --out compacted.trajstore --json
traj verify --deep --require-compact --json -S compacted.trajstore
```

Compaction uses Zstandard level 19 by default; `--level 1` through `--level 22`
select another level. Each frame keeps the smaller of its existing encoding and
the new encoding. `--no-recompress` only shares repeated frames and retains their
smallest existing encoding. Payload memory is bounded by frame size; identity
and catalog metadata scale with the store. Exact duplicate frames are shared
even when their members belong to different files or historical batches.

Both the source and destination must pass native deep verification. Existing or
nested destinations are refused. Unpublished artifacts are refused rather than
silently omitted. A failed operation retains the original store and any partial
destination for inspection. There is no overwrite, delete, or no-verify option.

The JSON report separates:

- `logical_bytes`: bytes referenced by all retained catalog rows, including history.
- `unique_blob_bytes`: bytes after exact whole-file/content deduplication.
- `unique_frame_bytes`: distinct uncompressed frame content.
- `pack_bytes`: actual pack files, including their headers.
- `metadata_bytes`: manifests, catalogs, indexes and derived tables.
- `stored_bytes`: all declared artifacts, packs plus metadata.
- `duplicate_frames` and `unreferenced_pack_bytes`: physical redundancy that
  `verify --deep --require-compact` rejects.

Compare total `stored_bytes` when choosing a published representation. Catalog
serialization and manifest changes can affect metadata size even when payload
storage decreases. If the original is already compact and smaller, retain that
representation. Verification receipts used by a publisher should bind the
manifest and every artifact hash, so an old receipt cannot approve new bytes.

This is lossless exact-file and exact-frame deduplication, not a claim of a
mathematically minimal encoding. Changed or shifted blocks, embedded archives,
and separate stores can still contain related data. Separate stores have separate
blob pools. Choosing retention rules or replacing generated data with recipes is
a separate decision and must not discard irreplaceable observations.
