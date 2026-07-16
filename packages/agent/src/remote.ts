import type { Platform, Screenshot, UiElement } from '@vrover/platform';
import { ScoutClient } from '@vrover/scout-client';
import type { HandshakeAck, HandshakeRequest } from '@vrover/scout-client';

/**
 * Brain-side {@link Platform} that talks to a Visual Scout server — a thin adapter
 * over {@link ScoutClient} (the standalone SDK in `@vrover/scout-client`). A drop-in
 * replacement for {@link MockPlatform}: pass it to
 * `runAgent({ platform: new RemotePlatform(host, port) })` and the agent operates
 * whatever backend the Scout server is driving, over the network — the D10
 * component split, brain ⇄ Scout as separate processes.
 *
 * This is the project's only internal consumer of the standalone client SDK; it lives
 * in the brain (`@vrover/agent`) so the client package stays free of project deps.
 * All connection / handshake / decode logic lives in {@link ScoutClient}; this class
 * only maps the {@link Platform} primitives onto the SDK's thin API.
 */
export class RemotePlatform implements Platform {
  private readonly client: ScoutClient;
  /** Resolves with the handshake ack once the session is established. */
  readonly ready: Promise<HandshakeAck>;

  constructor(host: string, port: number, req: HandshakeRequest = {}) {
    this.client = new ScoutClient(host, port, req);
    this.ready = this.client.ready;
  }

  /** The ack info, as a small health object (handy for clients/tests). */
  async health(): Promise<{ ok: true; backend: string; sessionId: string }> {
    return this.client.health();
  }

  async captureScreen(): Promise<Screenshot> {
    return this.client.capture();
  }

  async getElements(): Promise<UiElement[]> {
    return this.client.elements();
  }

  async performClick(x: number, y: number): Promise<void> {
    await this.client.click(x, y);
  }

  async performType(text: string): Promise<void> {
    await this.client.type(text);
  }

  async performScroll(x: number, y: number, direction: 'up' | 'down'): Promise<void> {
    await this.client.scroll(x, y, direction);
  }

  async performKeypress(keys: string): Promise<void> {
    await this.client.keypress(keys);
  }

  /** Close the underlying connection. */
  close(): void {
    this.client.close();
  }
}
