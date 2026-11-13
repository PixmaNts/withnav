#!/usr/bin/env bash
# Measure the build cost of the integration-test layout in tests/.
#
# Domain: every top-level tests/*.rs file is its own test binary, so a one-line
# change to a core struct re-links one executable per file (80 files on
# develop as of 2026-11-13; tests/common/mod.rs carries a crate-level allow
# precisely because each file is a separate crate). Before anyone restructures
# or merges these files, the actual cost must be measured, not assumed.
#
# What it does, per mode:
#   baseline     — current per-file layout as-is.
#   consolidated — generates a throwaway single-binary harness under
#                  tests/_cost_probe/ that compiles every test file as a
#                  module, sets `autotests = false` plus one `[[test]]` target
#                  in Cargo.toml, measures, then restores both. The harness and
#                  the Cargo.toml edit are never meant to be committed: this
#                  script only exists to produce the numbers.
#
# Each mode runs: cargo clean → timed `cargo test --no-run` (wall clock + peak
# RSS) → du of target/ → count of executable artifacts in target/debug/deps →
# touch src/session/service.rs → timed incremental `cargo test --no-run`.
#
# Peak RSS comes from wait4/getrusage (ru_maxrss) via python3 — the same
# facility GNU /usr/bin/time -v reads. GNU time was not installed on the host
# these numbers were first taken on (2026-11-13); if /usr/bin/time exists it is
# used instead and cross-checks the python figure.
#
# A compile cache wrapping cargo (e.g. a mise/mr-boxington wrapper) survives
# cargo clean, so "clean" builds through it are partly cache-served. Set
# CARGO_BIN=/usr/bin/cargo (or another unwrapped cargo) for cold numbers; the
# default PATH cargo measures what a developer on that host actually waits for.
#
# Run ONLY in a throwaway checkout or worktree: it destroys target/ and, in
# consolidated mode, temporarily rewrites Cargo.toml (refuses to run when
# tracked files are dirty). Printout lines start with "RESULT" so runs can be
# compared mechanically.
#
# Usage: [CARGO_BIN=…] measure-test-build-cost.sh <baseline|consolidated> [--keep-harness] [--results-dir DIR]
set -euo pipefail

MODE="${1:-}"
case "$MODE" in
  baseline|consolidated) ;;
  *) echo "usage: $0 <baseline|consolidated> [--keep-harness] [--results-dir DIR]" >&2; exit 2 ;;
esac
shift

KEEP_HARNESS=0
RESULTS_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --keep-harness) KEEP_HARNESS=1 ;;
    --results-dir) shift; RESULTS_DIR="${1:?--results-dir needs a value}" ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

ROOT="$(cd "$(dirname "$0")/.." && git rev-parse --show-toplevel)"

# The consolidated mode rewrites Cargo.toml and adds files under tests/: refuse
# to run on a checkout with dirty tracked files so the restore cannot clobber
# unrelated work. Untracked leftovers of a previous harness run would skew the
# artifact counts, and consolidated mode rm -rf's the harness paths, so a
# pre-existing file there must abort instead of being destroyed.
if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
  echo "refusing to run: tracked files in $ROOT are modified" >&2
  exit 1
fi
for remnant in "$ROOT/tests/_cost_probe" "$ROOT/tests/_cost_probe.rs"; do
  if [ -e "$remnant" ]; then
    echo "refusing to run: $remnant already exists (remove it or keep it elsewhere)" >&2
    exit 1
  fi
done
if [ ! -f "$ROOT/src/session/service.rs" ]; then
  echo "refusing to run: $ROOT/src/session/service.rs not found (incremental phase touches it)" >&2
  exit 1
fi

if [ -z "$RESULTS_DIR" ]; then
  RESULTS_DIR="/tmp/opencode/withnav-build-cost/$(date +%Y%m%dT%H%M%S)-$MODE"
fi
mkdir -p "$RESULTS_DIR"

# Keep the measurement self-contained: pin target/ inside this checkout and
# drop outer build-customization so both modes see identical flags.
export CARGO_TARGET_DIR="$ROOT/target"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET CARGO_BUILD_JOBS
# This host wraps rustc with the mr-boxington compile cache (mbx), reached
# through the PATH cargo (a mise command wrapper). cargo clean does not empty
# that cache, so a "clean" build would be partially cache-served — and the
# second trial would inherit a cache warmed by the first, biasing the
# comparison. Pass CARGO_BIN=/usr/bin/cargo to measure without the cache
# (verified: the real cargo produces no mbx lines); the default keeps the
# wrapper, i.e. the build environment a developer on this host actually uses.
CARGO_BIN="${CARGO_BIN:-cargo}"
# Pin the real rustc regardless of wrapper: keeps RUSTC stable across regimes.
export RUSTC=/usr/bin/rustc
unset RUSTC_WORKSPACE_WRAPPER

