// A monogram on a colour derived from the name: no network, nothing to load.
export function Avatar({ name, size = 22 }: { name: string; size?: number }) {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const hue = h % 360;
  return (
    <span className="avatar" style={{ width: size, height: size, fontSize: size * 0.46, background: `oklch(0.55 0.09 ${hue})` }} aria-hidden="true">
      {(name.trim()[0] ?? "?").toUpperCase()}
    </span>
  );
}
