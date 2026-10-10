#!/usr/bin/env bash
# Run the market-making scenario: oracle → bootstrap → router → makers → flow + arb.
#
#   scenario.sh deploy [local|testnet]  deploy the three DEX fork artifacts
#   scenario.sh up [local|testnet]      build, start every service, bootstrap
#   scenario.sh bootstrap [local|testnet]   tokens + pools + wallet top-ups only
#   scenario.sh status [local|testnet]  pools, wallets, processes
#   scenario.sh observe                 Prometheus :9190 + Grafana :3100 (dashboard
#                                       "mtm — market-making scenario")
#   scenario.sh snapshot FILE [local|testnet]   participant holdings at oracle prices
#   scenario.sh report START END [local|testnet] PnL, transactions, trades between snapshots
#   scenario.sh measure NAME SECONDS [local|testnet] snapshot → sample load → snapshot → report
#                                       into .scenario/runs/NAME-{t0,t1,load,report}.json
#
# SCENARIO_SCALE=x10|x100|assets layers config/scale-<scale>.toml (generated
# by scale.py) over the profile; SCENARIO_TOKENS_CONFIG picks the token
# bootstrap config (the asset scale uses bootstrap/scenario/tokens-assets.json).
#   scenario.sh down                    stop every service started by `up`/`observe`
#                                       (TERM, then KILL after 30 s)
#
# Secrets come from the environment (normally `doppler run --`; see
# bootstrap/scenario/README.md). Every step is idempotent, so `up` can be
# re-run after any failure and replays the same scenario: wallets derive from
# SCENARIO_SEED, pools are only created when missing, and wallets are only
# topped up to their targets.
set -euo pipefail

cmd="${1:-up}"
args=()
case "$cmd" in
  snapshot) args=("${2:?snapshot FILE}"); shift ;;
  report) args=("${2:?report START END}" "${3:?report START END}"); shift 2 ;;
  measure) args=("${2:?measure NAME SECONDS}" "${3:?measure NAME SECONDS}"); shift 2 ;;
esac
target="${2:-local}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state="$root/.scenario"
mkdir -p "$state/logs" "$state/pids" "$state/state"
cd "$root"

case "$target" in
  local)
    export MTM_PROFILE=scenario-local
    rpc="${SCENARIO_RPC_URL:-http://127.0.0.1:8899}"
    ;;
  testnet)
    export MTM_PROFILE=scenario-testnet
    rpc="${SCENARIO_RPC_URL:-https://api.testnet.solana.com}"
    ;;
  *) echo "unknown target: $target (local|testnet)" >&2; exit 2 ;;
esac
[[ -n "${SCENARIO_SCALE:-}" ]] && export MTM_PROFILE="$MTM_PROFILE,scale-$SCENARIO_SCALE"
export MTM_RPC__HTTP_URL="$rpc"
if [[ -n "${SCENARIO_WS_URL:-}" ]]; then
  export MTM_RPC__WS_URL="$SCENARIO_WS_URL"
elif [[ "$target" == local ]]; then
  export MTM_RPC__WS_URL="ws://127.0.0.1:$(( ${rpc##*:} + 1 ))" # Surfpool: RPC port + 1
else
  export MTM_RPC__WS_URL="${rpc/https/wss}"
fi

bin="${CARGO_TARGET_DIR:-$root/target}/${SCENARIO_BUILD:-release}"
build_flag=$([[ "${SCENARIO_BUILD:-release}" == release ]] && echo --release || true)