RESULT_LOG="$RESULTS_DIR/result-lines.txt"
: > "$RESULT_LOG"
result() { # result <phase> <key=value>...
  local phase="$1"; shift
  local line="RESULT mode=$MODE phase=$phase $*"
  echo "$line" | tee -a "$RESULT_LOG"
}

# --- timing / peak-RSS helper ------------------------------------------------
# subprocess.run waits for the cargo process and getrusage(RUSAGE_CHILDREN)
# then reports the largest peak RSS among reaped children. The kernel rolls the
# peaks of reaped descendants (cargo -> rustc -> linker) up transitively into
# that value, which is the same quantity /usr/bin/time -v prints as "Maximum
# resident set size" (GNU time reads wait4's rusage of its direct child).
HELPER="$RESULTS_DIR/measure_rusage.py"
cat > "$HELPER" <<'PYEOF'
import resource, subprocess, sys, time
start = time.monotonic()
proc = subprocess.run(sys.argv[1:])
elapsed = time.monotonic() - start
peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss  # KiB on Linux
print(f"WALL_SECONDS={elapsed:.2f}")
print(f"PEAK_RSS_KIB={peak}")
sys.exit(proc.returncode)
PYEOF

TIME_BIN=""
if [ -x /usr/bin/time ]; then TIME_BIN=/usr/bin/time; fi

run_measured() { # run_measured <phase> <cmd...>
  local phase="$1"; shift
  local log="$RESULTS_DIR/$MODE-$phase.log"
  echo ">> [$MODE/$phase] $*" | tee "$log"
  local wall rss extra=""
  if [ -n "$TIME_BIN" ]; then
    set +e
    "$TIME_BIN" -v -o "$RESULTS_DIR/$MODE-$phase.time" python3 "$HELPER" "$@" > >(tee -a "$log") 2>&1
    local rc=$?
    set -e
    wall=$(sed -n 's/^WALL_SECONDS=//p' "$log" | tail -1)
    rss=$(sed -n 's/^PEAK_RSS_KIB=//p' "$log" | tail -1)
    extra="gnu_time_max_rss_kib=$(sed -n 's/.*Maximum resident set size (kbytes): //p' "$RESULTS_DIR/$MODE-$phase.time" | tail -1)"
  else
    set +e
    python3 "$HELPER" "$@" > >(tee -a "$log") 2>&1
    local rc=$?
    set -e
    wall=$(sed -n 's/^WALL_SECONDS=//p' "$log" | tail -1)
    rss=$(sed -n 's/^PEAK_RSS_KIB=//p' "$log" | tail -1)
    extra=""
  fi
  if [ $rc -ne 0 ]; then
    echo "cargo failed (rc=$rc); log: $log" >&2
    tail -40 "$log" >&2
    exit $rc
  fi
  result "$phase" wall_seconds="$wall" peak_rss_kib="$rss" $extra
}

# --- artifact counting -------------------------------------------------------
count_artifacts() { # count_artifacts <phase> <stems-file>
  local phase="$1" stems_file="$2"
  local deps="$CARGO_TARGET_DIR/debug/deps"
  local total stem_hits=0 s n
  total=$(find "$deps" -maxdepth 1 -type f -perm /111 2>/dev/null | wc -l)
  # executable-permission regular files: the issue's requested metric. It can
  # include non-test executables (e.g. shared libs); test_stem_binaries below
  # is the narrow count of binaries named after the top-level test files.
  while IFS= read -r s; do
    [ -z "$s" ] && continue
    n=$(find "$deps" -maxdepth 1 -type f -name "$s-*" -perm /111 2>/dev/null | wc -l)
    stem_hits=$((stem_hits + n))
  done < "$stems_file"
  local disk
  disk=$(du -sm "$CARGO_TARGET_DIR" | cut -f1)
  result "$phase" target_mib="$disk" executable_files_in_deps="$total" test_stem_binaries="$stem_hits"
}

