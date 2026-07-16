import type { GroundingSource, Platform, UiElement } from '@vrover/platform';

/**
 * The first real {@link GroundingSource} — Scout's ④ grounding seam, finally wired
 * (per decisions.md D9: grounding is a Scout concern, abstracted away from the agent).
 *
 * Tier 1 of the D11感知分层 (accessibility tree / DOM): when the backend platform
 * already exposes precise elements (the mock does; Playwright's DOM would), we take
 * them directly — free, exact, no pixels. This class is the cheap tier; later tiers
 * (传统 CV + OCR, then ML detection) plug in as additional `GroundingSource`
 * implementations and the Scout server picks among them, all emitting the same
 * `UiElement[]` shape.
 */
export class PlatformGroundingSource implements GroundingSource {
  constructor(private readonly backend: Platform) {}

  async detect(): Promise<UiElement[]> {
    return this.backend.getElements();
  }
}
