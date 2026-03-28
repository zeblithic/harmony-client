/** Abstraction over Tauri IPC for testability. */
export interface TauriAdapter {
  invoke(cmd: string, args?: Record<string, unknown>): Promise<unknown>;
  listen(event: string, handler: (event: { payload: unknown }) => void): Promise<() => void>;
}

export interface DiscoveredNode {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
  lastSeen: number;
  /** Latest CPU usage from health telemetry (0-100). */
  cpuPercent?: number;
  /** Latest memory usage in MB from health telemetry. */
  memMb?: number;
}

export interface CapacityUpdate {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
}

export interface ProfilePayload {
  address: string;
  displayName: string;
  statusText?: string;
  avatarUrl?: string;
}

export interface ZenohStatusEvent {
  status: 'connected' | 'disconnected' | 'error';
  endpoint?: string;
  error?: string;
}

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'error' | 'reconnecting';

export class ZenohService {
  connectionStatus: ConnectionStatus = 'disconnected';
  discoveredNodes: Map<string, DiscoveredNode> = new Map();
  peerProfiles: Map<string, ProfilePayload> = new Map();
  errorMessage?: string;
  /** Own address — profile updates matching this are filtered out
   *  so our own published profile doesn't appear in peerProfiles. */
  ownAddress: string | null = null;

  /** Called whenever service state changes so the UI can sync immediately. */
  onChange?: () => void;

  private adapter: TauriAdapter;
  private unlisteners: Array<() => void> = [];
  private lastEndpoint: string | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectAttempt = 0;
  private userDisconnected = false;
  /** True while a connect invoke is in-flight. Used to accept 'connected'
   *  events even if a stale error reset status to 'reconnecting'. */
  private connectInFlight = false;

  constructor(adapter: TauriAdapter) {
    this.adapter = adapter;
  }

