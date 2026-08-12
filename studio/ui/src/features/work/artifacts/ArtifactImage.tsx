import { useEffect, useRef } from "react";

export function ArtifactImage({ ppmHex, alt }: { ppmHex: string; alt: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const bytes = new Uint8Array(ppmHex.match(/.{1,2}/g)!.map((hex) => parseInt(hex, 16)));
    let cursor = 0;
    function readLine() {
      let line = "";
      while (cursor < bytes.length) {
        const byte = bytes[cursor++];
        if (byte === 10) break;
        line += String.fromCharCode(byte);
      }
      return line;
    }
    // Skip comments and read header
    let magic = "";
    while (cursor < bytes.length) {
      const line = readLine();
      if (line.startsWith("#")) continue;
      magic = line;
      break;
    }
    if (magic !== "P6") return;
    let dims = "";
    while (cursor < bytes.length) {
      const line = readLine();
      if (line.startsWith("#")) continue;
      dims = line;
      break;
    }
    const [width, height] = dims.split(/\s+/).map(Number);
    if (!width || !height) return;
    let maxVal = "";
    while (cursor < bytes.length) {
      const line = readLine();
      if (line.startsWith("#")) continue;
      maxVal = line;
      break;
    }
    if (!maxVal) return;
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const image = ctx.createImageData(width, height);
    const data = image.data;
    for (let i = 0; i < width * height; i++) {
      const r = bytes[cursor + i * 3];
      const g = bytes[cursor + i * 3 + 1];
      const b = bytes[cursor + i * 3 + 2];
      data[i * 4] = r;
      data[i * 4 + 1] = g;
      data[i * 4 + 2] = b;
      data[i * 4 + 3] = 255;
    }
    ctx.putImageData(image, 0, 0);
  }, [ppmHex]);
  return <canvas ref={ref} aria-label={alt} style={{ maxWidth: "100%", height: "auto" }} />;
}
