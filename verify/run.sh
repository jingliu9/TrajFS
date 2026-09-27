#!/usr/bin/env bash
# Verify the TrajFS specification and the verified executable modules with Verus.
#
#   verify/run.sh            # everything
#   verify/run.sh --expand-errors ...   # extra verus flags are passed through
#
# Needs a Verus release on PATH or at $VERUS (https://github.com/verus-lang/verus/releases) and the
# Rust toolchain that release names (verus prints the rustup command if it is missing).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
verus="${VERUS:-$(command -v verus || echo /workspace/tools/verus-x86-linux/verus)}"
exec "$verus" --crate-type=lib "$here/src/lib.rs" "$@"
