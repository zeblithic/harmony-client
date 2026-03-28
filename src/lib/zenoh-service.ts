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

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'error';

export class ZenohService {
  connectionStatus: ConnectionStatus = 'disconnected';
  discoveredNodes: Map<string, DiscoveredNode> = new Map();
  errorMessage?: string;

  /** Called whenever service state changes so the UI can sync immediately. */
  onChange?: () => void;

  private adapter: TauriAdapter;
  private unlisteners: Array<() => void> = [];

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
          // Only accept 'connected' if we're still in 'connecting' state.
          // If disconnect() was called while connect was in flight,
          // connectionStatus is already 'disconnected' and we must ignore
          // the stale 'connected' event from the backend.
          if (this.connectionStatus === 'connecting') {
            this.connectionStatus = 'connected';
            this.errorMessage = undefined;
          }
        } else if (status.status === 'disconnected') {
          // Only accept 'disconnected' if we're already disconnected
          // (i.e., the user explicitly called disconnect()). Ignore stale
          // events that arrive after a reconnect has already succeeded
          // ('connected') or is in progress ('connecting').
          if (this.connectionStatus === 'disconnected') {
            this.errorMessage = undefined;
          }
        } else if (status.status === 'error') {
          this.connectionStatus = 'error';
          this.errorMessage = status.error;
        }
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenStatus);
  }

  async connect(endpoint: string): Promise<void> {

    this.connectionStatus = 'connecting';
    this.errorMessage = undefined;
    this.onChange?.();
    try {
      await this.adapter.invoke('connect_zenoh', { endpoint });
    } catch (e) {
      this.connectionStatus = 'error';
      this.errorMessage = String(e);
      this.onChange?.();
    }
  }

  async disconnect(): Promise<void> {

    this.connectionStatus = 'disconnected';
    this.errorMessage = undefined;
    this.discoveredNodes.clear();
    this.onChange?.();
    try {
      await this.adapter.invoke('disconnect_zenoh');
    } catch {
      // Ignore disconnect errors — backend may already be gone
    }
  }

  destroy(): void {
    for (const unlisten of this.unlisteners) {
      unlisten();
    }
    this.unlisteners = [];
  }
}
