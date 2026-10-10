#!/usr/bin/env bash
# Run the market-making scenario: oracle → bootstrap → router → makers → flow.
#
#   scenario.sh deploy [local|testnet]  deploy the three DEX fork artifacts
#   scenario.sh up [local|testnet]      build, start every service, bootstrap
#   scenario.sh bootstrap [local|testnet]   tokens + pools + wallet top-ups only
#   scenario.sh status [local|testnet]  pools, wallets, processes
#   scenario.sh observe                 Prometheus :9190 + Grafana :3100 (dashboard
#                                       "mtm — market-making scenario")
#   scenario.sh down                    stop every service started by `up`/`observe`
#
# Secrets come from the environment (normally `doppler run --`; see
# bootstrap/scenario/README.md). Every step is idempotent, so `up` can be
# re-run after any failure and replays the same scenario: wallets derive from
# SCENARIO_SEED, pools are only created when missing, and wallets are only
# topped up to their targets.
set -euo pipefail

cmd="${1:-up}"
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
export MTM_RPC__HTTP_URL="$rpc"
if [[ -n "${SCENARIO_WS_URL:-}" ]]; then
  export MTM_RPC__WS_URL="$SCENARIO_WS_URL"
elif [[ "$target" == local ]]; then
  export MTM_RPC__WS_URL="ws://127.0.0.1:$(( ${rpc##*:} + 1 ))" # Surfpool: RPC port + 1
else
  export MTM_RPC__WS_URL="${rpc/https/wss}"
fi

bin="$root/target/${SCENARIO_BUILD:-release}"
build_flag=$([[ "${SCENARIO_BUILD:-release}" == release ]] && echo --release || true)

pubkey_of() { # env var holding a keypair or pubkey → pubkey
  local value="${!1:?missing secret: $1}"
  if [[ "$value" == \[* ]]; then solana-keygen pubkey <(printf %s "$value"); else printf %s "$value"; fi
}

start() { # name, command...
  local name="$1"; shift
  if [[ -f "$state/pids/$name" ]] && kill -0 "$(cat "$state/pids/$name")" 2>/dev/null; then
    echo "$name already running (pid $(cat "$state/pids/$name"))"; return
  fi
  nohup "$@" >>"$state/logs/$name.log" 2>&1 &
  echo $! >"$state/pids/$name"
  echo "started $name (pid $!, log .scenario/logs/$name.log)"
}

wait_http() { # url, label
  for _ in $(seq 1 120); do curl -sf "$1" >/dev/null && return; sleep 1; done
  echo "$2 did not become healthy at $1" >&2; exit 1
}

build() {
  cargo build -q $build_flag -p oracle -p router -p mm -p flow -p scenario
}

tokens() {
  # Mirrors mainnet metadata onto the target; existing mints are left alone.
  # Call tsx directly when installed: `pnpm run` re-checks the install first.
  local runner=(pnpm bootstrap:tokens)
  [[ -x node_modules/.bin/tsx ]] && runner=(node_modules/.bin/tsx bootstrap/tokens/deploy.ts)
  "${runner[@]}" --apply --skip-existing --testnet-rpc "$rpc" \
    ${SCENARIO_MAINNET_RPC:+--mainnet-rpc "$SCENARIO_MAINNET_RPC"}
}

router_env() {
  export MTM_ROUTER__PROGRAMS__RAYDIUM_CLMM="$(pubkey_of KEYPAIR_RAYDIUM_CLMM)"
  export MTM_ROUTER__PROGRAMS__ORCA_WHIRLPOOL="$(pubkey_of KEYPAIR_ORCA_WHIRLPOOL)"
  export MTM_ROUTER__PROGRAMS__MANIFEST="$(pubkey_of KEYPAIR_MANIFEST)"
  export MTM_ROUTER__BRIDGE_MINTS="[\"$(pubkey_of PUBKEY_TOKEN_USDC_MINT)\"]"
  export MTM_ROUTER__JUPITER__PROGRAM_ID="${PUBKEY_JUPITER_FORK:-}"
}

deploy() {
  # Deploys each fork artifact at its program keypair, skipping programs that
  # are already deployed. Paths default to sibling fork checkouts.
  # Manifest is SBPF v3: use an Agave >= 4.x `solana` CLI.
  umask 077
  local dir
  if [[ -d /dev/shm && -w /dev/shm ]]; then dir="$(mktemp -d /dev/shm/kp.XXXXXX)"; else dir="$(mktemp -d)"; fi
  trap 'rm -rf "$dir"' RETURN
  printf %s "${KEYPAIR_PROGRAM_AUTHORITY:?missing secret}" >"$dir/authority.json"
  printf %s "${KEYPAIR_FAUCET:?missing secret}" >"$dir/faucet.json"
  local flags=()
  [[ "$target" == local ]] && flags+=(--use-rpc) # Surfpool has no TPU/QUIC
  for spec in \
    "KEYPAIR_RAYDIUM_CLMM:${SCENARIO_RAYDIUM_SO:-../raydium-clmm/target/deploy/raydium_clmm.so}" \
    "KEYPAIR_ORCA_WHIRLPOOL:${SCENARIO_ORCA_SO:-../whirlpools/target/deploy/whirlpool.so}" \
    "KEYPAIR_MANIFEST:${SCENARIO_MANIFEST_SO:-../manifest/target/deploy/manifest.so}"; do
    local var="${spec%%:*}" so="${spec#*:}"
    if solana program show -u "$rpc" "$(pubkey_of "$var")" >/dev/null 2>&1; then
      echo "$var already deployed"; continue
    fi
    printf %s "${!var:?missing secret: $var}" >"$dir/program.json"
    solana program deploy -u "$rpc" "${flags[@]}" --program-id "$dir/program.json" \
      --upgrade-authority "$dir/authority.json" --fee-payer "$dir/faucet.json" "$so"
  done
}

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
  echo "grafana http://127.0.0.1:3100 (admin/${GRAFANA_PASSWORD:-admin}), prometheus http://127.0.0.1:9190"
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
    for _ in $(seq 1 60); do
      [[ "$(curl -sf http://127.0.0.1:8180/v1/prices | grep -o '"symbol"' | wc -l)" -ge 3 ]] && break
      sleep 1
    done
    "$bin/scenario" bootstrap | tee "$state/bootstrap.json"
    (router_env; start router "$bin/router")
    wait_http http://127.0.0.1:8181/health router
    start mm "$bin/mm" --state-dir "$state/state"
    # Give makers a few ticks to put liquidity on every venue.
    sleep "${SCENARIO_WARMUP_S:-20}"
    start flow "$bin/flow"
    echo "scenario up — oracle :8180, router :8181; metrics :9180-9183"
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
      if kill -0 "$(cat "$pid")" 2>/dev/null; then echo "$name: running"; else echo "$name: stopped"; fi
    done
    ;;
  down)
    for name in flow mm router oracle grafana prometheus; do
      pid="$state/pids/$name"
      [[ -f "$pid" ]] || continue
      kill "$(cat "$pid")" 2>/dev/null && echo "stopped $name" || true
      rm -f "$pid"
    done
    ;;
  *) echo "unknown command: $cmd (deploy|up|bootstrap|observe|status|down)" >&2; exit 2 ;;
esac
