export function isUnicodeScalarString(value: string) {
  return !Array.from(value).some((char) => {
    const point = char.codePointAt(0)!;
    return point >= 0xd800 && point <= 0xdfff;
  });
}

export function evidencePreview(value: string, maxBytes = 500) {
  let output = "";
  let used = 0;
  let truncated = false;
  for (const char of value) {
    const point = char.codePointAt(0)!;
    const escaped = char === "\\" ? "\\\\" : /[\p{Cc}\p{Cf}]/u.test(char) ? `\\u{${point.toString(16)}}` : char;
    const bytes = new TextEncoder().encode(escaped).byteLength;
    if (used + bytes > maxBytes) { truncated = true; break; }
    output += escaped;
    used += bytes;
  }
  return truncated ? `${output}…` : output;
}

export function bytePreview(value: string, maxBytes: number) {
  const bytes = new TextEncoder().encode(value);
  if (bytes.byteLength <= maxBytes) return { text: value, truncated: false, bytes: bytes.byteLength };
  let end = Math.min(maxBytes, bytes.byteLength);
  const decoder = new TextDecoder("utf-8", { fatal: true });
  while (end > 0) {
    try { return { text: decoder.decode(bytes.slice(0, end)), truncated: true, bytes: bytes.byteLength }; }
    catch { end -= 1; }
  }
  return { text: "", truncated: true, bytes: bytes.byteLength };
}