  async init(): Promise<void> {
    const unlistenCapacity = await this.adapter.listen(
      'capacity-update',
      (event) => {
        // Only accept capacity updates when connected
        if (this.connectionStatus !== 'connected') return;
        const update = event.payload as CapacityUpdate;
        const existing = this.discoveredNodes.get(update.nodeAddr);
        this.discoveredNodes.set(update.nodeAddr, {
          ...existing,  // preserve cpuPercent, memMb from health telemetry
          ...update,
          lastSeen: Date.now(),
        });
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenCapacity);

    const unlistenProfile = await this.adapter.listen(
      'profile-update',
      (event) => {
        if (this.connectionStatus !== 'connected') return;
        const profile = event.payload as ProfilePayload;
        // Filter out our own profile — Zenoh delivers local PUTs to
        // local subscribers by default, so our published profile echoes back.
        if (this.ownAddress && profile.address === this.ownAddress) return;
        this.peerProfiles.set(profile.address, profile);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenProfile);

    const unlistenStatus = await this.adapter.listen(
      'zenoh-status',
      (event) => {
        const status = event.payload as ZenohStatusEvent;
        if (status.status === 'connected') {
          // Accept 'connected' if we're actively connecting OR if a connect
          // invoke is in flight (a stale error event may have temporarily
          // set status to 'reconnecting' while the invoke was pending).
          if ((this.connectionStatus === 'connecting' || this.connectInFlight) && !this.userDisconnected) {
            this.connectInFlight = false;
            this.connectionStatus = 'connected';
            this.errorMessage = undefined;
            this.reconnectAttempt = 0;
            this.cancelReconnect();
            this.onChange?.();
          }
        } else if (status.status === 'disconnected') {
          if (this.connectionStatus === 'disconnected') {
            this.errorMessage = undefined;
            this.onChange?.();
          }
        } else if (status.status === 'error') {
          if (this.userDisconnected) return;
          if (this.reconnectTimer !== null) return;
          // Don't clear connectInFlight here — the connected handler needs
          // it to accept a valid connected event that arrives after this
          // stale error. connectInFlight is cleared by: connected handler
          // (success), disconnect(), destroy(), or catch block (no reconnect).
          this.connectionStatus = 'error';
          this.errorMessage = status.error;
          if (this.lastEndpoint) {
            this.scheduleReconnect();
          } else {
            this.connectInFlight = false;
            this.onChange?.();
          }
        }
      },
    );
    this.unlisteners.push(unlistenStatus);

    const unlistenTelemetry = await this.adapter.listen(
      'telemetry-event',
      (event) => {
        if (this.connectionStatus !== 'connected') return;
        const telem = event.payload as import('./telemetry-types').TelemetryEvent;
        const node = this.discoveredNodes.get(telem.nodeAddr);
        if (!node) return;

        // Guard: payload must be a non-null object before accessing properties
        if (telem.payload === null || typeof telem.payload !== 'object') return;

        if (telem.intent === 'health') {
          const p = telem.payload as import('./telemetry-types').HealthPayload;
          if (p.cpu_percent !== undefined) node.cpuPercent = p.cpu_percent;
          if (p.mem_mb !== undefined) node.memMb = p.mem_mb;
          node.lastSeen = telem.timestamp * 1000; // epoch seconds → ms
          this.onChange?.();
        } else if (telem.intent === 'capacity_changed') {
          const p = telem.payload as import('./telemetry-types').CapacityChangedPayload;
          if (p.ready !== undefined) node.ready = p.ready;
          if (p.model_cid !== undefined) node.modelCid = p.model_cid;
          node.lastSeen = telem.timestamp * 1000; // epoch seconds → ms
          this.onChange?.();
        }
        // Unknown intents: silently ignore (forward-compatible)
      },
    );
    this.unlisteners.push(unlistenTelemetry);
  }

  async connect(endpoint: string, isReconnect = false): Promise<void> {
    this.userDisconnected = false;
    this.lastEndpoint = endpoint;
    this.cancelReconnect();
    // Only reset backoff counter on user-initiated connect, not auto-reconnect.
    // Otherwise the exponential delay (2s→4s→8s→…) never grows.
    if (!isReconnect) {
      this.reconnectAttempt = 0;
    }
    this.connectionStatus = 'connecting';
    this.errorMessage = undefined;
    this.connectInFlight = true;
    this.onChange?.();
    try {
      await this.adapter.invoke('connect_zenoh', { endpoint });
    } catch (e) {
      // connectInFlight stays true until the backend emits a status event.
      // Don't clear it here — the connected/error event handlers need it
      // to accept events that arrive after the invoke promise settles.
      if (this.userDisconnected) {
        this.connectInFlight = false;
        return;
      }
      if (this.reconnectTimer !== null) {
        // Error handler already scheduled a reconnect — don't double-handle.
        // connectInFlight stays true for the connected handler.
        return;
      }

      this.connectInFlight = false;
      this.connectionStatus = 'error';
      this.errorMessage = String(e);
      if (this.lastEndpoint) {
        this.scheduleReconnect();
      } else {
        this.onChange?.();
      }
    }
  }

  async disconnect(): Promise<void> {
    this.userDisconnected = true;
    this.connectInFlight = false;
    this.cancelReconnect();
    this.connectionStatus = 'disconnected';
    this.errorMessage = undefined;
    this.discoveredNodes.clear();
    this.peerProfiles.clear();
    this.onChange?.();
    try {
      await this.adapter.invoke('disconnect_zenoh');
    } catch {
      // Ignore disconnect errors
    }
  }

  private scheduleReconnect(): void {
    // Idempotent — if already reconnecting with a timer, don't double-schedule.
    // This prevents double-increment of reconnectAttempt when both the invoke
    // catch and the error event handler call this method for the same failure.
    if (this.reconnectTimer !== null) return;
    if (!this.lastEndpoint || this.userDisconnected) return;
    const delay = Math.min(2000 * Math.pow(2, this.reconnectAttempt), 30_000);
    this.reconnectAttempt++;
    this.connectionStatus = 'reconnecting';
    this.errorMessage = `Reconnecting in ${Math.round(delay / 1000)}s...`;
    this.onChange?.();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (this.lastEndpoint && !this.userDisconnected) {
        this.connect(this.lastEndpoint, true).catch(() => {});
      }
    }, delay);
  }

  private cancelReconnect(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  async publishProfile(profile: ProfilePayload): Promise<void> {
    await this.adapter.invoke('publish_profile', { profile });
  }

  destroy(): void {
    this.userDisconnected = true;
    this.connectInFlight = false;
    this.cancelReconnect();
    for (const unlisten of this.unlisteners) {
      unlisten();
    }
    this.unlisteners = [];
  }
}
