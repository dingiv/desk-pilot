import { createCanvas, type SKRSContext2D } from '@napi-rs/canvas';
import type { Platform, Screenshot, UiElement } from './types.js';
import { contains } from './types.js';

/**
 * An in-memory {@link Platform} backing the Visual Scout server: a tiny two-screen
 * app (login → home) rendered with @napi-rs/canvas. It exists so the Scout HTTP
 * surface — capture / elements / click / type — runs on the **real** code path
 * (real PNGs, real hit testing, a real navigation state machine) without a display,
 * a browser, or an API key.
 *
 * It is the "extended mock" backend. The single-screen {@link MockPlatform} and its
 * tests stay untouched for the in-process path; this class is what the server drives
 * by default. Swapping in a real backend later = implementing the same `Platform`.
 *
 * Screens:
 *   - `login`: username/password inputs + Login (+ Forgot-password link). Successful
 *     login transitions to `home`. Same semantics as MockPlatform, so the existing
 *     scripted-login tests carry over.
 *   - `home`: a welcome line, two inert links (Articles / Profile), and a Logout
 *     button that returns to `login`.
 */
export class MultiScreenPlatform implements Platform {
  readonly width = 1280;
  readonly height = 800;

  private screen: 'login' | 'home' = 'login';
  private username = '';
  private password = '';
  private focused: 'username' | 'password' | null = null;
  private notice = '';
  private homeNotice = '';

  /** Which screen is currently shown — handy for tests/assertions. */
  get currentScreen(): 'login' | 'home' {
    return this.screen;
  }

  /** True once Login succeeded (we're on the home screen). */
  get isLoggedIn(): boolean {
    return this.screen === 'home';
  }

  /** Elements for the screen currently shown — single source of truth for hit testing. */
  get elements(): UiElement[] {
    if (this.screen === 'login') return this.loginElements();
    return this.homeElements();
  }

  private loginElements(): UiElement[] {
    return [
      { id: 'username', role: 'input', label: 'Username', bounds: { x: 420, y: 315, width: 440, height: 52 } },
      { id: 'password', role: 'input', label: 'Password', bounds: { x: 420, y: 415, width: 440, height: 52 } },
      { id: 'login', role: 'button', label: 'Login', bounds: { x: 420, y: 500, width: 440, height: 54 } },
      { id: 'forgot', role: 'link', label: 'Forgot password?', bounds: { x: 420, y: 575, width: 240, height: 28 } },
    ];
  }

  private homeElements(): UiElement[] {
    return [
      { id: 'articles', role: 'link', label: 'Articles', bounds: { x: 420, y: 320, width: 440, height: 40 } },
      { id: 'profile', role: 'link', label: 'Profile', bounds: { x: 420, y: 380, width: 440, height: 40 } },
      { id: 'logout', role: 'button', label: 'Logout', bounds: { x: 420, y: 700, width: 440, height: 54 } },
    ];
  }

  async getElements(): Promise<UiElement[]> {
    return this.elements;
  }

  async captureScreen(): Promise<Screenshot> {
    return { width: this.width, height: this.height, png: this.render() };
  }

  async performClick(x: number, y: number): Promise<void> {
    const hit = this.elements.find((el) => contains(el.bounds, x, y));
    if (!hit) return;

    if (this.screen === 'login') {
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
      return;
    }

    // home screen
    switch (hit.id) {
      case 'logout':
        this.logout();
        break;
      case 'articles':
        this.homeNotice = 'Articles: nothing here yet (mock).';
        break;
      case 'profile':
        this.homeNotice = `Signed in as ${this.username || 'user'} (mock).`;
        break;
    }
  }

  async performType(text: string): Promise<void> {
    if (this.screen !== 'login' || this.focused === null) return;
    if (this.focused === 'username') this.username += text;
    else this.password += text;
  }

  async performKeypress(keys: string): Promise<void> {
    if (this.screen === 'login' && /return|enter/i.test(keys)) this.attemptLogin();
  }

  async performScroll(): Promise<void> {
    /* Nothing scrolls in this mock. */
  }

  private attemptLogin(): void {
    if (this.username && this.password) {
      this.screen = 'home';
      this.focused = null;
      this.notice = '';
      this.homeNotice = '';
    } else {
      this.notice = 'Please enter both username and password.';
    }
  }

  private logout(): void {
    this.screen = 'login';
    this.username = '';
    this.password = '';
    this.focused = null;
    this.notice = '';
    this.homeNotice = '';
  }

  // ── rendering ──────────────────────────────────────────────────────────────

  private render(): Buffer {
    return this.screen === 'login' ? this.renderLogin() : this.renderHome();
  }

