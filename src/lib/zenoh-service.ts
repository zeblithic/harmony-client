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

  private adapter: TauriAdapter;
  private unlisteners: Array<() => void> = [];

  constructor(adapter: TauriAdapter) {
    this.adapter = adapter;
  }

  async init(): Promise<void> {
    const unlistenCapacity = await this.adapter.listen(
      'capacity-update',
      (event) => {
        const update = event.payload as CapacityUpdate;
        this.discoveredNodes.set(update.nodeAddr, {
          ...update,
          lastSeen: Date.now(),
        });
      },
    );
    this.unlisteners.push(unlistenCapacity);

    const unlistenStatus = await this.adapter.listen(
      'zenoh-status',
      (event) => {
        const status = event.payload as ZenohStatusEvent;
        if (status.status === 'connected') {
          this.connectionStatus = 'connected';
          this.errorMessage = undefined;
        } else if (status.status === 'disconnected') {
          this.connectionStatus = 'disconnected';
          this.errorMessage = undefined;
        } else if (status.status === 'error') {
          this.connectionStatus = 'error';
          this.errorMessage = status.error;
        }
      },
    );
    this.unlisteners.push(unlistenStatus);
  }

  async connect(endpoint: string): Promise<void> {
    this.connectionStatus = 'connecting';
    this.errorMessage = undefined;
    try {
      await this.adapter.invoke('connect_zenoh', { endpoint });
    } catch (e) {
      this.connectionStatus = 'error';
      this.errorMessage = String(e);
    }
  }

  async disconnect(): Promise<void> {
    this.connectionStatus = 'disconnected';
    this.errorMessage = undefined;
    this.discoveredNodes.clear();
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
