"""Sample machine, service, and Surfpool load while a scenario runs.

    python3 bootstrap/scenario/sample.py --seconds 600 --out .scenario/runs/x10-load.json

Every `--interval` seconds it records, per scenario process (oracle, router,
mm, flow, arb, surfpool or solana-test-validator, prometheus, grafana): CPU (% of one core, from
/proc/<pid>/stat deltas) and RSS; machine CPU and available memory; and the
validator: landed transactions (getTransactionCount delta → TPS), slot
rate, and RPC latency (getLatestBlockhash round trip). A real validator
(solana-test-validator) also lands one consensus vote per slot, which
Surfpool does not: TPS excludes them (getTransactionCount counts them).
Under Alpenglow (e.g. a test validator with testnet's cloned feature set)
votes are not transactions, so nothing is subtracted.
The output keeps
every sample plus a summary (mean / p95 / max).

TPS is getTransactionCount over time: against a local validator that is the
scenario's own traffic; against a shared cluster (testnet) it counts every
user's transactions and votes, so the summary labels it `tpsScope` and the
participant-level counts in the report are the ones to use.
"""
import argparse, base64, json, os, statistics, time, urllib.parse, urllib.request

SERVICES = ["oracle", "router", "mm", "flow", "arb", "prometheus", "grafana"]
VALIDATORS = {"surfpool", "solana-test-validator"}
TICK = os.sysconf("SC_CLK_TCK")
ALPENGLOW = "A1pengvuM6JEcyNuTnMqepBKhwHE3N6PmUrdATGawhJS"
PID_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), ".scenario", "pids")


def rpc(url, method, params=None):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode()
    request = urllib.request.Request(url, body, {"content-type": "application/json"})
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)["result"]
    return result, (time.monotonic() - started) * 1000


def processes():
    """name -> pids: scenario services from scenario.sh pid files (so other
    stacks' processes with the same names are ignored), plus the validator."""
    out = {}
    for name in SERVICES:
        try:
            pid = open(os.path.join(PID_DIR, name)).read().strip()
        except OSError:
            continue
        if os.path.exists(f"/proc/{pid}"):
            out[name] = [pid]
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            argv0 = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")[0]
        except OSError:
            continue
        name = os.path.basename(argv0.decode(errors="replace"))
        if name in VALIDATORS:
            out.setdefault(name, []).append(pid)
    return out


def proc_times(pid):
    try:
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        rss_pages = int(open(f"/proc/{pid}/statm").read().split()[1])
    except OSError:
        return None
    return int(fields[11]) + int(fields[12]), rss_pages * os.sysconf("SC_PAGE_SIZE")


def machine():
    cpu = [int(v) for v in open("/proc/stat").readline().split()[1:]]
    with open("/proc/meminfo") as meminfo:
        mem = {line.split(":")[0]: int(line.split()[1]) for line in meminfo}
    return sum(cpu), cpu[3] + cpu[4], mem["MemAvailable"] * 1024


def summarize(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    ordered = sorted(values)
    return {
        "mean": round(statistics.fmean(values), 2),
        "p95": round(ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))], 2),
        "max": round(ordered[-1], 2),
    }


def feature_active(url, feature):
    """A feature account exists once activation is scheduled; byte 0 is the Some tag of activated_at."""
    try:
        value = rpc(url, "getAccountInfo", [feature, {"encoding": "base64"}])[0]["value"]
    except Exception:  # noqa: BLE001 - unknown means assume legacy voting
        return False
    return bool(value) and base64.b64decode(value["data"][0])[:1] == b"\x01"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=float, default=600)
    parser.add_argument("--interval", type=float, default=5)
    parser.add_argument("--rpc", default="http://127.0.0.1:8899")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    tower_votes = not feature_active(args.rpc, ALPENGLOW)
    samples = []
    previous = None
    deadline = time.monotonic() + args.seconds
    while True:
        now = time.monotonic()
        pids = processes()
        per_proc = {name: [proc_times(pid) for pid in ps] for name, ps in pids.items()}
        total, idle, available = machine()
        try:
            tx_count, _ = rpc(args.rpc, "getTransactionCount")
            slot, _ = rpc(args.rpc, "getSlot")
            _, latency = rpc(args.rpc, "getLatestBlockhash")
        except Exception as error:  # noqa: BLE001 - a stalled validator is a data point
            tx_count = slot = latency = None
            print("rpc sample failed:", error)
        votes_per_slot = 1 if tower_votes and "solana-test-validator" in pids else 0
        current = {"t": now, "proc": per_proc, "total": total, "idle": idle, "tx": tx_count, "slot": slot}
        if previous:
            dt = now - previous["t"]
            sample = {
                "time": round(time.time(), 1),
                "machineCpuPct": round(100 * (1 - (idle - previous["idle"]) / max(1, total - previous["total"])), 1),
                "memAvailableGiB": round(available / 2**30, 2),
                "rpcLatencyMs": round(latency, 1) if latency is not None else None,
                "tps": round((tx_count - previous["tx"] - votes_per_slot * (slot - previous["slot"])) / dt, 2)
                if tx_count is not None and previous["tx"] is not None else None,
                "slotsPerSec": round((slot - previous["slot"]) / dt, 2) if slot is not None and previous["slot"] is not None else None,
                "services": {},
            }
            for name, stats in per_proc.items():
                before = [s for s in previous["proc"].get(name, []) if s]
                after = [s for s in stats if s]
                if not after:
                    continue
                cpu = (sum(s[0] for s in after) - sum(s[0] for s in before)) / TICK / dt * 100 if before else None
                sample["services"][name] = {
                    "cpuPct": round(cpu, 1) if cpu is not None and cpu >= 0 else None,
                    "rssMiB": round(sum(s[1] for s in after) / 2**20, 1),
                }
            samples.append(sample)
        previous = current
        if now >= deadline:
            break
        time.sleep(max(0.0, args.interval - (time.monotonic() - now)))

    names = sorted({name for s in samples for name in s["services"]})
    host = urllib.parse.urlparse(args.rpc).hostname or ""
    local = host in ("127.0.0.1", "localhost", "::1")
    summary = {
        "samples": len(samples),
        "cpus": os.cpu_count(),
        "tpsScope": "local validator" if local else "network-wide (shared cluster: all users and votes)",
        "machineCpuPct": summarize([s["machineCpuPct"] for s in samples]),
        "memAvailableGiB": summarize([s["memAvailableGiB"] for s in samples]),
        "tps": summarize([s["tps"] for s in samples]),
        "slotsPerSec": summarize([s["slotsPerSec"] for s in samples]),
        "rpcLatencyMs": summarize([s["rpcLatencyMs"] for s in samples]),
        "services": {
            name: {
                "cpuPct": summarize([s["services"].get(name, {}).get("cpuPct") for s in samples]),
                "rssMiB": summarize([s["services"].get(name, {}).get("rssMiB") for s in samples]),
            }
            for name in names
        },
    }
    with open(args.out, "w") as out:
        json.dump({"summary": summary, "samples": samples}, out, indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
