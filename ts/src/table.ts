// Box-drawing table renderer, ported from print_table in src/main.rs.

export type Align = "left" | "right";

function width(s: string): number {
  return [...s].length;
}

export function printTable(
  headers: string[],
  aligns: Align[],
  rows: string[][],
): void {
  const n = headers.length;
  const widths = headers.map(width);
  for (const row of rows) {
    row.forEach((cell, i) => {
      widths[i] = Math.max(widths[i]!, width(cell));
    });
  }

  const rule = (left: string, mid: string, right: string): string => {
    let s = left;
    widths.forEach((w, i) => {
      s += "─".repeat(w + 2);
      s += i + 1 === n ? right : mid;
    });
    return s;
  };

  const printRow = (cells: string[]): void => {
    let s = "│";
    cells.forEach((cell, i) => {
      const pad = widths[i]! - width(cell);
      s +=
        aligns[i] === "left"
          ? ` ${cell}${" ".repeat(pad)} `
          : ` ${" ".repeat(pad)}${cell} `;
      s += "│";
    });
    console.log(s);
  };

  const printSpacer = (): void => {
    let s = "│";
    for (const w of widths) {
      s += " ".repeat(w + 2);
      s += "│";
    }
    console.log(s);
  };

  console.log(rule("┌", "┬", "┐"));
  printRow(headers);
  console.log(rule("├", "┼", "┤"));
  rows.forEach((row, i) => {
    if (i > 0) printSpacer();
    printRow(row);
  });
  console.log(rule("└", "┴", "┘"));
}
