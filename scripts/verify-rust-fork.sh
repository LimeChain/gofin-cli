#!/bin/sh
# Isolated verification only. This script never opens production credentials.
set -eu
umask 077
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
fin_anvil=${FIN_ANVIL_PATH:-"$HOME/.foundry/bin/anvil"}
case "$fin_anvil" in /*) ;; *) echo 'FIN_ANVIL_PATH must be an absolute installed Anvil path.' >&2; exit 1 ;; esac
[ -x "$fin_anvil" ] || { echo 'Install Foundry Anvil separately; no executable is downloaded by this test.' >&2; exit 1; }
cd "$fin_source"
# Compile before starting the fixture. No ignored financial test runs here yet.
cargo test --locked -p fin-broker --test fork_roundtrip --no-run
mkdir -p "$fin_source/work"
fin_evidence=$(mktemp -d "$fin_source/work/rust-fork.XXXXXX")
fin_pid=''
cleanup() {
  if [ -n "$fin_pid" ]; then kill "$fin_pid" 2>/dev/null || true; wait "$fin_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
# A new listener must bind this port. Never attach a test to somebody else's node.
python3 - <<'PY'
import socket
with socket.socket() as peer:
    try:
        peer.bind(('127.0.0.1', 8545))
    except OSError:
        raise SystemExit('Port 8545 is occupied. Stop your own node before starting a fresh fixture.')
PY
"$fin_anvil" --host 127.0.0.1 --port 8545 --chain-id 1 \
  --fork-url https://eth.drpc.org --fork-block-number 26099326 \
  --timestamp 1790880107 --no-cors > "$fin_evidence/anvil.log" 2>&1 &
fin_pid=$!
# Bound readiness wait; the Rust test independently repeats node/hash/chain
# guards before every signature and mutation. There is no remote live fallback.
fin_ready=0
fin_attempt=0
while [ "$fin_attempt" -lt 30 ]; do
  kill -0 "$fin_pid" 2>/dev/null || { echo "Anvil failed to start; inspect $fin_evidence/anvil.log." >&2; exit 1; }
  if python3 - <<'PY'
import json, urllib.request
try:
    payload=b'{"jsonrpc":"2.0","id":1,"method":"anvil_metadata","params":[]}'
    request=urllib.request.Request('http://127.0.0.1:8545',data=payload,headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(request,timeout=1) as response:
        body=json.load(response)
    meta=body['result']; fork=meta['forkedNetwork']
    assert meta['chainId']==1 and fork['forkBlockNumber']==26099326
    assert fork['forkBlockHash']=='0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf'
except Exception:
    raise SystemExit(1)
PY
  then fin_ready=1; break; fi
  fin_attempt=$((fin_attempt + 1))
  sleep 1
done
[ "$fin_ready" = 1 ] || { echo "Pinned archive fixture unavailable; evidence at $fin_evidence. No test passed." >&2; exit 1; }
echo "Guarded synthetic fork evidence: $fin_evidence"
if cargo test --locked -p fin-broker --test fork_roundtrip -- --ignored --nocapture > "$fin_evidence/test.log" 2>&1; then
  cat "$fin_evidence/test.log"
else
  cat "$fin_evidence/test.log"
  echo 'Fork verification failed. Node disposal does not establish financial restoration.' >&2
  exit 1
fi
