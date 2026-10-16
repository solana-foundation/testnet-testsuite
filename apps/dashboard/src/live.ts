// The live feed from control's /api/live websocket, folded into one state.

import { useEffect, useReducer } from "react";

import type { ChainEvent, Job, Live, SlotRecord, Source, Status } from "./api";

/** Slots kept client-side: 10 min at 200 ms. */
const SLOT_CAP = 3_000;
const OUTPUT_CAP = 5_000;

export interface TimelineEvent {
  source: Source;
  event: ChainEvent;
}

export interface LiveState {
  connected: boolean;
  status: Status | null;
  slots: SlotRecord[];
  events: TimelineEvent[];
  /** Job updates seen live, by id (newest state). */
  jobs: Map<number, Job>;
  output: Map<number, string[]>;
}

type Action = { type: "connected"; connected: boolean } | Live;

const initial: LiveState = {
  connected: false,
  status: null,
  slots: [],
  events: [],
  jobs: new Map(),
  output: new Map(),
};

function appendSlots(
  current: SlotRecord[],
  incoming: SlotRecord[],
): SlotRecord[] {
  const last = current.at(-1)?.slot ?? -1;
  const fresh = incoming.filter((s) => s.slot > last);
  const merged = fresh.length ? current.concat(fresh) : current;
  return merged.length > SLOT_CAP
    ? merged.slice(merged.length - SLOT_CAP)
    : merged;
}

function reduce(state: LiveState, action: Action): LiveState {
  switch (action.type) {
    case "connected":
      return { ...state, connected: action.connected };
    case "snapshot":
      return {
        ...state,
        status: action.status,
        slots: action.slots.slice(-SLOT_CAP),
        events: action.events.map(([source, event]) => ({ source, event })),
      };
    case "status":
      return { ...state, status: action.status };
    case "slots":
      return { ...state, slots: appendSlots(state.slots, action.slots) };
    case "event":
      return {
        ...state,
        events: [
          ...state.events,
          { source: action.source, event: action.event },
        ],
      };
    case "reset":
      return {
        ...state,
        slots: [],
        events: state.events.filter((e) => e.source === "control"),
      };
    case "job": {
      const jobs = new Map(state.jobs);
      jobs.set(action.job.id, action.job);
      return { ...state, jobs };
    }
    case "job_output": {
      const output = new Map(state.output);
      const lines = [...(output.get(action.id) ?? []), action.line];
      output.set(
        action.id,
        lines.length > OUTPUT_CAP ? lines.slice(-OUTPUT_CAP) : lines,
      );
      return { ...state, output };
    }
  }
}

/** Connect to /api/live and reconnect with backoff. */
export function useLive(): LiveState {
  const [state, dispatch] = useReducer(reduce, initial);
  useEffect(() => {
    let socket: WebSocket | null = null;
    let retry = 500;
    let timer: number | undefined;
    let closed = false;
    const connect = () => {
      const scheme = location.protocol === "https:" ? "wss" : "ws";
      socket = new WebSocket(`${scheme}://${location.host}/api/live`);
      socket.onopen = () => {
        retry = 500;
        dispatch({ type: "connected", connected: true });
      };
      socket.onmessage = (message: MessageEvent<string>) => {
        dispatch(JSON.parse(message.data) as Live);
      };
      socket.onclose = () => {
        dispatch({ type: "connected", connected: false });
        if (!closed) {
          timer = window.setTimeout(connect, retry);
          retry = Math.min(retry * 2, 10_000);
        }
      };
    };
    connect();
    return () => {
      closed = true;
      window.clearTimeout(timer);
      socket?.close();
    };
  }, []);
  return state;
}
