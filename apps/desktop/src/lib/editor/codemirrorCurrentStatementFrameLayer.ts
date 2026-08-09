import type { EditorView, LayerMarker, ViewUpdate } from "@codemirror/view";
import type { Extension } from "@codemirror/state";

export interface StatementFrameRequest {
  /** Document range covered by the frame; null hides the frame. */
  from: number;
  to: number;
}

export type StatementFrameResolver = (view: EditorView) => StatementFrameRequest | null;

/** Breathing room between the measured text bounds and the frame border (pixels). */
export const FRAME_INSET_PX = 2;

export interface FrameRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

interface InkBounds {
  top: number;
  bottom: number;
}

type Viewish = Pick<EditorView, "coordsAtPos" | "domAtPos" | "state" | "scrollDOM" | "scaleX" | "scaleY" | "lineBlockAt" | "defaultCharacterWidth">;

/**
 * Measure a single character's real ink bounds via the DOM.
 *
 * `domAtPos` returns the text node holding the character; a Range over that
 * one character yields the exact box the browser laid out (correct for any
 * font, glyph class or language — unlike line-box or `ch` estimates). When
 * the node isn't measurable (no TEXT_NODE / no ownerDocument / zero-height
 * rect), falls back to the line box from `coordsAtPos` — coarser but still
 * produces a usable top/bottom for the frame.
 */
function charInkBounds(view: Viewish, pos: number): InkBounds | null {
  const dom = view.domAtPos(pos, 1);
  const node = dom?.node;
  if (node && node.nodeType === 3 /* TEXT_NODE */) {
    const doc = node.ownerDocument;
    const range = doc && doc.createRange();
    if (range && node.textContent) {
      const start = dom!.offset;
      range.setStart(node, start);
      range.setEnd(node, Math.min(start + 1, node.textContent.length));
      const rect = range.getBoundingClientRect();
      if (rect && rect.height > 0) return { top: rect.top, bottom: rect.bottom };
    }
  }
  const coords = view.coordsAtPos(pos, 1);
  return coords ? { top: coords.top, bottom: coords.bottom } : null;
}

/**
 * Measure the pixel rectangle of one statement rendered in `view`.
 *
 * The frame is a single continuous rectangle: it starts at the first
 * statement character, spans every statement line (blank and comment lines
 * included), and ends at the widest line.
 *
 * **Vertical bounds**: prefer `charInkBounds` (pixel-perfect ink) for the
 * first and last characters when they're in the viewport; fall back to
 * `lineBlockAt` (document-level coordinates, includes leading) for
 * off-viewport positions.
 *
 * **Horizontal bounds**: use `coordsAtPos` for visible lines (exact on tabs,
 * CJK/fullwidth glyphs and non-monospaced fonts). For off-viewport lines,
 * estimate using an offset calibrated from the first visible line's
 * `coordsAtPos` result, so estimates account for editor padding, gutter
 * width, and zoom.
 *
 * All four sides get a symmetric `FRAME_INSET_PX` breathing room.
 */
export function currentStatementFrameRect(view: Viewish, from: number, to: number): FrameRect | null {
  const doc = view.state.doc;
  if (!doc || to < from || from > doc.length || to > doc.length || to === from) return null;

  const startLine = doc.lineAt(from);
  const endLine = doc.lineAt(to);

  // Vertical bounds: prefer charInkBounds (pixel-perfect ink) for the first
  // and last characters when they're in the viewport; fall back to lineBlockAt
  // (document-level coordinates, includes leading) for off-viewport positions.
  const startBlock = view.lineBlockAt(from);
  const endBlock = view.lineBlockAt(to);
  const scrollTop = view.scrollDOM.scrollTop;
  const base = view.scrollDOM.getBoundingClientRect();

  const firstCharInk = charInkBounds(view, from);
  const lastCharInk = charInkBounds(view, Math.max(from, to - 1));

  let top: number;
  let bottom: number;

  if (firstCharInk) {
    // charInkBounds returns screen coordinates; convert to scrollDOM-relative
    const topOffset = base.top - scrollTop * view.scaleY;
    top = firstCharInk.top - topOffset;
  } else {
    // lineBlockAt returns document coordinates; convert by subtracting scrollTop
    top = startBlock.top - scrollTop;
  }

  if (lastCharInk) {
    const topOffset = base.top - scrollTop * view.scaleY;
    bottom = lastCharInk.bottom - topOffset;
  } else {
    bottom = endBlock.bottom - scrollTop;
  }

  if (bottom - top <= 0) return null;

  // Horizontal bounds: use coordsAtPos for visible lines. For off-viewport
  // lines, estimate using an offset calibrated from the first visible line.
  let left = Infinity;
  let right = -Infinity;
  const charWidth = view.defaultCharacterWidth;
  let calibratedOffset: number | null = null;

  for (let lineNumber = startLine.number; lineNumber <= endLine.number; lineNumber += 1) {
    const line = doc.line(lineNumber);
    const anchorFrom = lineNumber === startLine.number ? from : line.from;
    const anchorTo = Math.min(line.to, to);
    const lineLeft = view.coordsAtPos(anchorFrom, 1);
    const lineRight = view.coordsAtPos(anchorTo, -1);

    if (lineLeft) {
      left = Math.min(left, lineLeft.left);
      right = Math.max(right, lineRight?.right ?? lineLeft.left);
      // Calibrate offset from the first visible line
      if (calibratedOffset === null && charWidth > 0) {
        const indent = line.text.match(/^\s*/)?.[0].length || 0;
        calibratedOffset = lineLeft.left - indent * charWidth;
      }
    } else if (calibratedOffset !== null && charWidth > 0) {
      // Estimate: use calibrated offset + (indent + text.length) * charWidth
      const indent = line.text.match(/^\s*/)?.[0].length || 0;
      const estimatedRight = calibratedOffset + (indent + line.text.length) * charWidth;
      right = Math.max(right, estimatedRight);
    }
  }

  // If no lines were measurable (entire statement off-viewport), skip frame
  if (!isFinite(left) || !isFinite(right)) return null;

  const leftOffset = base.left - view.scrollDOM.scrollLeft * view.scaleX;

  return {
    left: left - leftOffset - FRAME_INSET_PX,
    top: top - FRAME_INSET_PX,
    width: right - left + FRAME_INSET_PX * 2,
    height: bottom - top + FRAME_INSET_PX * 2,
  };
}

interface CurrentStatementFrameModule {
  layer: (config: { above: boolean; class?: string; markers(view: EditorView): readonly LayerMarker[]; update(update: ViewUpdate, layer: HTMLElement): boolean }) => Extension;
  RectangleMarker: new (className: string, left: number, top: number, width: number | null, height: number) => LayerMarker;
}

/**
 * Draw one continuous green rectangle around the current executable
 * statement. `resolve` decides visibility and returns the statement range;
 * returning null hides the frame.
 */
export function currentStatementFrameLayer(viewModule: CurrentStatementFrameModule, resolve: StatementFrameResolver): Extension {
  return viewModule.layer({
    above: false,
    class: "cm-db-currentStatementFrameLayer",
    markers(view) {
      const request = resolve(view);
      if (!request || request.to < request.from) return [];
      const rect = currentStatementFrameRect(view, request.from, request.to);
      if (!rect) return [];
      return [new viewModule.RectangleMarker("cm-db-currentStatementFrame", rect.left, rect.top, rect.width, rect.height)];
    },
    update(update) {
      return update.docChanged || update.selectionSet || update.viewportChanged || update.geometryChanged || update.transactions.some((transaction) => transaction.reconfigured);
    },
  });
}
