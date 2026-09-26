# Prototype benchmark records

Raw measurements behind Review 1 (`../idea-review.md`), taken in September 2026 on the measurement host with a
warm page cache, before any Rust code existed. `prototype-one-round.json` covers one round of the reference run
(about 107 K retained files, 474 MB): building a tar, tar+zstd at levels 3 and 19, point reads from the compressed
tar with and without an offset index over the uncompressed tar, a Parquet file holding paths and contents, a
Parquet pair with SHA-256 deduplication (`files` path-to-sha and `blobs` sha-to-content) queried through DuckDB
(`ls`, `cat`, `find`, `grep`, per-directory sizes), the same operations on the native filesystem, and a zstd
squashfs image. `prototype-whole-run.json` covers the whole reference run (2.17 M files, 12.9 GB retained):
hashing every file with 16 processes, writing the `files` catalog, and writing the distinct blobs into SHA-addressed
Parquet shards of about 256 MB raw each.

The Python scripts that produced these files were exploratory one-offs tied to private paths and are not kept; the
JSON keys are the raw measurements exactly as the scripts wrote them (`*_s` are seconds, `*_bytes` are bytes,
`*_s_each` are per-probe averages). Their retention rules differ from the current `traj` profiles, so these numbers
are not comparable with the README benchmarks; `traj bench` is the supported way to time a store.