  private renderLogin(): Buffer {
    const canvas = createCanvas(this.width, this.height);
    const ctx = canvas.getContext('2d');

    ctx.fillStyle = '#e9ecef';
    ctx.fillRect(0, 0, this.width, this.height);

    const win = { x: 340, y: 170, w: 600, h: 460 };
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(win.x, win.y, win.w, win.h);
    ctx.fillStyle = '#f8f9fa';
    ctx.fillRect(win.x, win.y, win.w, 56);
    ctx.strokeStyle = '#dee2e6';
    ctx.lineWidth = 1;
    ctx.strokeRect(win.x, win.y, win.w, win.h);

    ctx.fillStyle = '#212529';
    ctx.font = 'bold 30px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText('Sign in to VRover', win.x + win.w / 2, win.y + 28);

    this.drawField(ctx, 'Username', this.username, 'username', this.focused === 'username');
    this.drawField(ctx, 'Password', '•'.repeat(this.password.length), 'password', this.focused === 'password');
    this.drawButton(ctx, 'login', 'Login', '#2563eb');

    const forgot = this.loginElements().find((e) => e.id === 'forgot')!;
    ctx.fillStyle = '#2563eb';
    ctx.font = '16px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'left';
    ctx.fillText('Forgot password?', forgot.bounds.x, forgot.bounds.y + 14);

    if (this.notice) {
      ctx.fillStyle = '#dc2626';
      ctx.font = '15px sans-serif';
      ctx.fillText(this.notice, 420, 660);
    }

    return canvas.toBuffer('image/png');
  }

  private renderHome(): Buffer {
    const canvas = createCanvas(this.width, this.height);
    const ctx = canvas.getContext('2d');

    ctx.fillStyle = '#e9ecef';
    ctx.fillRect(0, 0, this.width, this.height);

    const win = { x: 240, y: 120, w: 800, h: 620 };
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(win.x, win.y, win.w, win.h);
    ctx.strokeStyle = '#dee2e6';
    ctx.lineWidth = 1;
    ctx.strokeRect(win.x, win.y, win.w, win.h);

    ctx.fillStyle = '#f8f9fa';
    ctx.fillRect(win.x, win.y, win.w, 64);

    ctx.fillStyle = '#212529';
    ctx.font = 'bold 28px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'left';
    ctx.fillText(`Welcome, ${this.username || 'user'}`, win.x + 24, win.y + 32);

    // Links.
    for (const id of ['articles', 'profile'] as const) {
      const el = this.homeElements().find((e) => e.id === id)!;
      ctx.fillStyle = '#ffffff';
      ctx.fillRect(el.bounds.x, el.bounds.y, el.bounds.width, el.bounds.height);
      ctx.strokeStyle = '#ced4da';
      ctx.lineWidth = 1;
      ctx.strokeRect(el.bounds.x, el.bounds.y, el.bounds.width, el.bounds.height);
      ctx.fillStyle = '#2563eb';
      ctx.font = '18px sans-serif';
      ctx.textBaseline = 'middle';
      ctx.fillText(el.label, el.bounds.x + 12, el.bounds.y + el.bounds.height / 2 + 1);
    }

    this.drawButton(ctx, 'logout', 'Logout', '#6b7280');

    if (this.homeNotice) {
      ctx.fillStyle = '#495057';
      ctx.font = '15px sans-serif';
      ctx.textBaseline = 'middle';
      ctx.fillText(this.homeNotice, 420, 640);
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
    const el = this.loginElements().find((e) => e.id === id)!;
    const { x, y, width, height } = el.bounds;
    ctx.fillStyle = '#495057';
    ctx.font = '14px sans-serif';
    ctx.textBaseline = 'alphabetic';
    ctx.textAlign = 'left';
    ctx.fillText(label, x, y - 10);
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(x, y, width, height);
    ctx.strokeStyle = focused ? '#3b82f6' : '#ced4da';
    ctx.lineWidth = focused ? 2 : 1;
    ctx.strokeRect(x, y, width, height);
    ctx.fillStyle = '#212529';
    ctx.font = '18px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.fillText(value, x + 12, y + height / 2 + 1);
  }

  private drawButton(ctx: SKRSContext2D, id: string, label: string, fill: string): void {
    const el = this.elements.find((e) => e.id === id)!;
    const { x, y, width, height } = el.bounds;
    ctx.fillStyle = fill;
    ctx.fillRect(x, y, width, height);
    ctx.fillStyle = '#ffffff';
    ctx.font = 'bold 20px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText(label, x + width / 2, y + height / 2 + 1);
    ctx.textAlign = 'left';
  }
}