pubkey_of() { # env var holding a keypair or pubkey → pubkey
  local value="${!1:?missing secret: $1}"
  if [[ "$value" == \[* ]]; then solana-keygen pubkey <(printf %s "$value"); else printf %s "$value"; fi
}

running() { # name → true if its pid file names a live process running that binary
  local pid
  pid="$(cat "$state/pids/$1" 2>/dev/null)" || return 1
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null || return 1
  # A pid reused by an unrelated process does not count.
  tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q -- "$1"
}

start() { # name, command...
  local name="$1"; shift
  if running "$name"; then
    echo "$name already running (pid $(cat "$state/pids/$name"))"; return
  fi
  nohup "$@" >>"$state/logs/$name.log" 2>&1 &
  local pid=$!
  echo "$pid" >"$state/pids/$name"
  # Until the fork execs, its cmdline is still this script's, and `running`
  # would report the service as already gone.
  for _ in $(seq 1 50); do
    kill -0 "$pid" 2>/dev/null || break
    tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -q -- "$name" && break
    sleep 0.1
  done
  echo "started $name (pid $pid, log .scenario/logs/$name.log)"
}

stop() { # name: TERM, wait up to 30 s, then KILL; the pid file goes only once it is gone
  local name="$1" pid
  running "$name" || { rm -f "$state/pids/$name"; return 0; }
  pid="$(cat "$state/pids/$name")"
  kill -CONT "$pid" 2>/dev/null || true # a SIGSTOPped process never handles TERM
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 60); do
    kill -0 "$pid" 2>/dev/null || { rm -f "$state/pids/$name"; echo "stopped $name"; return 0; }
    sleep 0.5
  done
  kill -KILL "$pid" 2>/dev/null || true
  rm -f "$state/pids/$name"
  echo "killed $name (no exit 30 s after TERM)"
}

wait_http() { # url, name (as given to start)
  for _ in $(seq 1 120); do
    curl -sf "$1" >/dev/null && return
    # A process that already exited will never answer: say why, now.
    if ! running "$2"; then
      echo "$2 exited before becoming healthy at $1; last log lines:" >&2
      tail -n 5 "$state/logs/$2.log" >&2 2>/dev/null || true
      exit 1
    fi
    sleep 1
  done
  echo "$2 did not become healthy at $1" >&2; exit 1
}

require_agave_cli() { # deploys need an Agave >= 4.x CLI (Manifest is SBPF v3)
  local version
  version="$(solana --version 2>/dev/null | awk '{print $2}')"
  if [[ -z "$version" || "${version%%.*}" -lt 4 ]]; then
    echo "need the Agave 4.x solana CLI on PATH (found: ${version:-none}): Manifest is SBPF v3;" \
      "install it with \`agave-install init <4.x version>\`" >&2
    exit 1
  fi
}

build() {
  cargo build -q $build_flag -p oracle -p router -p mm -p flow -p arb -p scenario
}

tokens() {
  # Mirrors mainnet metadata onto the target; existing mints are left alone.
  # Call tsx directly when installed: `pnpm run` re-checks the install first.
  local runner=(pnpm bootstrap:tokens)
  [[ -x node_modules/.bin/tsx ]] && runner=(node_modules/.bin/tsx bootstrap/tokens/deploy.ts)
  "${runner[@]}" --apply --skip-existing --testnet-rpc "$rpc" \
    ${SCENARIO_TOKENS_CONFIG:+--config "$SCENARIO_TOKENS_CONFIG"} \
    ${SCENARIO_MAINNET_RPC:+--mainnet-rpc "$SCENARIO_MAINNET_RPC"}
}

router_env() {
  export MTM_ROUTER__PROGRAMS__RAYDIUM_CLMM="$(pubkey_of KEYPAIR_RAYDIUM_CLMM)"
  export MTM_ROUTER__PROGRAMS__ORCA_WHIRLPOOL="$(pubkey_of KEYPAIR_ORCA_WHIRLPOOL)"
  export MTM_ROUTER__PROGRAMS__MANIFEST="$(pubkey_of KEYPAIR_MANIFEST)"
  export MTM_ROUTER__BRIDGE_MINTS="[\"$(pubkey_of PUBKEY_TOKEN_USDC_MINT)\"]"
  export MTM_ROUTER__JUPITER__PROGRAM_ID="${PUBKEY_JUPITER_FORK:-}"
}

