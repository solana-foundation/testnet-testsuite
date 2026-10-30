import { useState } from "react";

import type { SlotRecord } from "./api";
import { Control } from "./components/Control";
import { Features } from "./components/Features";
import { Overview } from "./components/Overview";
import { SlotDetail } from "./components/SlotDetail";
import { StatusLabel } from "./components/Tooltip";
import { compact } from "./format";
import { useLive } from "./live";

type Tab = "overview" | "features" | "control";

export function App() {
  const live = useLive();
  const [tab, setTab] = useState<Tab>("overview");
  const [slot, setSlot] = useState<SlotRecord | null>(null);
  const status = live.status;
  const chain = status?.chain ?? null;
  const epoch = live.events.filter((e) => e.event.kind === "epoch").at(-1);

  return (
    <div className="app">
      <header className="top">
        <h1>mtm</h1>
        <span className="pill">{status?.target ?? "…"}</span>
        {live.connected ? (
          <StatusLabel color="var(--good)" label="live" />
        ) : (
          <StatusLabel color="var(--critical)" label="disconnected" />
        )}
        {chain ? (
          <>
            <span className="meta">
              run <b>{chain.run_id}</b>
            </span>
            <span className="meta">
              slot <b>{chain.processed_slot.toLocaleString()}</b> · confirmed{" "}
              {chain.confirmed_slot.toLocaleString()}
            </span>
            <span className="meta">
              watcher lag <b>{compact(chain.lag_slots)}</b> slots
            </span>
            {epoch && (
              <span className="meta">
                epoch <b>{epoch.event.subject}</b>
              </span>
            )}
            <span className="meta">
              {chain.rpc_host} ·{" "}
              {chain.block_details === "slots"
                ? "slots only (public RPC)"
                : `${chain.block_details} blocks`}{" "}
              · {chain.our_wallets} wallets
            </span>
          </>
        ) : (
          <StatusLabel
            color="var(--warning)"
            label="chain-watch not reachable"
            hollow
          />
        )}
        {status?.job && (
          <span className="meta">
            job{" "}
            <b>
              #{status.job.id} {status.job.command}
            </b>{" "}
            running
          </span>
        )}
        <span className="spacer" />
        <nav className="tabs">
          {(["overview", "features", "control"] as Tab[]).map((t) => (
            <button key={t} aria-pressed={tab === t} onClick={() => setTab(t)}>
              {t === "overview"
                ? "Overview"
                : t === "features"
                  ? "Feature gates"
                  : "Control"}
            </button>
          ))}
        </nav>
        <ThemeToggle />
      </header>
      {tab === "overview" && <Overview live={live} onSelectSlot={setSlot} />}
      {tab === "features" && <Features />}
      {tab === "control" && <Control live={live} />}
      {slot && (
        <SlotDetail
          slot={slot}
          target={status?.target ?? "local"}
          onClose={() => setSlot(null)}
        />
      )}
    </div>
  );
}

function ThemeToggle() {
  const [theme, setTheme] = useState<string | null>(() =>
    localStorage.getItem("theme"),
  );
  const apply = (next: string | null) => {
    if (next) {
      document.documentElement.dataset.theme = next;
      localStorage.setItem("theme", next);
    } else {
      delete document.documentElement.dataset.theme;
      localStorage.removeItem("theme");
    }
    setTheme(next);
  };
  return (
    <select
      value={theme ?? ""}
      onChange={(e) => apply(e.target.value || null)}
      aria-label="Theme"
    >
      <option value="">system</option>
      <option value="light">light</option>
      <option value="dark">dark</option>
    </select>
  );
}
