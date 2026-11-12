#!/usr/bin/env bash
# Run the market-making scenario: oracle → bootstrap → router → makers → flow + arb.
#
#   scenario.sh deploy [local|devnet|testnet]  deploy the three DEX fork artifacts and record
#                                       their hashes (bootstrap/scenario/programs-<target>.lock);
#                                       a deployed program not running its recorded code
#                                       (e.g. left as the janitor) is refused, or upgraded
#                                       to the artifact with SCENARIO_UPGRADE_PROGRAMS=1
#   scenario.sh up [local|devnet|testnet]      build, check every DEX program runs its recorded
#                                       code (by hash), start every service, bootstrap
#   scenario.sh bootstrap [local|devnet|testnet]   tokens + pools + wallet top-ups only
#   scenario.sh status [local|devnet|testnet]  pools, wallets, processes
#   scenario.sh observe                 Prometheus :9190 + Grafana :3100 (dashboard
#                                       "mtm — market-making scenario")
#   scenario.sh watch [local|devnet|testnet]  chain-watch alone (API :8183, metrics :9185):
#                                       slots, blocks, versions, feature gates into
#                                       .scenario/runs/<run id>/run.db. `up` starts it too.
#                                       CHAIN_WATCH_RPC_URL / CHAIN_WATCH_WS_URL (Doppler)
#                                       override the scenario RPC for it; SCENARIO_RUN_ID
#                                       names the run (default <target>-<UTC time>).
#   scenario.sh snapshot FILE [local|devnet|testnet]   participant holdings at oracle prices
#   scenario.sh report START END [local|devnet|testnet] PnL, transactions, trades between snapshots
#   scenario.sh measure NAME SECONDS [local|devnet|testnet] snapshot → sample load → snapshot → report
#                                       into .scenario/runs/NAME-{t0,t1,load,report}.json
#
# SCENARIO_SCALE=<name> layers config/scale-<name>.toml over the profile: a
# preset (x10|x100|assets|x200|x500|x750|x1000|x1500|x2000) or a composed
# spec (c-<hash>), both written by `scenario overlay`. `up` records it in
# .scenario/scale, and later commands use that unless SCENARIO_SCALE names it
# (a different one is refused). SCENARIO_TOKENS_CONFIG picks the token
# bootstrap config; an overlay that lists its own tokens defaults it to
# bootstrap/scenario/tokens-assets.json.
#   scenario.sh down                    stop every service started by `up`/`observe`
#                                       (TERM, then KILL after 30 s)
#   scenario.sh teardown [local|devnet|testnet]  stop the scenario services, unwind every
#                                       participant, sweep all SOL to the faucet, and
#                                       write .scenario/{teardown,leftovers}.json
#                                       (rent still locked in pools, markets, …).
#                                       The DEX programs stay deployed: a closed program
#                                       ID can never be reused. Operator wallets are swept too on
#                                       local (SCENARIO_SWEEP_OPERATORS=0|1 overrides).
#                                       SCENARIO_JANITOR=1 first upgrades the DEX programs
#                                       to SCENARIO_JANITOR_SO and closes every account
#                                       they own (pools, tick arrays, positions, markets)
#                                       and every vault their PDAs hold, then restores the
#                                       forks (the fork artifacts must be the deployed code).
#   scenario.sh supervise NAME CMD...   (internal) rerun CMD whenever it exits
#
# Secrets come from the environment (normally `doppler run --`; see
# bootstrap/scenario/README.md). Every step is idempotent, so `up` can be
# re-run after any failure and replays the same scenario: wallets derive from
# SCENARIO_SEED, pools are only created when missing, and wallets are only
# topped up to their targets.
set -euo pipefail