program_exists() { # pubkey → 0 deployed, 1 absent; exits on an RPC error
  # Only a definite "no account" may lead to `program deploy --program-id`,
  # which would *upgrade* an existing program: a transient RPC error must not.
  # At `confirmed`: the RPC default (finalized) misses a program deployed in
  # the last ~30 slots.
  local body
  body="$(curl -sf -H 'content-type: application/json' "$rpc" -d \
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"$1\",{\"encoding\":\"base64\",\"commitment\":\"confirmed\",\"dataSlice\":{\"offset\":0,\"length\":0}}]}")" \
    || { echo "getAccountInfo $1 failed at $rpc" >&2; exit 1; }
  case "$(jq -r 'if .error then "error" elif .result.value == null then "absent" else "present" end' <<<"$body")" in
    present) return 0 ;;
    absent) return 1 ;;
    *) echo "getAccountInfo $1: $(jq -c .error <<<"$body")" >&2; exit 1 ;;
  esac
}

deploy() (
  # Deploys each fork artifact at its program keypair, skipping programs that
  # are already deployed. Paths default to sibling fork checkouts.
  # Manifest is SBPF v3: use an Agave >= 4.x `solana` CLI.
  # A subshell, so umask and traps stay local; the key directory is removed
  # on any exit, Ctrl-C and SIGTERM included.
  require_agave_cli
  umask 077
  dir=""
  trap '[[ -n "$dir" ]] && rm -rf "$dir"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if [[ -d /dev/shm && -w /dev/shm ]]; then dir="$(mktemp -d /dev/shm/kp.XXXXXX)"; else dir="$(mktemp -d)"; fi
  printf %s "${KEYPAIR_PROGRAM_AUTHORITY:?missing secret}" >"$dir/authority.json"
  printf %s "${KEYPAIR_FAUCET:?missing secret}" >"$dir/faucet.json"
  flags=()
  [[ "$target" == local ]] && flags+=(--use-rpc) # Surfpool has no TPU/QUIC
  for spec in \
    "KEYPAIR_RAYDIUM_CLMM:${SCENARIO_RAYDIUM_SO:-../raydium-clmm/target/deploy/raydium_clmm.so}" \
    "KEYPAIR_ORCA_WHIRLPOOL:${SCENARIO_ORCA_SO:-../whirlpools/target/deploy/whirlpool.so}" \
    "KEYPAIR_MANIFEST:${SCENARIO_MANIFEST_SO:-../manifest/target/deploy/manifest.so}"; do
    var="${spec%%:*}" so="${spec#*:}"
    if program_exists "$(pubkey_of "$var")"; then
      echo "$var already deployed"; continue
    fi
    printf %s "${!var:?missing secret: $var}" >"$dir/program.json"
    solana program deploy -u "$rpc" "${flags[@]}" --program-id "$dir/program.json" \
      --upgrade-authority "$dir/authority.json" --fee-payer "$dir/faucet.json" "$so"
  done
)

observe() {
  # Docker when usable (ops/compose.scenario.yaml), else `prometheus` and
  # `grafana` binaries from PATH run as the current user.
  local compose="${MTM_COMPOSE:-docker compose}"
  if $compose version >/dev/null 2>&1 && ${compose%% *} ps >/dev/null 2>&1; then
    $compose -f ops/compose.scenario.yaml up -d
    return
  fi
  command -v prometheus >/dev/null && command -v grafana >/dev/null || {
    echo "need docker (or prometheus + grafana binaries on PATH) for observability" >&2; exit 1; }
  start prometheus prometheus --config.file="$root/ops/prometheus.scenario.yml" \
    --web.listen-address=127.0.0.1:9190 --storage.tsdb.path="$state/prometheus"
  local prov="$state/grafana/provisioning"
  mkdir -p "$prov/datasources" "$prov/dashboards" "$state/grafana/data"
  cp "$root/ops/grafana/scenario/datasources.yml" "$prov/datasources/"
  sed "s#/var/lib/grafana/dashboards#$root/ops/grafana/dashboards#" \
    "$root/ops/grafana/scenario/dashboards.yml" >"$prov/dashboards/dashboards.yml"
  local home; home="$(dirname "$(dirname "$(readlink -f "$(command -v grafana)")")")"
  GF_PATHS_PROVISIONING="$prov" GF_PATHS_DATA="$state/grafana/data" \
    GF_SERVER_HTTP_ADDR=127.0.0.1 GF_SERVER_HTTP_PORT=3100 \
    GF_SECURITY_ADMIN_PASSWORD="${GRAFANA_PASSWORD:-admin}" \
    GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH="$root/ops/grafana/dashboards/scenario.json" \
    start grafana grafana server --homepath "$home"
  echo "grafana http://127.0.0.1:3100 (user admin, password \$GRAFANA_PASSWORD or admin), prometheus http://127.0.0.1:9190"
}

