/** Configuration for the native OmniParser. */
export interface NativeConfig {
  /** Path to the `icon_detect.onnx` weight file. */
  yoloPath: string;
  /** Detection confidence threshold (default 0.05). */
  boxThreshold?: number;
  /** NMS IoU threshold (default 0.1). */
  iouThreshold?: number;
}

/** A single detected element from the Rust parser. */
export interface NativeParsedElement {
  /** 0-based mark index. */
  mark: number;
  /** `"Icon"` or `"Text"`. */
  type: 'Icon' | 'Text';
  /** Bounding box left edge (pixels). */
  x1: number;
  /** Bounding box top edge (pixels). */
  y1: number;
  /** Bounding box right edge (pixels). */
  x2: number;
  /** Bounding box bottom edge (pixels). */
  y2: number;
  /** Whether the element is interactive. */
  interactivity: boolean;
  /** Caption / OCR text, if available. */
  content: string | null;
}

/** Result of calling the native parser. */
export interface NativeParseResult {
  /** PNG-encoded SoM-annotated image. */
  annotatedPng: Buffer;
  /** Detected elements with their bounding boxes. */
  elements: NativeParsedElement[];
  /** Image width in pixels. */
  width: number;
  /** Image height in pixels. */
  height: number;
}

/**
 * The native parser interface — call `parse(pngBuffer)` to run
 * detection + SoM annotation in a single Rust pass.
 */
export interface NativeParser {
  parse(pngBuffer: Buffer): NativeParseResult;
}
