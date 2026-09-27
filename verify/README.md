# verify/: a verified TrajFS core in Verus

This directory holds the formal specification of a TrajFS store, its refinement layers, and the
verified executable modules, all checked by [Verus](https://github.com/verus-lang/verus). It is
independent of the Cargo workspace: nothing here is compiled into `traj` yet.

- `SPEC.md`: the high-level specification in prose, its promises, and the trusted assumptions.
  Review this first.
- `REFINEMENT.md`: the layers, the abstraction functions, and the status of every obligation.
- `src/spec/`: L0 (`abstract_store.rs`), L1 (`catalog.rs`), L2 (`durable.rs`), paths.
- `src/exec/`: verified executable code (`namespace.rs`).

## Run

```bash
# once: a Verus release and the toolchain it names
mkdir -p ~/tools && cd ~/tools
gh release download release/0.2026.09.20.aef82ed --repo verus-lang/verus --pattern '*x86-linux*'
unzip verus-*-x86-linux.zip && ./verus-x86-linux/verus --version   # prints the rustup command if needed

# every time
VERUS=~/tools/verus-x86-linux/verus verify/run.sh --triggers-mode silent
```

The last line is `verification results:: N verified, 0 errors`. `grep -n admit verify/src` lists
the obligations that are stated but not yet proved (one at the time of writing).