supervise() { # name, command...: rerun the command whenever it exits, with bounded backoff
  # `up` runs the oracle under this (`scenario.sh supervise oracle <bin>`):
  # the oracle exits on any dead task and must come back on its own. TERM
  # (from `stop`) goes to the child and ends the loop. Backoff doubles from
  # 1 s to 30 s while the command keeps dying within a minute, and resets
  # after a longer run; after SCENARIO_MAX_FAST_RESTARTS (default 5) fast
  # exits in a row it gives up, so a broken config fails `up` instead of
  # looping forever.
  local name="$1"; shift
  local child="" delay=1 fast=0 restarts=0 started ran status
  local max_fast="${SCENARIO_MAX_FAST_RESTARTS:-5}"
  trap '[[ -n "$child" ]] && kill -TERM "$child" 2>/dev/null; wait "$child" 2>/dev/null; exit 143' TERM INT
  while true; do
    started=$SECONDS
    "$@" &
    child=$!
    status=0; wait "$child" || status=$?
    child=""
    ran=$(( SECONDS - started ))
    if (( ran >= 60 )); then delay=1 fast=0; else fast=$(( fast + 1 )); fi
    if (( fast > max_fast )); then
      echo "$(date -Is) supervise: $name exited (status $status) $fast times in a row within 60 s; giving up"
      exit 1
    fi
    restarts=$(( restarts + 1 ))
    echo "$(date -Is) supervise: $name exited (status $status) after ${ran}s; restart #$restarts in ${delay}s"
    sleep "$delay" & child=$!
    wait "$child" || true
    child=""
    delay=$(( delay * 2 > 30 ? 30 : delay * 2 ))
  done
}

if [[ "${1:-}" == supervise ]]; then
  shift
  supervise "$@"
fi

usage() { echo "usage: $0 {deploy|up|bootstrap|observe|watch|status|down} [local|devnet|testnet]" \
  "| snapshot FILE [target] | report START END [target] | measure NAME SECONDS [target]" >&2; exit 2; }
is_target() { [[ "$1" == local || "$1" == devnet || "$1" == testnet ]]; }

# Positional arguments, checked strictly: a forgotten operand must not shift
# the target into its place (e.g. `measure x10 testnet` running locally).
cmd="${1:-up}"
[[ $# -gt 0 ]] && shift
args=()
case "$cmd" in
  snapshot) operands=1 ;;
  report | measure) operands=2 ;;
  *) operands=0 ;;
esac
for _ in $(seq 1 "$operands"); do
  [[ $# -gt 0 ]] && ! is_target "$1" || usage
  args+=("$1"); shift
done
if [[ "$cmd" == measure ]] && ! [[ "${args[1]}" =~ ^[0-9]+$ ]]; then
  echo "measure: SECONDS must be a whole number, got '${args[1]}'" >&2; exit 2
fi
target="${1:-local}"
[[ $# -le 1 ]] || usage
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state="$root/.scenario"
mkdir -p "$state/logs" "$state/pids" "$state/state"
cd "$root"

case "$target" in
  local)
    export MTM_PROFILE=scenario-local
    rpc="${SCENARIO_RPC_URL:-http://127.0.0.1:8899}"
    ;;
  devnet)
    # A rehearsal of the testnet run: same profile shape, devnet endpoints.
    export MTM_PROFILE=scenario-devnet
    rpc="${SCENARIO_RPC_URL:-https://api.devnet.solana.com}"
    ;;
  testnet)
    export MTM_PROFILE=scenario-testnet
    rpc="${SCENARIO_RPC_URL:-https://api.testnet.solana.com}"
    ;;
  *) echo "unknown target: $target (local|devnet|testnet)" >&2; exit 2 ;;
esac
# The scale `up` started at, so later commands resolve the same participants
# (a snapshot or report at another scale reads a different wallet set). Unset,
# the recorded scale applies; a different one is refused.
if [[ "$cmd" == up ]]; then
  printf %s "${SCENARIO_SCALE:-}" >"$state/scale"
