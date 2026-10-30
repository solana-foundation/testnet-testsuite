// Drill-down for one slot: its record and our transactions in it.

import { useEffect, useState } from "react";

import { getJson, type OurTx, type SlotRecord } from "../api";
import { compact, short } from "../format";
import { slotState } from "./SlotStrip";
import { KeyValues, StatusLabel } from "./Tooltip";

export function explorer(
  kind: "tx" | "block" | "address",
  id: string | number,
  target: string,
): string {
  const cluster =
    target === "testnet" || target === "devnet"
      ? `?cluster=${target}`
      : `?cluster=custom&customUrl=${encodeURIComponent("http://127.0.0.1:8899")}`;
  return `https://explorer.solana.com/${kind}/${id}${cluster}`;
}

export function SlotDetail({
  slot,
  target,
  onClose,
}: {
  slot: SlotRecord;
  target: string;
  onClose: () => void;
}) {
  const [txs, setTxs] = useState<OurTx[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setTxs(null);
    getJson<OurTx[]>(`/api/chain/txs?slot=${slot.slot}`).then(
      setTxs,
      (e: unknown) => setError(String(e)),
    );
  }, [slot.slot]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [onClose]);

  return (
    <aside className="drawer" aria-label={`Slot ${slot.slot}`}>
      <header>
        <h2>Slot {slot.slot.toLocaleString()}</h2>
        <a
          href={explorer("block", slot.slot, target)}
          target="_blank"
          rel="noreferrer"
        >
          explorer
        </a>
        <span className="spacer" style={{ flex: 1 }} />
        <button onClick={onClose}>Close</button>
      </header>
      <KeyValues
        rows={[
          ["status", slotState(slot)],
          ["leader", slot.leader ?? "–"],
          [
            "client / version",
            `${slot.leader_client ?? "–"} ${slot.leader_version ?? ""}`,
          ],
          ["block height", slot.block_height?.toLocaleString() ?? "–"],
          [
            "transactions",
            `${slot.tx_total} (${slot.tx_vote} vote, ${slot.tx_failed} failed)`,
          ],
          ["ours", `${slot.tx_ours} (${slot.tx_ours_failed} failed)`],
          ["CU consumed", compact(slot.cu_consumed)],
          ["cost units", compact(slot.cost_units)],
          ["fees", `${compact(slot.fees_lamports)} lamports`],
          [
            "priority p50 / p90 / max",
            `${compact(slot.prio_p50)} / ${compact(slot.prio_p90)} / ${compact(slot.prio_max)}`,
          ],
        ]}
      />
      <h3 style={{ fontSize: 14, marginTop: 18 }}>Our transactions</h3>
      {error && <div className="error">{error}</div>}
      {txs == null && !error && <div className="hint">loading…</div>}
      {txs?.length === 0 && <div className="hint">none in this slot</div>}
      {txs && txs.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>result</th>
              <th>role</th>
              <th>programs</th>
              <th className="num">CU / limit</th>
              <th className="num">prio</th>
              <th>signature</th>
            </tr>
          </thead>
          <tbody>
            {txs.map((tx) => (
              <tr key={tx.signature}>
                <td>
                  {tx.ok ? (
                    <StatusLabel color="var(--good)" label="ok" />
                  ) : (
                    <>
                      <StatusLabel color="var(--critical)" label="failed" />
                      <div className="mono hint">
                        {tx.error_program ? `${tx.error_program}: ` : ""}
                        {tx.error}
                      </div>
                    </>
                  )}
                </td>
                <td>{tx.role}</td>
                <td>{tx.programs}</td>
                <td className="num">
                  {compact(tx.cu_consumed)} / {compact(tx.cu_limit)}
                </td>
                <td className="num">{compact(tx.prio_price)}</td>
                <td className="mono">
                  <a
                    href={explorer("tx", tx.signature, target)}
                    target="_blank"
                    rel="noreferrer"
                  >
                    {short(tx.signature, 6)}
                  </a>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </aside>
  );
}
