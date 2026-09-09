#!/bin/sh
set -eu

usage() {
  echo "usage: $0 [--repeat N] [--parallel-threads N] [--cargo /absolute/path/to/cargo] [--output DIR]" >&2
  exit 64
}

repeat=10
parallel_threads=8
cargo_bin=/Users/admin/.cargo/bin/cargo
output_dir=

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repeat)
      [ "$#" -ge 2 ] || usage
      repeat=$2
      shift 2
      ;;
    --parallel-threads)
      [ "$#" -ge 2 ] || usage
      parallel_threads=$2
      shift 2
      ;;
    --cargo)
      [ "$#" -ge 2 ] || usage
      cargo_bin=$2
      shift 2
      ;;
    --output)
      [ "$#" -ge 2 ] || usage
      output_dir=$2
      shift 2
      ;;
    -h|--help)
      usage
      ;;
    *)
      usage
      ;;
  esac
done

case "$repeat" in
  ''|*[!0-9]*) usage ;;
esac
if [ "$repeat" -lt 10 ]; then
  echo "H186 attribution requires --repeat N with N >= 10" >&2
  exit 64
fi
case "$parallel_threads" in
  ''|*[!0-9]*) usage ;;
esac
if [ "$parallel_threads" -lt 2 ]; then
  echo "H186 parallel profile requires at least 2 test threads" >&2
  exit 64
fi
case "$cargo_bin" in
  /*) ;;
  *)
    echo "--cargo must be an absolute path" >&2
    exit 64
    ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
cd "$repo_root"

fixed_head=$(git rev-parse HEAD)
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "H186 attribution requires a clean tracked tree at fixed HEAD $fixed_head" >&2
  exit 65
fi

if [ -z "$output_dir" ]; then
  run_stamp=$(date -u +%Y%m%dT%H%M%SZ)
  output_dir="$repo_root/orch/target/h186-attribution/$run_stamp-$$"
fi
if [ -e "$output_dir" ]; then
  echo "H186 output path already exists; refusing to overwrite raw evidence: $output_dir" >&2
  exit 65
fi
mkdir -p "$output_dir"
summary="$output_dir/summary.tsv"
printf 'profile\titeration\texit\telapsed_seconds\tfailures\tlog\n' >"$summary"

target=verdict_seal_cli
case_name=doctor_fails_with_bound_and_current_hashes_after_recorded_review_is_tampered
serial_failures=0
parallel_failures=0

run_profile() {
  profile=$1
  iteration=$2
  threads=$3

  observed_head=$(git rev-parse HEAD)
  if [ "$observed_head" != "$fixed_head" ]; then
    echo "HEAD moved during H186 attribution: expected=$fixed_head observed=$observed_head" >&2
    exit 66
  fi
  if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "tracked tree changed during H186 attribution at $profile iteration $iteration" >&2
    exit 66
  fi

  log="$output_dir/$profile-$iteration.log"
  started=$(date +%s)
  set +e
  "$cargo_bin" test --locked --manifest-path orch/Cargo.toml -p orch-cli \
    --test "$target" -- --test-threads="$threads" >"$log" 2>&1
  status=$?
  set -e
  finished=$(date +%s)
  elapsed=$((finished - started))

  if ! grep -Fq "test $case_name ... ok" "$log" \
    && ! grep -Fq "test $case_name ... FAILED" "$log"; then
    echo "watched H186 case was not observed at $profile iteration $iteration; see $log" >&2
    exit 67
  fi

  failures=$(awk '
    /^test .* \.\.\. FAILED$/ {
      name=$2
      if (names != "") names=names ","
      names=names name
    }
    END { print names }
  ' "$log")
  if [ "$status" -ne 0 ] && [ -z "$failures" ]; then
    failures="unparsed-see-log"
  fi
  if [ -z "$failures" ]; then
    failures="-"
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$profile" "$iteration" "$status" "$elapsed" "$failures" "$log" >>"$summary"
  printf 'H186_RUN profile=%s iteration=%s exit=%s elapsed_seconds=%s failures=%s log=%s\n' \
    "$profile" "$iteration" "$status" "$elapsed" "$failures" "$log"

  if [ "$status" -ne 0 ]; then
    case "$profile" in
      serial) serial_failures=$((serial_failures + 1)) ;;
      parallel) parallel_failures=$((parallel_failures + 1)) ;;
    esac
  fi
}

printf 'H186_META head=%s repeat=%s serial_threads=1 parallel_threads=%s target=%s watched_case=%s output=%s\n' \
  "$fixed_head" "$repeat" "$parallel_threads" "$target" "$case_name" "$output_dir"

iteration=1
while [ "$iteration" -le "$repeat" ]; do
  run_profile serial "$iteration" 1
  run_profile parallel "$iteration" "$parallel_threads"
  iteration=$((iteration + 1))
done

classification=R3-not-reproduced
if [ "$parallel_failures" -gt 0 ] && [ "$serial_failures" -eq 0 ]; then
  classification=R1-parallel-related
elif [ "$parallel_failures" -gt 0 ] || [ "$serial_failures" -gt 0 ]; then
  classification=R2-not-isolated-to-parallelism
fi

printf 'H186_RESULT classification=%s serial_failures=%s parallel_failures=%s total_runs=%s summary=%s\n' \
  "$classification" "$serial_failures" "$parallel_failures" "$((repeat * 2))" "$summary"