elif [[ "$cmd" =~ ^(bootstrap|status|snapshot|report|measure|teardown)$ && -f "$state/scale" ]]; then
  recorded="$(cat "$state/scale")"
  if [[ -z "${SCENARIO_SCALE+set}" ]]; then
    SCENARIO_SCALE="$recorded"
    [[ -n "$recorded" ]] && echo "using the running scenario's scale: $recorded (.scenario/scale)" >&2
  elif [[ "$SCENARIO_SCALE" != "$recorded" ]]; then
    echo "$cmd: the scenario was started at scale '${recorded:-base}', not '${SCENARIO_SCALE:-base}' (.scenario/scale)" >&2
    exit 2
  fi
fi
if [[ -n "${SCENARIO_SCALE:-}" ]]; then
  overlay="config/scale-$SCENARIO_SCALE.toml"
  # The config loader skips a missing profile file: a typo would run the base profile.
  [[ -f "$overlay" ]] || { echo "no overlay $overlay (scenario overlay writes it)" >&2; exit 2; }
  export MTM_PROFILE="$MTM_PROFILE,scale-$SCENARIO_SCALE"
  if grep -q '^\[\[scenario\.tokens\]\]' "$overlay"; then
    : "${SCENARIO_TOKENS_CONFIG:=bootstrap/scenario/tokens-assets.json}"
  fi
fi
export MTM_RPC__HTTP_URL="$rpc"
if [[ -n "${SCENARIO_WS_URL:-}" ]]; then
  export MTM_RPC__WS_URL="$SCENARIO_WS_URL"
