import { describe, expect, it } from "vitest";
import { Text } from "@codemirror/state";
import { currentStatementFrameRect, FRAME_INSET_PX } from "@/lib/editor/codemirrorCurrentStatementFrameLayer";

interface CoordRect {
  left: number;
  right: number;
  top: number;
  bottom: number;
  height: number;
}
interface DomNode {
  nodeType: number;
  textContent: string | null;
  ownerDocument?: {
    createRange: () => { getBoundingClientRect: () => CoordRect };
    createTreeWalker: (root: DomNode, whatToShow: number) => { nextNode: () => DomNode | null };
  };
}

interface LineBlock {
  from: number;
  length: number;
  top: number;
  bottom: number;
  height: number;
}

interface ViewLike {
  coordsAtPos: (pos: number, side?: -1 | 1) => CoordRect | null;
  domAtPos: (pos: number, side?: -1 | 1) => { node: DomNode; offset: number } | null;
  lineBlockAt: (pos: number) => LineBlock;
  defaultCharacterWidth: number;
  state: { doc: ReturnType<typeof Text.of> };
  scrollDOM: { scrollLeft: number; scrollTop: number; getBoundingClientRect(): DOMRect };
  scaleX: number;
  scaleY: number;
}

const CHAR_WIDTH = 9;
const LINE_HEIGHT = 20;
const INK_TOP = 4;
const INK_HEIGHT = 12;

interface Ink {
  top: number;
  bottom: number;
}

function buildView(lines: string[], inkAt: (pos: number) => Ink | null = () => null): ViewLike {
  const doc = Text.of(lines);
  const lineTop = (pos: number) => {
    const line = doc.lineAt(pos);
    return (line.number - 1) * LINE_HEIGHT;
  };
  return {
    defaultCharacterWidth: CHAR_WIDTH,
    lineBlockAt(pos: number): LineBlock {
      const top = lineTop(pos);
      const line = doc.lineAt(pos);
      return { from: line.from, length: line.length, top, bottom: top + LINE_HEIGHT, height: LINE_HEIGHT };
    },
    state: { doc },
    scaleX: 1,
    scaleY: 1,
    scrollDOM: {
      scrollLeft: 0,
      scrollTop: 0,
      getBoundingClientRect: () => ({ left: 0, top: 0, right: 1000, bottom: 1000, width: 1000, height: 1000 }) as DOMRect,
    },
    domAtPos(pos: number): { node: DomNode; offset: number } | null {
      const ink = inkAt(pos);
      if (!ink) return null;
      const col = pos - doc.lineAt(pos).from;
      return {
        node: {
          nodeType: 3,
          textContent: "x",
          ownerDocument: {
            createRange: () => ({
              setStart: () => undefined,
              setEnd: () => undefined,
              getBoundingClientRect: (): CoordRect => ({
                top: ink.top,
                bottom: ink.bottom,
                height: ink.bottom - ink.top,
                left: col * CHAR_WIDTH,
                right: col * CHAR_WIDTH + CHAR_WIDTH,
              }),
            }),
            createTreeWalker: () => ({ nextNode: () => null }),
          },
        },
        offset: 0,
      };
    },
    coordsAtPos(pos: number, side: -1 | 1 = 1): CoordRect {
      const col = pos - doc.lineAt(pos).from;
      const left = col * CHAR_WIDTH;
      const top = lineTop(pos);
      return side === -1 ? { left: left - CHAR_WIDTH, right: left, top, bottom: top + LINE_HEIGHT, height: LINE_HEIGHT } : { left, right: left + CHAR_WIDTH, top, bottom: top + LINE_HEIGHT, height: LINE_HEIGHT };
    },
  };
}

describe("currentStatementFrameRect", () => {
  it("returns null for an empty or invalid range", () => {
    const view = buildView(["SELECT 1"]);
    expect(currentStatementFrameRect(view, 3, 3)).toBeNull();
    expect(currentStatementFrameRect(view, 5, 2)).toBeNull();
    expect(currentStatementFrameRect(view, 0, 99)).toBeNull();
    expect(currentStatementFrameRect(view, 4, 8)).not.toBeNull();
  });

  it("hugs the line box when no DOM ink is reported (fallback path)", () => {
    const view = buildView(["SELECT 1"]);
    const rect = currentStatementFrameRect(view, 0, 8);
    expect(rect).not.toBeNull();
    expect(rect!.left).toBe(0 - FRAME_INSET_PX);
    expect(rect!.width).toBe(8 * CHAR_WIDTH + FRAME_INSET_PX * 2);
    expect(rect!.top).toBe(0 - FRAME_INSET_PX);
    expect(rect!.height).toBe(LINE_HEIGHT + FRAME_INSET_PX * 2);
  });

  it("uses DOM ink bounds (tighter than the line box) when reported", () => {
    const view = buildView(["SELECT 1"], () => ({ top: INK_TOP, bottom: INK_TOP + INK_HEIGHT }));
    const rect = currentStatementFrameRect(view, 0, 8);
    expect(rect).not.toBeNull();
    expect(rect!.top).toBe(INK_TOP - FRAME_INSET_PX);
    expect(rect!.height).toBe(INK_HEIGHT + FRAME_INSET_PX * 2);
    expect(rect!.top).toBeGreaterThan(0 - FRAME_INSET_PX);
  });

  it("spans a multi-line statement as one continuous rectangle", () => {
    const view = buildView(["SELECT a,", "b", "FROM t"]);
    const to = "SELECT a,\nb\nFROM t".length;
    const rect = currentStatementFrameRect(view, 0, to);
    expect(rect).not.toBeNull();
    expect(rect!.top).toBe(0 - FRAME_INSET_PX);
    expect(rect!.height).toBe(3 * LINE_HEIGHT + FRAME_INSET_PX * 2);
    expect(rect!.width).toBe(9 * CHAR_WIDTH + FRAME_INSET_PX * 2);
  });

  it("frames a statement interrupted by a blank line", () => {
    const view = buildView(["SELECT a", "", "FROM t"]);
    const rect = currentStatementFrameRect(view, 0, view.state.doc.length);
    expect(rect).not.toBeNull();
    expect(rect!.top).toBe(0 - FRAME_INSET_PX);
    expect(rect!.height).toBe(3 * LINE_HEIGHT + FRAME_INSET_PX * 2);
  });

  it("bottoms at the last statement line, not the next line", () => {
    const view = buildView(["SELECT 1;", "SELECT 2"], (pos) => {
      if (pos === 9) return null;
      return pos >= 10 ? { top: INK_TOP, bottom: INK_TOP + LINE_HEIGHT } : { top: INK_TOP, bottom: INK_TOP + INK_HEIGHT };
    });
    const rect = currentStatementFrameRect(view, 0, 9);
    expect(rect).not.toBeNull();
    expect(rect!.height).toBe(INK_HEIGHT + FRAME_INSET_PX * 2);
  });
});
