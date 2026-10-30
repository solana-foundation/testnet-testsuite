// Wire types mirrored from crates/control-client and crates/chain-watch-client.
// Keep in step with the Rust definitions (serde snake_case).

export type BlockDetails = "full" | "accounts" | "slots";

export interface Health {
  run_id: string;
  block_details: BlockDetails;
  rpc_host: string;
  processed_slot: number;
  confirmed_slot: number;
  cursor_slot: number;
  lag_slots: number;
  our_wallets: number;
}

export type SlotStatus = "produced" | "skipped" | "unavailable";

export interface SlotRecord {
  slot: number;
  leader: string | null;
  leader_client: string | null;
  leader_version: string | null;
  status: SlotStatus;
  block_time: number | null;
  block_height: number | null;
  tx_total: number;
  tx_vote: number;
  tx_failed: number;
  tx_ours: number;
  tx_ours_failed: number;
  cu_consumed: number | null;
  cost_units: number | null;
  fees_lamports: number;
  prio_p50: number | null;
  prio_p90: number | null;
  prio_max: number | null;
  observed_at_us: number;
}

export interface Feature {
  id: string;
  description: string | null;
  activated_slot: number | null;
  activated_time_us: number | null;
  time_estimated: boolean;
  first_seen_pending_us: number | null;
}

export interface ChainEvent {
  id: number;
  time_us: number;
  slot: number | null;
  kind: string;
  subject: string;
  detail: Record<string, unknown> | null;
}

export interface OurTx {
  signature: string;
  slot: number;
  payer: string;
  role: string;
  ok: boolean;
  error: string | null;
  error_program: string | null;
  cu_consumed: number | null;
  cu_limit: number | null;
  prio_price: number | null;
  fee_lamports: number;
  programs: string;
}

export const COMMANDS = [
  "up",
  "down",
  "status",
  "deploy",
  "bootstrap",
  "observe",
  "watch",
  "snapshot",
  "report",
  "measure",
  "teardown",
] as const;
export type Command = (typeof COMMANDS)[number];

export const CONFIRM: ReadonlySet<Command> = new Set([
  "down",
  "deploy",
  "teardown",
]);

export interface JobRequest {
  command: Command;
  operands: string[];
  scale: string | null;
  run_id: string | null;
  janitor: boolean;
  close_programs: boolean;
  confirm: string | null;
}

export type JobState = "running" | "succeeded" | "failed" | "cancelled";

export interface Job {
  id: number;
  command: Command;
  args: string[];
  env: [string, string][];
  target: string;
  state: JobState;
  exit_code: number | null;
  started_us: number;
  ended_us: number | null;
}

export interface Process {
  name: string;
  pid: number | null;
  running: boolean;
}

export interface Status {
  target: string;
  processes: Process[];
  chain: Health | null;
  job: Job | null;
  scales: string[];
  /** Scale `up` started the scenario at ("" = base, null = unknown). */
  active_scale: string | null;
}

export type Source = "chain" | "control";

export type Live =
  | {
      type: "snapshot";
      status: Status;
      slots: SlotRecord[];
      events: [Source, ChainEvent][];
    }
  | { type: "status"; status: Status }
  | { type: "slots"; slots: SlotRecord[] }
  | { type: "event"; source: Source; event: ChainEvent }
  | { type: "job"; job: Job }
  | { type: "job_output"; id: number; line: string }
  | { type: "reset"; run_id: string };

async function check(response: Response): Promise<Response> {
  if (!response.ok) {
    throw new Error(
      (await response.text()) || `${response.status} ${response.statusText}`,
    );
  }
  return response;
}

export async function getJson<T>(path: string): Promise<T> {
  return (await (await check(await fetch(path))).json()) as T;
}

export async function getText(path: string): Promise<string> {
  return (await check(await fetch(path))).text();
}

export async function startJob(request: JobRequest): Promise<Job> {
  const response = await fetch("/api/jobs", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(request),
  });
  return (await (await check(response)).json()) as Job;
}

export async function cancelJob(id: number): Promise<void> {
  await check(await fetch(`/api/jobs/${id}/cancel`, { method: "POST" }));
}

// --- Prometheus (through control's proxy) -----------------------------------

