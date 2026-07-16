import type { Screenshot, UiElement } from '@vrover/platform';

/** A UI element paired with the number drawn over it on the annotated screenshot. */
export interface SoMElement {
  /** 1-based number painted on the image; the agent references elements by this. */
  mark: number;
  element: UiElement;
  /** Short label shown in the element table, e.g. "[button] Login". */
  description: string;
}

/** Output of the SoM pipeline: an annotated image plus the table mapping marks → elements. */
export interface SoMResult {
  annotated: Screenshot;
  table: SoMElement[];
}
