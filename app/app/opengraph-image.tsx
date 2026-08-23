import { ImageResponse } from "next/og";

export const alt = "ply — npm for containers";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

export default function OpenGraphImage() {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          justifyContent: "space-between",
          background: "#08100c",
          color: "#e5ece7",
          padding: "68px 76px",
          fontFamily: "sans-serif",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            fontFamily: "monospace",
            fontSize: 24,
          }}
        >
          <div style={{ display: "flex", alignItems: "center" }}>
            <span
              style={{
                display: "flex",
                border: "1px solid #34483d",
                borderRadius: 8,
                padding: "7px 11px",
              }}
            >
              ply
            </span>
            <span style={{ marginLeft: 8, color: "#829087" }}>box</span>
          </div>
          <span style={{ color: "#43d991", fontSize: 18 }}>Linux · x64 + arm64</span>
        </div>

        <div style={{ display: "flex", flexDirection: "column" }}>
          <span
            style={{
              color: "#43d991",
              fontFamily: "monospace",
              fontSize: 20,
              letterSpacing: 3,
              textTransform: "uppercase",
            }}
          >
            npm for containers
          </span>
          <span
            style={{
              marginTop: 24,
              maxWidth: 980,
              fontSize: 76,
              fontWeight: 500,
              lineHeight: 1.04,
              letterSpacing: -4,
            }}
          >
            Ship one file. Run like a process.
          </span>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            borderTop: "1px solid #26382f",
            paddingTop: 26,
            color: "#829087",
            fontFamily: "monospace",
            fontSize: 20,
          }}
        >
          <span style={{ color: "#43d991", marginRight: 14 }}>$</span>
          curl -fsSL https://plybox.sh/install.sh | sh
        </div>
      </div>
    ),
    size,
  );
}
