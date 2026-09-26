# Design history

Superseded design drafts and exploration notes, kept for context. Nothing here describes the current behaviour of
`traj`; the live design documents are [`../PLAN.md`](../PLAN.md) (store format, CLI, Git integration, test plan),
[`../PLAN-fuse.md`](../PLAN-fuse.md) (the read-only mount), [`../PLAN-deletion.md`](../PLAN-deletion.md) and
[`../granularity.md`](../granularity.md).

| file | what it is |
|---|---|
| `idea.md` | the raw brainstorming transcript that started the project (opens mid-conversation; unedited) |
| `idea-review.md` | Review 1: the tar+FUSE versus Parquet+DuckDB comparison, measured on the reference run, that fixed the content-addressed design |
| `PLAN.v1.md` | draft 1 of the plan, before adapters made the core format-agnostic; replaced by `../PLAN.md` |
| `vscode-viewer.md` | why a FUSE mount was chosen over a VS Code extension; the mount design moved to `../PLAN-fuse.md` |
| `bench/` | raw measurements of the Python prototype behind Review 1 (see `bench/README.md`) |

Numbers in these files were taken on the reference run (about 2.17 M paths and 12.9 GB retained under the rules of
the time; 2.14 M paths and 12.0 GB under the current rules) on the measurement host. They predate the Rust
implementation and are not comparable with the figures in the README.
