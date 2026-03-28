/** Matches the camelCase-serialized TelemetryEventPayload from the Tauri backend. */
export interface TelemetryEvent {
  nodeAddr: string;
  intent: string;
  sequence: number;
  timestamp: number;
  /** Opaque JSON payload — shape depends on intent. */
  payload: unknown;
  confidence?: number;
  source?: string;
}

/** Shape of payload when intent === "health". */
export interface HealthPayload {
  cpu_percent?: number;
  mem_mb?: number;
}

/** Shape of payload when intent === "capacity_changed". */
export interface CapacityChangedPayload {
  model_cid?: string;
  ready?: boolean;
}