elif [[ "$target" == local && "$rpc" =~ ^http://([^/:]+):([0-9]+)/?$ ]]; then
  # Surfpool and solana-test-validator: websocket on the RPC port + 1.
  export MTM_RPC__WS_URL="ws://${BASH_REMATCH[1]}:$(( BASH_REMATCH[2] + 1 ))"
elif [[ "$target" == local ]]; then
  echo "cannot derive the websocket URL from $rpc: set SCENARIO_WS_URL" >&2; exit 2
else
  export MTM_RPC__WS_URL="${rpc/https/wss}"
fi

# chain-watch observes through its own endpoint when Doppler sets one (the
# provider's key is in the URL); otherwise it shares the scenario's.
if [[ -n "${CHAIN_WATCH_RPC_URL:-}" ]]; then
  export MTM_CHAIN_WATCH__RPC__HTTP_URL="$CHAIN_WATCH_RPC_URL"
  watch_ws="${CHAIN_WATCH_RPC_URL/https/wss}"
  export MTM_CHAIN_WATCH__RPC__WS_URL="${CHAIN_WATCH_WS_URL:-${watch_ws/http/ws}}"
elif [[ -n "${CHAIN_WATCH_WS_URL:-}" ]]; then
  export MTM_CHAIN_WATCH__RPC__WS_URL="$CHAIN_WATCH_WS_URL"
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
  cargo build -q $build_flag -p oracle -p router -p mm -p flow -p arb -p scenario -p chain-watch
}

watch() { # start chain-watch first, so the run store covers bootstrap too
  running chain-watch && { echo "chain-watch already running (pid $(cat "$state/pids/chain-watch"))"; return; }
  MTM_CHAIN_WATCH__RUN_ID="${SCENARIO_RUN_ID:-$target-$(date -u +%Y%m%dT%H%M%SZ)}" \
    start chain-watch "$bin/chain-watch"
  wait_http http://127.0.0.1:8183/health chain-watch
}

# Passed to `scenario check-programs`, so a program left running the janitor
# is named as such.
janitor_check=()
if [[ -f "${SCENARIO_JANITOR_SO:-}" ]]; then janitor_check=(--janitor-so "$SCENARIO_JANITOR_SO"); fi
# The code each DEX program should run, by sha256, as `deploy` recorded it:
# tracked for shared targets (commit it, so a host without the fork
# artifacts, or with another build of them, still verifies), run state
# locally. A program with no entry is compared with its artifact; with
# neither, a remote target refuses it.
programs_lock="${SCENARIO_PROGRAMS_LOCK:-$([[ "$target" == local ]] \
  && echo "$state/programs-local.lock" || echo "$root/bootstrap/scenario/programs-$target.lock")}"
program_check=(--lock "$programs_lock" "${janitor_check[@]}")
[[ "$target" == local ]] || program_check+=(--require-verified)
# Written before the janitor closes anything, removed once a run finished
# with nothing left: while it exists, pools may have lost their vaults.
janitor_incomplete="$state/janitor-incomplete"

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

artifacts() { # program keypair var:fork artifact, one per DEX (defaults: sibling fork checkouts)
  printf '%s\n' \
    "KEYPAIR_RAYDIUM_CLMM:${SCENARIO_RAYDIUM_SO:-../raydium-clmm/target/deploy/raydium_clmm.so}" \
    "KEYPAIR_ORCA_WHIRLPOOL:${SCENARIO_ORCA_SO:-../whirlpools/target/deploy/whirlpool.so}" \
    "KEYPAIR_MANIFEST:${SCENARIO_MANIFEST_SO:-../manifest/target/deploy/manifest.so}"
}

program_args() { # → `checks`: one `--program <id>=<artifact>` per DEX
  local spec
  checks=()
  while IFS= read -r spec; do checks+=(--program "$(pubkey_of "${spec%%:*}")=${spec#*:}"); done < <(artifacts)
}

janitor() (
  # Upgrade the three DEX programs to the janitor (programs/janitor, built
  # with `just build-janitor`), close every account they own (rent to the
  # faucet), then put the forks back. The forks are restored on every exit
  # once the first upgrade has started (a failed `scenario janitor`, a
  # failed upgrade, Ctrl-C, a dropped terminal): every program whose janitor
  # upgrade was attempted is redeployed, each one even if another fails, and
  # all three are then checked; any program still not its fork fails the
  # run and is listed (`deploy` and `up` refuse it until
  # `SCENARIO_UPGRADE_PROGRAMS=1 scenario.sh deploy`). The restore's deploy
  # output goes to .scenario/logs/janitor-restore.log, so it survives a
  # closed terminal; run long teardowns under tmux or nohup all the same.
  so="${SCENARIO_JANITOR_SO:?set SCENARIO_JANITOR_SO to the janitor .so (just build-janitor)}"
  require_agave_cli
  umask 077
  dir="" upgraded=0 log="$state/logs/janitor-restore.log"
  attempted=() left=()
  say() { # to the log and, while there is one, the terminal
    echo "$*" >>"$log"
    echo "$*" 1>&2 2>/dev/null || true
  }
  restore() { # attempted programs → their forks, then all three checked; 1 if any is left
    local spec var artifact key try ok
    left=()
    while IFS= read -r spec; do
      var="${spec%%:*}" artifact="${spec#*:}"
      # Redeployed whatever the chain says now: a lagging RPC node can still
      # show the fork for a program whose janitor upgrade just landed.
      if [[ " ${attempted[*]} " == *" $var "* ]]; then
        upgrade "$var" "$artifact" || { left+=("$var"); continue; }
      fi
      key="$(pubkey_of "$var")" ok=0
      for try in 1 2 3; do
        if "$bin/scenario" check-programs --program "$key=$artifact" --lock "$programs_lock" \
          --janitor-so "$so" --require-artifact >/dev/null; then ok=1; break; fi
        [[ "$try" == 3 ]] || sleep "${SCENARIO_RESTORE_CHECK_S:-5}"
      done
      [[ "$ok" == 1 ]] || left+=("$var")
    done < <(artifacts)
    (( ${#left[@]} == 0 ))
  }
  finish() {
    local status=$?
    set +e # every step below runs whatever fails
    if [[ "$upgraded" == 1 ]]; then
      upgraded=0
      # A second Ctrl-C stops the deploy in flight (the CLI gets it too),
      # not the restore of the others.
      trap 'say "janitor: interrupted; still restoring the forks"' INT TERM HUP
      say "janitor: restoring the forks (deploy output: .scenario/logs/janitor-restore.log)"
      if restore >>"$log" 2>&1 </dev/null; then
        if [[ -f "$janitor_incomplete" ]]; then
          [[ "$status" != 0 ]] || status=1
          say "janitor: failed (status $status): the forks are restored, but the janitor run did" \
            "not finish (.scenario/janitor-incomplete): pools may have lost their vaults, and" \
            "swaps on them would fail. Re-run \`SCENARIO_JANITOR=1 $0 teardown $target\`;" \
            "\`up\` refuses until a janitor run finishes"
        elif [[ "$status" == 0 ]]; then
          say "janitor: forks restored with no accounts or vaults; bootstrap re-creates them"
        else
          say "janitor: failed (status $status) before closing anything; the forks are restored"
        fi
      else
        [[ "$status" != 0 ]] || status=1
        say "janitor: NOT restored, still running the janitor (or other code): ${left[*]};" \
          "restore with \`SCENARIO_UPGRADE_PROGRAMS=1 $0 deploy $target\`"
        if [[ -f "$janitor_incomplete" ]]; then
          say "janitor: the janitor run did not finish either (.scenario/janitor-incomplete):" \
            "re-run the janitor teardown once the forks are restored"
        fi
      fi
    fi
    if [[ -n "$dir" ]]; then rm -rf "$dir"; fi
    exit "$status"
  }
  trap finish EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap 'exit 129' HUP
  if [[ -d /dev/shm && -w /dev/shm ]]; then dir="$(mktemp -d /dev/shm/kp.XXXXXX)"; else dir="$(mktemp -d)"; fi
  printf %s "${KEYPAIR_PROGRAM_AUTHORITY:?missing secret}" >"$dir/authority.json"
  printf %s "${KEYPAIR_FAUCET:?missing secret}" >"$dir/faucet.json"
  flags=()
  [[ "$target" == local ]] && flags+=(--use-rpc)
  upgrade() { # keypair var, artifact
    printf %s "${!1:?missing secret: $1}" >"$dir/program.json"
    solana program deploy -u "$rpc" "${flags[@]}" --program-id "$dir/program.json" \
      --upgrade-authority "$dir/authority.json" --fee-payer "$dir/faucet.json" "$2" >&2 </dev/null
  }
  live=0
  while IFS= read -r spec; do
    key="$(pubkey_of "${spec%%:*}")"
    if program_exists "$key" && ! program_closed "$key"; then live=$((live + 1)); fi
  done < <(artifacts)
  if [[ "$live" == 0 ]]; then echo "janitor: the DEX programs are already closed" >&2; exit 0; fi
  [[ "$live" == 3 ]] || { echo "janitor: only $live of 3 DEX programs exist; refusing a partial run" >&2; exit 1; }
  # Read-only, before any upgrade: what the run will close.
  "$bin/scenario" janitor-plan >"$state/janitor-plan.json"
  other="$(jq -s '[.[] | .vaults.ofOtherMints // 0] | add // 0' "$state/janitor-plan.json")"
  if [[ "$other" != 0 ]]; then
    echo "janitor: $other vaults on the DEX programs hold non-scenario mints (another operator's" \
      "pools?); they are closed too (.scenario/janitor-plan.json)" >&2
  fi
  upgraded=1 # from here on, `finish` restores the forks
  while IFS= read -r spec; do
    attempted+=("${spec%%:*}") # before the attempt: a failed deploy may still have landed
    upgrade "${spec%%:*}" "$so"
  done < <(artifacts)
  date -u +%Y-%m-%dT%H:%M:%SZ >"$janitor_incomplete"
  "$bin/scenario" janitor --janitor-so "$so" | tee "$state/janitor.json"
  remaining="$(jq -s '[.[] | .programs[]? | .remaining // 0] | add // 0' "$state/janitor.json")"
  if [[ "$remaining" != 0 ]]; then
    echo "janitor: $remaining accounts are still owned by the DEX programs (.scenario/janitor.json)" >&2
    exit 1
  fi
  rm -f "$janitor_incomplete"
)

program_closed() { # pubkey → 0 if it is an upgradeable program that was closed
  # A closed program keeps its account (so program_exists says yes) but can
  # never be deployed to or upgraded again.
  # Captured, not piped into `grep -q`: under pipefail its early exit would
  # SIGPIPE `solana` and fail the check even on a match.
  local out
  out="$(solana program show -u "$rpc" "$1" 2>&1 </dev/null || true)"
  [[ "$out" == *"has been closed"* ]]
}

deploy() (
  # Deploys each fork artifact at its program keypair, skipping programs that
  # already run it. A deployed program running other code (the janitor after
  # a failed teardown, an older build) is refused, or upgraded to the
  # artifact with SCENARIO_UPGRADE_PROGRAMS=1. Paths default to sibling fork
  # checkouts.
  # Manifest is SBPF v3: use an Agave >= 4.x `solana` CLI.
  # A subshell, so umask and traps stay local; the key directory is removed
  # on any exit, Ctrl-C and SIGTERM included.
  require_agave_cli
  cargo build -q $build_flag -p scenario # check-programs
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
  while IFS= read -r spec; do
    var="${spec%%:*}" so="${spec#*:}"
    key="$(pubkey_of "$var")"
    if program_exists "$key"; then
      if program_closed "$key"; then
        echo "$var was closed; closed program IDs cannot be reused: use a new keypair" >&2; exit 1
      fi
      if "$bin/scenario" check-programs --program "$key=$so" "${program_check[@]}" >/dev/null; then
        echo "$var already deployed"; continue
      fi
      if [[ "${SCENARIO_UPGRADE_PROGRAMS:-0}" != 1 ]]; then
        echo "$var is deployed but does not run its expected code (above; expected: its entry in" \
          "$programs_lock, else $so); refusing. SCENARIO_UPGRADE_PROGRAMS=1 upgrades it to the" \
          "artifact (on a shared target, only if that is the intended release)" >&2
        exit 1
      fi
      echo "$var: upgrading to $so"
    fi
    printf %s "${!var:?missing secret: $var}" >"$dir/program.json"
    solana program deploy -u "$rpc" "${flags[@]}" --program-id "$dir/program.json" \
      --upgrade-authority "$dir/authority.json" --fee-payer "$dir/faucet.json" "$so" </dev/null
  done < <(artifacts)
  # Every program now runs its expected code: record the hashes, so `up`
  # verifies without the artifacts (commit the lock for a shared target).
  program_args
  "$bin/scenario" check-programs "${checks[@]}" "${janitor_check[@]}" --lock "$programs_lock" \
    --record >/dev/null
  echo "recorded the DEX program hashes in $programs_lock"
  if [[ -f "$janitor_incomplete" ]]; then
    echo "deploy: the last janitor run did not finish (.scenario/janitor-incomplete): pools on" \
      "these programs may have lost their vaults; re-run \`SCENARIO_JANITOR=1 $0 teardown $target\`" >&2
  fi
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
    --web.listen-address=127.0.0.1:9190 --storage.tsdb.path="$state/prometheus" \
    --storage.tsdb.retention.time=90d
  if command -v node_exporter >/dev/null; then
    start node_exporter node_exporter --web.listen-address=127.0.0.1:9186
  fi
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
  watch)
    cargo build -q $build_flag -p chain-watch
    watch
    ;;
  deploy)
    deploy
    ;;
  up)
    build
    if [[ -f "$janitor_incomplete" ]]; then
      echo "up: the last janitor run did not finish (.scenario/janitor-incomplete, $(cat "$janitor_incomplete")):" \
        "pools on the DEX programs may have lost their vaults, and bootstrap leaves existing pools" \
        "alone. Re-run \`SCENARIO_JANITOR=1 $0 teardown $target\`. Nothing was started" >&2
      exit 1
    fi
    # The forks, not the janitor a failed teardown left (nor an old build),
    # compared by code hash.
    if [[ "${SCENARIO_SKIP_PROGRAM_CHECK:-0}" == 1 ]]; then
      echo "up: SCENARIO_SKIP_PROGRAM_CHECK=1: the DEX programs' code is not checked" >&2
    else
      program_args
      "$bin/scenario" check-programs "${checks[@]}" "${program_check[@]}" >/dev/null || {
        echo "up: a DEX program does not run its expected code (above, with the sha256s). Left as" \
          "the janitor or another build: \`SCENARIO_UPGRADE_PROGRAMS=1 $0 deploy $target\` restores" \
          "the fork artifact. Right code, but this host has no artifact or another build of it: the" \
          "expected hashes come from $programs_lock (\`deploy\` records them; commit it for a shared" \
          "target), or SCENARIO_SKIP_PROGRAM_CHECK=1 skips the check. Nothing was started" >&2
        exit 1
      }
    fi
    watch
    tokens
    # Supervised: the oracle exits when any of its tasks dies, and comes
    # back (restarts are logged in .scenario/logs/oracle.log). Its synthetic
    # generator state survives the restart.
    (export MTM_ORACLE__STATE_PATH="$state/state/oracle-pricing.json"
      start oracle "$root/bootstrap/scenario/scenario.sh" supervise oracle "$bin/oracle")
    # Pools are created at live oracle prices: wait until every instrument
    # has a fresh price (every asset in the profile), not just a listening
    # socket.
    wait_http http://127.0.0.1:8180/ready oracle
    "$bin/scenario" bootstrap | tee "$state/bootstrap.json"
    (router_env; start router "$bin/router")
    wait_http http://127.0.0.1:8181/health router
    start mm "$bin/mm" --state-dir "$state/state"
    # Give makers a few ticks to put liquidity on every venue.
    sleep "${SCENARIO_WARMUP_S:-20}"
    start flow "$bin/flow"
    start arb "$bin/arb"
    echo "scenario up — oracle :8180, router :8181, chain-watch :8183; metrics :9180-9185"
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
    # Let in-flight transactions land before reading. A local validator
    # settles in a slot or two; a remote RPC node keeps rebroadcasting until
    # the blockhash expires, 150 blocks on: wait for those blocks (however
    # long a slow cluster takes, up to SCENARIO_SETTLE_TIMEOUT_S, after which
    # the snapshot is taken and marked timedOut), with SCENARIO_SETTLE_S of
    # wall-clock time as the fallback when the height is never readable.
    remote=$([[ "$target" == local ]] && echo false || echo true)
    settle_s="${SCENARIO_SETTLE_S:-$($remote && echo 90 || echo 3)}"
    settle_blocks="${SCENARIO_SETTLE_BLOCKS:-$($remote && echo 150 || echo 0)}"
    settle_timeout_s="${SCENARIO_SETTLE_TIMEOUT_S:-300}"
    frozen_snapshot() {
      signal STOP
      "$bin/scenario" snapshot --out "$1" --settle-blocks "$settle_blocks" \
        --settle-s "$settle_s" --settle-timeout-s "$settle_timeout_s" >&2 || { signal CONT; return 1; }
    }
    trap 'signal CONT' EXIT
    frozen_snapshot "$runs/$name-t0.json"
    signal CONT
    python3 -I "$root/bootstrap/scenario/sample.py" --seconds "$seconds" --rpc "$rpc" \
      --out "$runs/$name-load.json" >/dev/null
    frozen_snapshot "$runs/$name-t1.json"
    # Still frozen through the report: its block scan and Prometheus reads
    # compete with flow's connections at high scale (x2000 ran out of ports).
    "$bin/scenario" report --start "$runs/$name-t0.json" --end "$runs/$name-t1.json" \
      --prometheus "${SCENARIO_PROMETHEUS:-http://127.0.0.1:9190}" >"$runs/$name-report.json"
    signal CONT
    echo "wrote $runs/$name-{t0,t1,load,report}.json"
    ;;
  teardown)
    # Everything the unwind needs, checked before anything is stopped: a
    # missing secret must not leave the run half torn down.
    if [[ -n "${SCENARIO_CLOSE_PROGRAMS:-}" ]]; then
      echo "teardown: SCENARIO_CLOSE_PROGRAMS is gone: the DEX programs stay deployed (a closed" \
        "program ID can never be reused); unset it. Nothing was stopped" >&2
      exit 1
    fi
    missing=()
    [[ -n "${KEYPAIR_FAUCET:-}" ]] || missing+=(KEYPAIR_FAUCET)
    if [[ "${SCENARIO_JANITOR:-0}" == 1 ]]; then
      [[ -n "${KEYPAIR_PROGRAM_AUTHORITY:-}" ]] || missing+=(KEYPAIR_PROGRAM_AUTHORITY)
    fi
    if [[ "${SCENARIO_JANITOR:-0}" == 1 && ! -f "${SCENARIO_JANITOR_SO:-}" ]]; then
      missing+=("SCENARIO_JANITOR_SO (the janitor .so: just build-janitor)")
    fi
    if (( ${#missing[@]} )); then
      echo "teardown: missing ${missing[*]}; nothing was stopped" >&2; exit 1
    fi
    if [[ "${SCENARIO_JANITOR:-0}" == 1 ]]; then
      # The janitor's restore deploys the fork artifacts: each must be here,
      # readable, and exactly the code deployed now, or the programs would
      # be left as the janitor (or come back as another build).
      program_args
      "$bin/scenario" check-programs "${checks[@]}" --lock "$programs_lock" \
        --janitor-so "$SCENARIO_JANITOR_SO" --require-artifact >/dev/null || {
        echo "teardown: the janitor could not restore the forks (above): every fork artifact" \
          "(SCENARIO_{RAYDIUM,ORCA,MANIFEST}_SO) must be the deployed code; nothing was stopped" >&2
        exit 1
      }
    elif [[ -f "$janitor_incomplete" ]]; then
      echo "teardown: note: the last janitor run did not finish (.scenario/janitor-incomplete);" \
        "SCENARIO_JANITOR=1 re-runs it" >&2
    fi
    # Seed, program ids and mints, resolved exactly as the unwind will.
    "$bin/scenario" check >/dev/null || { echo "teardown: scenario check failed; nothing was stopped" >&2; exit 1; }
    # Wait for every participant to exit: the unwind must not race a maker
    # that is still re-centring.
    for name in arb flow mm router oracle; do
      if [[ -f "$state/pids/$name" ]]; then stop "$name"; fi
    done
    "$bin/scenario" teardown | tee "$state/teardown.json"
    # After the unwind (which needs the real DEX code), before any close.
    if [[ "${SCENARIO_JANITOR:-0}" == 1 ]]; then janitor; fi
    sweep_operators="${SCENARIO_SWEEP_OPERATORS:-$([[ "$target" == local ]] && echo 1 || echo 0)}"
    [[ "$sweep_operators" == 1 ]] && "$bin/scenario" teardown --operators >"$state/teardown-operators.json"
    rm -rf "$state/state" # mm position state files
    "$bin/scenario" leftovers | tee "$state/leftovers.json"
    ;;
  down)
    # Takers first, so nothing trades against makers that are unwinding.
    # chain-watch after the participants: it records their last transactions.
    for name in arb flow mm router oracle chain-watch grafana prometheus node_exporter; do
      if [[ -f "$state/pids/$name" ]]; then stop "$name"; fi
    done
    compose="${MTM_COMPOSE:-docker compose}"
    if $compose version >/dev/null 2>&1 && ${compose%% *} ps >/dev/null 2>&1 \
      && [[ -n "$($compose -f ops/compose.scenario.yaml ps -q 2>/dev/null)" ]]; then
      $compose -f ops/compose.scenario.yaml down # volumes are kept
    fi
    ;;
  *) echo "unknown command: $cmd (deploy|up|bootstrap|observe|watch|status|snapshot|report|measure|teardown|down)" >&2; exit 2 ;;
esac