interface PromMatrix {
  status: string;
  data: {
    resultType: string;
    result: { metric: Record<string, string>; values: [number, string][] }[];
  };
}
interface PromVector {
  status: string;
  data: {
    resultType: string;
    result: { metric: Record<string, string>; value: [number, string] }[];
  };
}

export interface Series {
  labels: Record<string, string>;
  /** unix seconds */
  times: number[];
  values: number[];
}

export async function queryRange(
  query: string,
  start: number,
  end: number,
  step: number,
): Promise<Series[]> {
  const params = new URLSearchParams({
    query,
    start: `${start}`,
    end: `${end}`,
    step: `${step}`,
  });
  const body = await getJson<PromMatrix>(`/api/prom/query_range?${params}`);
  return body.data.result.map((r) => ({
    labels: r.metric,
    times: r.values.map(([t]) => t),
    values: r.values.map(([, v]) => Number(v)),
  }));
}

export async function queryInstant(
  query: string,
): Promise<{ labels: Record<string, string>; value: number }[]> {
  const body = await getJson<PromVector>(
    `/api/prom/query?${new URLSearchParams({ query })}`,
  );
  return body.data.result.map((r) => ({
    labels: r.metric,
    value: Number(r.value[1]),
  }));
}

// --- Compose (crates/scenario-spec through control) --------------------------

export const VENUES = ["manifest", "orca-whirlpool", "raydium-clmm"] as const;
export type Venue = (typeof VENUES)[number];

export interface Spec {
  /** Assets besides USDC (catalog names). */
  tokens: string[];
  /** Venues every <token>/USDC pair is listed on. */
  venues: Venue[];
  /** Extra Orca pools between two non-USDC tokens. */
  crosses: [string, string][];
  makers: {
    ladders: number;
    ranges: number;
    pool_arbs: number;
    thin_tokens: string[];
    /** Decimal string; null sizes it to the flow. */
    capital: string | null;
  };
  takers: {
    count: number;
    trades_per_min: number;
    median_usd: number;
    sigma: number;
    large_probability: number;
    large_multiplier: number;
    exact_out_probability: number;
    slippage_bps: number;
    sol: string | null;
  };
  arb: { bots: number };
}

export interface NamedSpec {
  name: string;
  spec: Spec;
}

export interface Chain {
  name: string;
  slot_ms: number;
  block_cu_limit: number;
  account_cu_limit: number;
  host_cu_per_s: number | null;
  shared: boolean;
}

export type Calibration = Record<string, unknown>;

export interface ComposeInfo {
  presets: NamedSpec[];
  overlays: NamedSpec[];
  catalog: string[];
  chains: Chain[];
  calibration: Calibration;
}

export type Limit = "host" | "block" | "account";

export interface Estimate {
  chain: Chain;
  takers: number;
  makers: number;
  bots: number;
  participants: number;
  pools: number;
  capital: string;
  offered_swaps_per_s: number;
  trade_seconds: number;
  taker_tx_per_s: number;
  maker_tx_per_s: number;
  arb_tx_per_s: number;
  tps: number;
  tps_capped: number;
  cu_per_s: number;
  cu_per_block: number;
  headroom: { limit: Limit; utilisation: number; detail: string }[];
  bottleneck: Limit | null;
  venues: { venue: Venue; legs_per_s: number; share: number }[];
  hottest: { pool: string; legs_per_s: number; cu_per_block: number }[];
  bootstrap: { transactions: number; seconds: number };
  sol: { fees_per_hour: number; rent: number; float: number };
  warnings: string[];
}

export interface EstimateResponse {
  error: string | null;
  estimate: Estimate;
  name: string;
  target_takers: number | null;
}

async function postJson<T>(path: string, body: unknown): Promise<T> {
  const response = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return (await (await check(response)).json()) as T;
}

export function estimateSpec(
  spec: Spec,
  chain: Chain,
  targetTps: number | null,
): Promise<EstimateResponse> {
  return postJson("/api/compose/estimate", {
    spec,
    chain,
    target_tps: targetTps,
  });
}

export function composeSpec(
  spec: Spec,
): Promise<{ name: string; created: boolean }> {
  return postJson("/api/compose", { spec });
}
