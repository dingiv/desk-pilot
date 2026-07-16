import { createCanvas, type SKRSContext2D } from '@napi-rs/canvas';
import type { Platform, Screenshot, UiElement } from '../types.js';
import { contains } from '../types.js';

/**
 * A fully in-memory {@link Platform} that renders a synthetic "login" screen with @napi-rs/canvas.
 *
 * It exists so the agent loop, SoM annotation, and LLM vision all run on the **real** code path
 * (real PNG in, real PNG out, real hit testing) without needing OS capture, a browser, or an API
 * key to make sense. The state machine is tiny: focus a field, type into it, click Login to win.
 *
 * Geometry lives in {@link MockPlatform#elements} so rendering and getElements never drift apart.
 */
export class MockPlatform implements Platform {
  readonly width = 1280;
  readonly height = 800;

  private username = '';
  private password = '';
  private focused: 'username' | 'password' | null = null;
  private loggedIn = false;
  private notice = '';

  /** Single source of truth for the interactive elements and their bounds. */
  get elements(): UiElement[] {
    return [
      { id: 'username', role: 'input', label: 'Username', bounds: { x: 420, y: 315, width: 440, height: 52 } },
      { id: 'password', role: 'input', label: 'Password', bounds: { x: 420, y: 415, width: 440, height: 52 } },
      { id: 'login', role: 'button', label: 'Login', bounds: { x: 420, y: 500, width: 440, height: 54 } },
      { id: 'forgot', role: 'link', label: 'Forgot password?', bounds: { x: 420, y: 575, width: 240, height: 28 } },
    ];
  }

  /** True once Login was clicked with both fields filled — the demo's goal state. */
  get isLoggedIn(): boolean {
    return this.loggedIn;
  }

  async getElements(): Promise<UiElement[]> {
    return this.elements;
  }

  async captureScreen(): Promise<Screenshot> {
    return { width: this.width, height: this.height, png: this.render() };
  }

  async performClick(x: number, y: number): Promise<void> {
    if (this.loggedIn) return;
    const hit = this.elements.find((el) => contains(el.bounds, x, y));
    if (!hit) {
      this.focused = null;
      return;
    }
    switch (hit.id) {
      case 'username':
        this.focused = 'username';
        this.notice = '';
        break;
      case 'password':
        this.focused = 'password';
        this.notice = '';
        break;
      case 'login':
        this.attemptLogin();
        break;
      case 'forgot':
        this.focused = null;
        this.notice = 'Password reset link sent (mock).';
        break;
    }
  }

  async performType(text: string): Promise<void> {
    if (this.loggedIn || this.focused === null) return;
    if (this.focused === 'username') this.username += text;
    else this.password += text;
  }

  async performKeypress(keys: string): Promise<void> {
    // Treat Enter/Return as "submit the form".
    if (/return|enter/i.test(keys)) this.attemptLogin();
  }

  async performScroll(): Promise<void> {
    /* Nothing scrolls in this mock. */
  }

  private attemptLogin(): void {
    if (this.username && this.password) {
      this.loggedIn = true;
      this.notice = '';
    } else {
      this.notice = 'Please enter both username and password.';
    }
  }

  // ── rendering ──────────────────────────────────────────────────────────────

  private render(): Buffer {
    const canvas = createCanvas(this.width, this.height);
    const ctx = canvas.getContext('2d');

    // Desktop background.
    ctx.fillStyle = '#e9ecef';
    ctx.fillRect(0, 0, this.width, this.height);

    // Window + title bar.
    const win = { x: 340, y: 170, w: 600, h: 460 };
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(win.x, win.y, win.w, win.h);
    ctx.fillStyle = '#f8f9fa';
    ctx.fillRect(win.x, win.y, win.w, 56);
    ctx.strokeStyle = '#dee2e6';
    ctx.lineWidth = 1;
    ctx.strokeRect(win.x, win.y, win.w, win.h);

    // Title.
    ctx.fillStyle = '#212529';
    ctx.font = 'bold 30px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText('Sign in to VRover', win.x + win.w / 2, win.y + 28);

    this.drawField(ctx, 'Username', this.username, 'username', this.focused === 'username');
    this.drawField(ctx, 'Password', '•'.repeat(this.password.length), 'password', this.focused === 'password');
    this.drawButton(ctx);

    if (this.loggedIn) {
      ctx.fillStyle = '#16a34a';
      ctx.font = 'bold 22px sans-serif';
      ctx.textAlign = 'center';
      ctx.fillText(`✓ Logged in as ${this.username}`, win.x + win.w / 2, 660);
      ctx.textAlign = 'left';
      return canvas.toBuffer('image/png');
    }

    // Forgot-password link.
    const forgot = this.elements[3]!;
    ctx.fillStyle = '#2563eb';
    ctx.font = '16px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.fillText('Forgot password?', forgot.bounds.x, forgot.bounds.y + 14);

    if (this.notice) {
      ctx.fillStyle = '#dc2626';
      ctx.font = '15px sans-serif';
      ctx.textBaseline = 'middle';
      ctx.fillText(this.notice, 420, 660);
    }

    return canvas.toBuffer('image/png');
  }

  private drawField(
    ctx: SKRSContext2D,
    label: string,
    value: string,
    id: string,
    focused: boolean,
  ): void {
    const el = this.elements.find((e) => e.id === id)!;
    const { x, y, width, height } = el.bounds;
    // Label above the field.
    ctx.fillStyle = '#495057';
    ctx.font = '14px sans-serif';
    ctx.textBaseline = 'alphabetic';
    ctx.fillText(label, x, y - 10);
    // Field box.
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(x, y, width, height);
    ctx.strokeStyle = focused ? '#3b82f6' : '#ced4da';
    ctx.lineWidth = focused ? 2 : 1;
    ctx.strokeRect(x, y, width, height);
    // Value text inside.
    ctx.fillStyle = '#212529';
    ctx.font = '18px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'left';
    ctx.fillText(value, x + 12, y + height / 2 + 1);
  }

  private drawButton(ctx: SKRSContext2D): void {
    const el = this.elements.find((e) => e.id === 'login')!;
    const { x, y, width, height } = el.bounds;
    ctx.fillStyle = '#2563eb';
    ctx.fillRect(x, y, width, height);
    ctx.fillStyle = '#ffffff';
    ctx.font = 'bold 20px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText('Login', x + width / 2, y + height / 2 + 1);
    ctx.textAlign = 'left';
  }
}

// The Ubuntu-calculator mock lives in its own module; re-exported here so consumers keep
// importing all in-memory mocks from '@vrover/platform' (via this barrel → ./mock.js).
export { CalculatorPlatform } from './calculator.js';