case "$cmd" in
  observe)
    observe
    ;;
  deploy)
    deploy
    ;;
  up)
    build
    tokens
    start oracle "$bin/oracle"
    wait_http http://127.0.0.1:8180/health oracle
    # Pools are created at live oracle prices: wait for the first prices.
    want="$("$bin/scenario" token-count 2>/dev/null || echo 3)"
    for _ in $(seq 1 120); do
      [[ "$(curl -sf http://127.0.0.1:8180/v1/prices | grep -o '"symbol"' | wc -l)" -ge "$want" ]] && break
      sleep 1
    done
    "$bin/scenario" bootstrap | tee "$state/bootstrap.json"
    (router_env; start router "$bin/router")
    wait_http http://127.0.0.1:8181/health router
    start mm "$bin/mm" --state-dir "$state/state"
    # Give makers a few ticks to put liquidity on every venue.
    sleep "${SCENARIO_WARMUP_S:-20}"
    start flow "$bin/flow"
    start arb "$bin/arb"
    echo "scenario up — oracle :8180, router :8181; metrics :9180-9184"
    ;;
  bootstrap)
    build
    tokens
    "$bin/scenario" bootstrap
    ;;
  status)
    "$bin/scenario" status
    for pid in "$state"/pids/*; do
      [[ -f "$pid" ]] || continue
      name="$(basename "$pid")"
      if running "$name"; then echo "$name: running"; else echo "$name: stopped"; fi
    done
    ;;
  snapshot)
    "$bin/scenario" snapshot --out "${args[0]}"
    ;;
  report)
    "$bin/scenario" report --start "${args[0]}" --end "${args[1]}" \
      --prometheus "${SCENARIO_PROMETHEUS:-http://127.0.0.1:9190}"
    ;;
  measure)
    # Participants are frozen (SIGSTOP) while each snapshot reads, so every
    # wallet, seat, and position is read in the same chain state.
    name="${args[0]}" seconds="${args[1]}" runs="$state/runs"
    mkdir -p "$runs"
    participants=(flow arb mm)
    signal() {
      for n in "${participants[@]}"; do
        [[ -f "$state/pids/$n" ]] && kill "-$1" "$(cat "$state/pids/$n")" 2>/dev/null || true
      done
    }
    frozen_snapshot() {
      signal STOP
      sleep "${SCENARIO_SETTLE_S:-3}" # let in-flight transactions land
      "$bin/scenario" snapshot --out "$1" >&2 || { signal CONT; return 1; }
      signal CONT
    }
    trap 'signal CONT' EXIT
    frozen_snapshot "$runs/$name-t0.json"
    python3 -I "$root/bootstrap/scenario/sample.py" --seconds "$seconds" --rpc "$rpc" \
      --out "$runs/$name-load.json" >/dev/null
    frozen_snapshot "$runs/$name-t1.json"
    "$bin/scenario" report --start "$runs/$name-t0.json" --end "$runs/$name-t1.json" \
      --prometheus "${SCENARIO_PROMETHEUS:-http://127.0.0.1:9190}" >"$runs/$name-report.json"
    echo "wrote $runs/$name-{t0,t1,load,report}.json"
    ;;
  down)
    # Takers first, so nothing trades against makers that are unwinding.
    for name in arb flow mm router oracle grafana prometheus; do
      if [[ -f "$state/pids/$name" ]]; then stop "$name"; fi
    done
    compose="${MTM_COMPOSE:-docker compose}"
    if $compose version >/dev/null 2>&1 && ${compose%% *} ps >/dev/null 2>&1 \
      && [[ -n "$($compose -f ops/compose.scenario.yaml ps -q 2>/dev/null)" ]]; then
      $compose -f ops/compose.scenario.yaml down # volumes are kept
    fi
    ;;
  *) echo "unknown command: $cmd (deploy|up|bootstrap|observe|status|snapshot|report|measure|down)" >&2; exit 2 ;;
esac