# --- stems list (top-level test files) ---------------------------------------
STEMS_FILE="$RESULTS_DIR/stems.txt"
: > "$STEMS_FILE"
for f in "$ROOT"/tests/*.rs; do
  b="$(basename "$f" .rs)"
  [ "$b" = "_cost_probe" ] && continue
  echo "$b" >> "$STEMS_FILE"
done

MODIFIED_CARGO=0
restore() {
  if [ "$MODIFIED_CARGO" = 1 ]; then
    git -C "$ROOT" checkout -- Cargo.toml
  fi
  if [ "$KEEP_HARNESS" != 1 ]; then
    rm -rf "$ROOT/tests/_cost_probe" "$ROOT/tests/_cost_probe.rs"
  fi
}
trap restore EXIT

# --- consolidated harness generation -----------------------------------------
if [ "$MODE" = consolidated ]; then
  PROBE_DIR="$ROOT/tests/_cost_probe"
  PROBE_RS="$ROOT/tests/_cost_probe.rs"
  rm -rf "$PROBE_DIR" "$PROBE_RS"
  mkdir -p "$PROBE_DIR"

  # Copies of each test file live at tests/_cost_probe/<name>.rs. The probe root
  # (tests/_cost_probe.rs, dir = tests/) must reference each copy with an EXPLICIT
  # #[path]: a bare `mod doctor;` from tests/ resolves to the ORIGINAL tests/doctor.rs,
  # not the copy. `common` is loaded ONCE by the root; each copy drops its own
  # `mod common;` and qualifies `use common::` to `use crate::common::`.
  # Every path inside a copy that was relative to tests/ must gain one "../" level:
  #   #[path = "../P"]               -> #[path = "../../P"]   (repo-root targets)
  #   #[path = "P"]   (P not ../..)  -> #[path = "../P"]      (tests/ targets)
  #   include_{str,bytes}!("../P")   -> "../../P"; ("P") -> "../P"
  # Inner attributes stay legal because each copy is a real file module.
  for f in "$ROOT"/tests/*.rs; do
    b="$(basename "$f" .rs)"
    [ "$b" = "_cost_probe" ] && continue
    sed -E \
      -e '/^[[:space:]]*mod common;[[:space:]]*$/d' \
      -e 's|^([[:space:]]*)(pub(\([^)]*\))?[[:space:]]+)?use common::|\1\2use crate::common::|' \
      -e 's@(^|[^:_[:alnum:]])common::@\1crate::common::@g' \
      -e 's|#\[path = "\.\./|#\[path = "../../|g' \
      -e 's|#\[path = "([^./"])|#[path = "../\1|g' \
      -e 's|include_str!\("\.\./|include_str!("../../|g' \
      -e 's|include_str!\("([^./"])|include_str!("../\1|g' \
      -e 's|include_bytes!\("\.\./|include_bytes!("../../|g' \
      -e 's|include_bytes!\("([^./"])|include_bytes!("../\1|g' \
      "$f" > "$PROBE_DIR/$b.rs"
  done

  {
    echo '// GENERATED by scripts/measure-test-build-cost.sh for the consolidated-build-cost trial.'
    echo '// Throwaway file: never commit it. One binary that compiles every test file as a module.'
    echo '#[path = "common/mod.rs"] mod common;'
    while IFS= read -r b; do
      echo "#[path = \"_cost_probe/$b.rs\"] mod $(echo "$b" | tr '-' '_');"
    done < "$STEMS_FILE"
  } > "$PROBE_RS"

  # Only the declared [[test]] target is built, so the root package contributes
  # exactly one test binary instead of one per tests/*.rs file.
  if grep -q '^autotests' "$ROOT/Cargo.toml"; then
    echo "refusing to run: Cargo.toml already sets autotests" >&2
    exit 1
  fi
  # 0,/re/ = first match only; matching every `name = "withnav"` line would also
  # insert autotests=false into [[lib]]/[[bin]]/[[bench]] blocks, which cargo then
  # rejects ("autotests cannot be a member of target section") and aborts the run.
  sed -i '0,/^name = "withnav"$/s//autotests = false\nname = "withnav"/' "$ROOT/Cargo.toml"
  printf '\n[[test]]\nname = "_cost_probe"\npath = "tests/_cost_probe.rs"\n' >> "$ROOT/Cargo.toml"
  grep -q '^autotests = false$' "$ROOT/Cargo.toml" && grep -q '^\[\[test\]\]$' "$ROOT/Cargo.toml" \
    || { echo "Cargo.toml edit failed" >&2; exit 1; }
  MODIFIED_CARGO=1
fi

# --- provenance context -------------------------------------------------------
result context \
  "date=$(date -Is)" \
  "git_head=$(git -C "$ROOT" rev-parse HEAD)" \
  "rustc=$(rustc -vV | sed -n 's/^release: //p')" \
  "cargo=$("$CARGO_BIN" -V | cut -d' ' -f2)" \
  "nproc=$(nproc)" \
  "test_files=$(wc -l < "$STEMS_FILE")" \
  "rss_source=$( [ -n "$TIME_BIN" ] && echo "/usr/bin/time -v + wait4" || echo "wait4/getrusage" )" \
  "rustc_pin=$RUSTC" \
  "cargo_bin=$CARGO_BIN"

# --- measurement --------------------------------------------------------------
cargo_clean() { "$CARGO_BIN" clean; }
cargo_clean
run_measured clean-build "$CARGO_BIN" test --no-run
count_artifacts clean-build "$STEMS_FILE"

# Forced rebuild after touching this central core module (148 KB, under the
# server): shows the layout's incremental cost for a source-tree change to a
# file this size. One data point, not a general claim about all edits.
touch "$ROOT/src/session/service.rs"
run_measured incremental-after-touch "$CARGO_BIN" test --no-run
count_artifacts incremental-after-touch "$STEMS_FILE"

echo
echo "===== summary ($MODE) ====="
cat "$RESULT_LOG"
echo "logs: $RESULTS_DIR"
