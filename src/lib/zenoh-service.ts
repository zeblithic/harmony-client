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
}

export interface CapacityUpdate {
  nodeAddr: string;
  modelCid: string;
  ready: boolean;
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
  errorMessage?: string;

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
        this.discoveredNodes.set(update.nodeAddr, {
          ...update,
          lastSeen: Date.now(),
        });
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenCapacity);

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
          this.connectInFlight = false;
          this.connectionStatus = 'error';
          this.errorMessage = status.error;
          if (this.lastEndpoint) {
            // scheduleReconnect calls onChange internally
            this.scheduleReconnect();
          } else {
            this.onChange?.();
          }
        }
      },
    );
    this.unlisteners.push(unlistenStatus);
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
