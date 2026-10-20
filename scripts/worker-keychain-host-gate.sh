#!/usr/bin/env bash
# Run on the macOS host, not inside a Seatbelt-confined worker.
set -euo pipefail
worktree="${1:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)}"
cd "$worktree"
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS host required' >&2; exit 1; }
mkdir -p target .cortexkit/internal-docs/investigations
logs="$PWD/.cortexkit/internal-docs/investigations/keychain-host-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$logs"
git log -1 --oneline
git diff --stat
git diff --exit-code
git diff --cached --exit-code
cargo --version
rustc --version
/usr/bin/sw_vers
storage="$(mktemp -d "$PWD/target/keychain-host-storage.XXXXXX")"
export AFT_STORAGE_DIR="$storage"
# Remove inherited wrapper overrides so tests can exercise their own Cargo config.
# The compatibility probe's nested Cargo commands explicitly disable wrappers.
unset RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER
file=crates/aft/src/cli/sandbox_launch/seatbelt.rs
mutated=0
restore() {
    if [[ "$mutated" == 1 ]]; then
        git checkout -- "$file"
        touch "$file"
        mutated=0
    fi
}
trap restore EXIT
trap 'restore; exit 130' INT
trap 'restore; exit 143' TERM
run_gate() {
    local label="$1"; shift
    local status=0
    env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 "$@" >"$logs/$label.log" 2>&1 || status=$?
    cat "$logs/$label.log"
    return "$status"
}
probe() {
    local label="$1" test="$2"
    run_gate "$label" cargo test -p agent-file-tools --test worker_jail -- "$test" --exact --nocapture --test-threads=1 || return $?
    python3 - "$logs/$label.log" "$test" <<'PY'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
assert "test result: ok. 1 passed; 0 failed;" in text, text
assert f"test {sys.argv[2]} ... ok" in text, text
PY
}
probe throwaway-green keychain::worker_jail_throwaway_keychain_is_unreadable
probe file-green keychain::worker_jail_default_keychain_file_is_unreadable
probe head-green keychain::head_shell_throwaway_keychain_remains_readable
probe compatibility-green keychain::worker_jail_keychain_build_and_tls_compatibility
run_gate suite-green cargo test -p agent-file-tools --test worker_jail -- --nocapture --test-threads=1
python3 - "$logs/suite-green.log" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
counts = re.findall(r"test result: ok\. (\d+) passed; 0 failed;", text)
assert counts and int(counts[-1]) > 0, text
PY

for layer in mach file; do
    # Stage the live version before mutation, then require a clean tracked diff.
    git add "$file"
    git diff --stat
    git diff --exit-code
    mutated=1
    if [[ "$layer" == mach ]]; then
        perl -0777 -i -pe 'BEGIN { $n=0 } $n += s/^    for service in \[\n(?:(?!^    \] \{).)*?"com\.apple\.SecurityServer"(?:(?!^    \] \{).)*?^    \] \{\n.*?^    \}\n/    \/\/ NON-VACUITY BREAK: credential Mach lookup rules removed.\n/msg; END { die "Mach mutation matched $n blocks, expected exactly 1\n" unless $n == 1 }' "$file"
        expected=keychain::worker_jail_throwaway_keychain_is_unreadable
        status_marker=QUERY_STATUS=0
    else
        perl -0777 -i -pe 'BEGIN { $n=0 } $n += s/^    for path in &profile\.keychain_deny \{\n.*?^    \}\n/    \/\/ NON-VACUITY BREAK: keychain file rules removed.\n/msg; END { die "File mutation matched $n blocks, expected exactly 1\n" unless $n == 1 }' "$file"
        expected=keychain::worker_jail_default_keychain_file_is_unreadable
        status_marker=FILE_STATUS=0
    fi
    git diff --stat >"$logs/$layer-applied.stat"
    cat "$logs/$layer-applied.stat"
    [[ -s "$logs/$layer-applied.stat" ]] || { echo 'Mutation produced no tracked diff' >&2; exit 1; }
    status=0
    probe "$layer-red" "$expected" || status=$?
    restore
    git diff --stat >"$logs/$layer-restored.stat"
    cat "$logs/$layer-restored.stat"
    [[ ! -s "$logs/$layer-restored.stat" ]] || { echo 'Restore left a tracked diff' >&2; exit 1; }
    [[ "$status" != 0 ]] || { echo "UNDEFENDED: $expected stayed green" >&2; exit 1; }
    python3 - "$logs/$layer-red.log" "$expected" "$status_marker" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
name, marker = sys.argv[2:]
summary = re.search(r"\nfailures:\n\n?((?:    [^\n]+\n)+)\n", text)
assert summary, text
assert re.findall(r"^    (\S+)$", summary.group(1), re.M) == [name], text
assert "test result: FAILED. 0 passed; 1 failed;" in text, text
assert marker in text, "Expected denial assertion was not reached:\n" + text
print(f"NON-VACUITY BREAK reddened only {name}; all other tests filtered out")
PY
    probe "$layer-restored-green" "$expected"
done
# Ensure the final executable contains both restored protections.
probe final-throwaway-green keychain::worker_jail_throwaway_keychain_is_unreadable
probe final-file-green keychain::worker_jail_default_keychain_file_is_unreadable
git diff --stat
git diff --exit-code
git diff --cached --exit-code
printf 'Host keychain gates passed. Logs: %s\nDisposable storage: %s\n' "$logs" "$storage"
