// Command and control (P1): run scenario.sh jobs against this control's
// target, follow their output, see the processes they started and tail
// their logs. Destructive commands need the command name typed again.

import { useEffect, useMemo, useState } from "react";

import {
  CONFIRM,
  COMMANDS,
  cancelJob,
  getJson,
  getText,
  startJob,
  type Command,
  type Job,
  type JobState,
  type Status,
} from "../api";
import { dateTime, seconds } from "../format";
import { Compose } from "./Compose";
import { usePoll } from "../hooks";
import type { LiveState } from "../live";
import { StatusLabel } from "./Tooltip";

const OPERANDS: Partial<Record<Command, string[]>> = {
  snapshot: ["snapshot name"],
  report: ["start snapshot", "end snapshot"],
  measure: ["run name", "seconds"],
};

const HELP: Record<Command, string> = {
  up: "build, start chain-watch, bootstrap, then oracle, router, makers, flow and arb",
  down: "stop every service (TERM, then KILL after 30 s)",
  status: "pools, wallets and processes",
  deploy: "deploy the DEX forks that are not deployed yet",
  bootstrap: "tokens, pools and wallet top-ups only",
  observe: "Prometheus :9190 and Grafana :3100",
  watch: "chain-watch alone",
  snapshot:
    "participant holdings at oracle prices (.scenario/snapshots/<name>.json)",
  report: "PnL, transactions and trades between two snapshots",
  measure: "snapshot → sample load for N seconds → snapshot → report",
  teardown: "stop participants, unwind every position, sweep SOL to the faucet",
};

function stateLabel(state: JobState) {
  const color = {
    running: "var(--series-1)",
    succeeded: "var(--good)",
    failed: "var(--critical)",
    cancelled: "var(--text-muted)",
  }[state];
  return <StatusLabel color={color} label={state} />;
}

export function Control({ live }: { live: LiveState }) {
  const status = live.status;
  return (
    <>
      <Compose status={status} />
      <div className="grid2">
        <JobForm status={status} />
        <Processes status={status} />
      </div>
      <Jobs live={live} />
    </>
  );
}

