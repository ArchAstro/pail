import { useEffect, useState } from "react";

type Theme = "light" | "dark" | "system";

export function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    try { return (localStorage.getItem("pail-theme") as Theme) || "system"; } catch { return "system"; }
  });
  useEffect(() => {
    const el = document.documentElement;
    if (theme === "system") delete el.dataset.theme;
    else el.dataset.theme = theme;
    try { theme === "system" ? localStorage.removeItem("pail-theme") : localStorage.setItem("pail-theme", theme); } catch { /* private mode */ }
  }, [theme]);
  return [theme, setTheme];
}
