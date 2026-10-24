import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vite-plus/test";

import { Sparkline, sparkShape } from "./Sparkline.tsx";

afterEach(cleanup);

describe("sparkShape", () => {
  it("joins points a step apart, scaled to the window and the max", () => {
    const shape = sparkShape(
      [
        { t: 0, v: 0 },
        { t: 60, v: 50 },
        { t: 120, v: 100 },
      ],
      60,
      0,
      120,
      100,
    );
    expect(shape).toEqual({ d: "M0 30 L50 15 L100 0", dots: [] });
  });

  it("breaks the line at missing values and long gaps instead of drawing zeros", () => {
    const shape = sparkShape(
      [
        { t: 0, v: 10 },
        { t: 60, v: 10 },
        { t: 120, v: null },
        { t: 180, v: 10 },
        { t: 600, v: 10 },
        { t: 660, v: 10 },
      ],
      60,
      0,
      660,
      10,
    );
    expect(shape.d).toBe("M0 0 L9.09 0 M90.91 0 L100 0");
    expect(shape.dots).toEqual([{ x: 27.27, y: 0 }]);
  });

  it("scales to the largest value without a max, and handles no data", () => {
    expect(
      sparkShape(
        [
          { t: 0, v: 2 },
          { t: 10, v: 4 },
        ],
        10,
        0,
        10,
      ).d,
    ).toBe("M0 15 L100 0");
    expect(sparkShape([], 60, 0, 60)).toEqual({ d: "", dots: [] });
  });
});

describe("Sparkline", () => {
  it("is an image named for screen readers", () => {
    render(
      <Sparkline
        points={[
          { t: 0, v: 1 },
          { t: 60, v: 2 },
        ]}
        step={60}
        from={0}
        to={60}
        label="CPU over the last hour"
      />,
    );
    const chart = screen.getByRole("img", { name: "CPU over the last hour" });
    expect(chart.querySelector("path")?.getAttribute("d")).toBe("M0 15 L100 0");
  });
});