function JobForm({ status }: { status: Status | null }) {
  const [command, setCommand] = useState<Command>("status");
  const [operands, setOperands] = useState<string[]>([]);
  const [scale, setScale] = useState("");
  const [runId, setRunId] = useState("");
  const [janitor, setJanitor] = useState(false);
  const [closePrograms, setClosePrograms] = useState(false);
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const fields = OPERANDS[command] ?? [];
  const needsConfirm = CONFIRM.has(command);
  const running = status?.job ?? null;

  useEffect(() => {
    setOperands(fields.map(() => ""));
    setConfirm("");
    setError(null);
  }, [command]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await startJob({
        command,
        operands,
        scale: scale || null,
        run_id:
          (command === "up" || command === "watch") && runId ? runId : null,
        janitor: command === "teardown" && janitor,
        close_programs: command === "teardown" && closePrograms,
        confirm: needsConfirm ? confirm : null,
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <div className="card-head">
        <h2>Run a command</h2>
        <span className="sub">
          scenario.sh … <b>{status?.target ?? "?"}</b> (fixed by the Doppler
          config control runs under)
        </span>
      </div>
      <div className="form" style={{ marginTop: 8 }}>
        <label>
          command
          <select
            value={command}
            onChange={(e) => setCommand(e.target.value as Command)}
          >
            {COMMANDS.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        </label>
        {fields.map((name, i) => (
          <label key={name}>
            {name}
            <input
              value={operands[i] ?? ""}
              onChange={(e) =>
                setOperands(
                  operands.map((v, j) => (j === i ? e.target.value : v)),
                )
              }
              inputMode={name === "seconds" ? "numeric" : "text"}
              style={{ width: 140 }}
            />
          </label>
        ))}
        <label>
          scale
          <select value={scale} onChange={(e) => setScale(e.target.value)}>
            <option value="">
              {/* Unset: scenario.sh applies the scale `up` recorded. */}
              {command !== "up" && status?.active_scale
                ? `running scenario's (${status.active_scale})`
                : "base"}
            </option>
            {(status?.scales ?? []).map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
        {(command === "up" || command === "watch") && (
          <label>
            run id (optional)
            <input
              value={runId}
              onChange={(e) => setRunId(e.target.value)}
              placeholder="target-UTC time"
              style={{ width: 160 }}
            />
          </label>
        )}
        {command === "teardown" && (
          <>
            <label className="check">
              <input
                type="checkbox"
                checked={janitor}
                onChange={(e) => setJanitor(e.target.checked)}
              />
              janitor (close every DEX account)
            </label>
            <label className="check">
              <input
                type="checkbox"
                checked={closePrograms}
                onChange={(e) => setClosePrograms(e.target.checked)}
              />
              close the DEX programs
            </label>
          </>
        )}
        {needsConfirm && (
          <label>
            type “{command}” to confirm
            <input
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              style={{ width: 120 }}
            />
          </label>
        )}
        <button
          className={needsConfirm ? "danger" : ""}
          disabled={
            busy || running != null || (needsConfirm && confirm !== command)
          }
          onClick={submit}
        >
          Run
        </button>
      </div>
      <p className="hint">
        {HELP[command]}
        {status?.active_scale != null &&
          ` · the running scenario was started at ${status.active_scale || "base"} scale; other commands use it unless you pick one (a different one is refused)`}
      </p>
      {running && (
        <p className="hint">
          Job #{running.id} ({running.command}) is running; one job at a time.{" "}
          <button
            onClick={() =>
              cancelJob(running.id).catch((e: unknown) => setError(String(e)))
            }
          >
            Cancel
          </button>{" "}
          (cancelling <code>up</code> also stops the services it started)
        </p>
      )}
      {error && <div className="error">{error}</div>}
    </section>
  );
}

function Processes({ status }: { status: Status | null }) {
  const [open, setOpen] = useState<string | null>(null);
  const log = usePoll(
    () => (open ? getText(`/api/logs/${open}?lines=300`) : Promise.resolve("")),
    3_000,
    [open],
  );
  return (
    <section className="card">
      <div className="card-head">
        <h2>Processes</h2>
        <span className="sub">
          started by scenario.sh (.scenario/pids); click for the log
        </span>
      </div>
      <table>
        <thead>
          <tr>
            <th>service</th>
            <th>state</th>
            <th className="num">pid</th>
          </tr>
        </thead>
        <tbody>
          {(status?.processes ?? []).map((p) => (
            <tr
              key={p.name}
              className={`clickable${open === p.name ? " selected" : ""}`}
              onClick={() => setOpen(open === p.name ? null : p.name)}
            >
              <td>{p.name}</td>
              <td>
                {p.running ? (
                  <StatusLabel color="var(--good)" label="running" />
                ) : (
                  <StatusLabel
                    color="var(--text-muted)"
                    label="stopped"
                    hollow
                  />
                )}
              </td>
              <td className="num">{p.pid ?? "–"}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {open && (
        <pre className="console">
          {log.error ? `no log: ${log.error}` : log.data || "…"}
        </pre>
      )}
    </section>
  );
}

function Jobs({ live }: { live: LiveState }) {
  const history = usePoll(() => getJson<Job[]>("/api/jobs"), 10_000, []);
  const jobs = useMemo(() => {
    const byId = new Map<number, Job>();
    for (const job of history.data ?? []) byId.set(job.id, job);
    for (const job of live.jobs.values()) byId.set(job.id, job);
    return [...byId.values()].sort((a, b) => b.id - a.id);
  }, [history.data, live.jobs]);
  const [selected, setSelected] = useState<number | null>(null);
  const shown = selected ?? jobs[0]?.id ?? null;
  const liveLines = shown != null ? live.output.get(shown) : undefined;
  const stored = usePoll(
    () =>
      shown != null && !liveLines
        ? getText(`/api/jobs/${shown}/output`)
        : Promise.resolve(""),
    5_000,
    [shown, liveLines == null],
  );
  const output = liveLines ? liveLines.join("\n") : stored.data;

  return (
    <section className="card">
      <div className="card-head">
        <h2>Jobs</h2>
        <span className="sub">
          newest first; output streams live while a job runs
        </span>
      </div>
      <div
        className="grid2"
        style={{ gridTemplateColumns: "minmax(360px, 1fr) 2fr" }}
      >
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th className="num">#</th>
                <th>command</th>
                <th>state</th>
                <th>started</th>
                <th className="num">took</th>
              </tr>
            </thead>
            <tbody>
              {jobs.map((job) => (
                <tr
                  key={job.id}
                  className={`clickable${shown === job.id ? " selected" : ""}`}
                  onClick={() => setSelected(job.id)}
                >
                  <td className="num">{job.id}</td>
                  <td className="mono">
                    {job.args.join(" ")}
                    {job.env.length > 0 && (
                      <div className="hint">
                        {job.env.map(([k, v]) => `${k}=${v}`).join(" ")}
                      </div>
                    )}
                  </td>
                  <td>{stateLabel(job.state)}</td>
                  <td>{dateTime(job.started_us)}</td>
                  <td className="num">
                    {job.ended_us
                      ? seconds((job.ended_us - job.started_us) / 1e6)
                      : "…"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <pre className="console" aria-live="polite">
          {output || (shown == null ? "no jobs yet" : "no output")}
        </pre>
      </div>
    </section>
  );
}
