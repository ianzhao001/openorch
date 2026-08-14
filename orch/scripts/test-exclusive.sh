#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 /absolute/path/to/cargo" >&2
  exit 64
fi

cargo_bin=$1
case "$cargo_bin" in
  /*) ;;
  *)
    echo "testExclusive requires an absolute cargo path: $cargo_bin" >&2
    exit 64
    ;;
esac

run_exact() {
  package=$1
  target_kind=$2
  target_name=$3
  test_name=$4

  case "$target_kind" in
    --lib)
      list_output=$("$cargo_bin" test --locked --manifest-path orch/Cargo.toml -p "$package" --lib "$test_name" -- --list --ignored --exact)
      ;;
    --test)
      list_output=$("$cargo_bin" test --locked --manifest-path orch/Cargo.toml -p "$package" --test "$target_name" "$test_name" -- --list --ignored --exact)
      ;;
    *)
      echo "unsupported testExclusive target kind: $target_kind" >&2
      exit 64
      ;;
  esac

  printf '%s\n' "$list_output"
  match_count=$(printf '%s\n' "$list_output" | awk -F ': ' -v exact="$test_name" '$1 == exact && $2 == "test" { count++ } END { print count + 0 }')
  if [ "$match_count" -ne 1 ]; then
    echo "testExclusive expected exactly one ignored test named $test_name; found $match_count" >&2
    exit 65
  fi

  case "$target_kind" in
    --lib)
      "$cargo_bin" test --locked --manifest-path orch/Cargo.toml -p "$package" --lib "$test_name" -- --ignored --exact --test-threads=1
      ;;
    --test)
      "$cargo_bin" test --locked --manifest-path orch/Cargo.toml -p "$package" --test "$target_name" "$test_name" -- --ignored --exact --test-threads=1
      ;;
  esac
}

run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_normal_gate_exit_at_runtime
run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_timeout_gate_exit_at_runtime
run_exact orch-cli --test wake_signal_isolation_cli four_signal_topology_cells_pass_ten_consecutive_runs
run_exact orch-host --test gate_reap_convergence successful_kill_converges_under_pressure
run_exact orch-host --lib _ serve::tests::production_entries_visit_all_six_unattended_stations
