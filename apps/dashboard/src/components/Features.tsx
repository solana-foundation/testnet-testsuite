// Every feature gate on the cluster, pending first, then newest activation.
// A table: hundreds of classes that all carry meaning.

import { useMemo, useState } from "react";

import { getJson, type Feature } from "../api";
import { dateTime } from "../format";
import { usePoll } from "../hooks";
import { StatusLabel } from "./Tooltip";

export function Features() {
  const { data, error } = usePoll(
    () => getJson<Feature[]>("/api/chain/features"),
    60_000,
    [],
  );
  const [search, setSearch] = useState("");
  const [onlyRecent, setOnlyRecent] = useState(false);
  const rows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const recent = Date.now() * 1000 - 30 * 86_400 * 1e6;
    return (data ?? [])
      .filter(
        (f) =>
          !needle ||
          f.id.toLowerCase().includes(needle) ||
          (f.description ?? "").toLowerCase().includes(needle),
      )
      .filter(
        (f) =>
          !onlyRecent ||
          f.activated_slot == null ||
          (f.activated_time_us ?? 0) >= recent,
      )
      .sort((a, b) => {
        if ((a.activated_slot == null) !== (b.activated_slot == null))
          return a.activated_slot == null ? -1 : 1;
        return (b.activated_slot ?? 0) - (a.activated_slot ?? 0);
      });
  }, [data, search, onlyRecent]);
  const pending = (data ?? []).filter((f) => f.activated_slot == null).length;

  return (
    <>
      <div className="filters">
        <input
          placeholder="Search id or description"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          style={{ width: 320 }}
        />
        <label
          className="hint"
          style={{ display: "flex", gap: 6, alignItems: "center" }}
        >
          <input
            type="checkbox"
            checked={onlyRecent}
            onChange={(e) => setOnlyRecent(e.target.checked)}
          />
          pending or activated in the last 30 days
        </label>
      </div>
      <section className="card">
        <div className="card-head">
          <h2>Feature gates</h2>
          <span className="sub">
            {data
              ? `${data.length} on chain · ${pending} pending (activate at the next epoch boundary)`
              : error
                ? `chain-watch: ${error}`
                : "loading…"}
          </span>
        </div>
        <div className="scroll" style={{ maxHeight: "70vh" }}>
          <table>
            <thead>
              <tr>
                <th>state</th>
                <th>description</th>
                <th>id</th>
                <th className="num">activation slot</th>
                <th>activated</th>
                <th>seen</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((f) => (
                <tr key={f.id}>
                  <td>
                    {f.activated_slot == null ? (
                      <StatusLabel
                        color="var(--warning)"
                        label="pending"
                        hollow
                      />
                    ) : (
                      <StatusLabel color="var(--good)" label="active" />
                    )}
                  </td>
                  <td>
                    {f.description ?? (
                      <span className="hint">
                        unknown to this Agave version
                      </span>
                    )}
                  </td>
                  <td className="mono">{f.id}</td>
                  <td className="num">
                    {f.activated_slot?.toLocaleString() ?? "–"}
                  </td>
                  <td>
                    {f.activated_time_us != null
                      ? `${f.time_estimated ? "≈ " : ""}${dateTime(f.activated_time_us)}`
                      : "–"}
                  </td>
                  <td className="hint">
                    {f.first_seen_pending_us != null
                      ? `pending since ${dateTime(f.first_seen_pending_us)}`
                      : ""}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="hint">
          ≈ estimated time: the RPC no longer serves that block, so it is
          extrapolated from the slot pace.
        </p>
      </section>
    </>
  );
}
