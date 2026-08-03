
export interface VirtualWindow {

  startIndex: number;

  endIndex: number;

  offsetY: number;

  totalHeight: number;
}

export interface WindowInput {

  scrollTop: number;

  viewportHeight: number;

  rowHeight: number;

  count: number;

  overscan?: number;
}


export function computeWindow(input: WindowInput): VirtualWindow {
  const { scrollTop, viewportHeight, rowHeight, count } = input;
  const overscan = input.overscan ?? 4;
  const totalHeight = Math.max(0, count) * rowHeight;

  if (count <= 0 || rowHeight <= 0) {
    return { startIndex: 0, endIndex: 0, offsetY: 0, totalHeight };
  }

  const clampedScroll = Math.min(Math.max(0, scrollTop), totalHeight);
  const firstVisible = Math.floor(clampedScroll / rowHeight);
  const visibleRows = Math.ceil(viewportHeight / rowHeight);

  const startIndex = Math.max(0, firstVisible - overscan);
  const endIndex = Math.min(count, firstVisible + visibleRows + overscan);
  const offsetY = startIndex * rowHeight;

  return { startIndex, endIndex, offsetY, totalHeight };
}


export function isNearEnd(input: WindowInput, thresholdRows = 6): boolean {
  const { scrollTop, viewportHeight, rowHeight, count } = input;
  if (count <= 0 || rowHeight <= 0) return false;
  const lastVisible = Math.ceil((scrollTop + viewportHeight) / rowHeight);
  return lastVisible >= count - thresholdRows;
}
