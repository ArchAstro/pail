import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { useTheme } from "../theme";
import { Icon } from "./Icon";

export function Logo() {
  return (
    <svg className="logo" width="22" height="22" viewBox="0 0 32 32" aria-hidden="true">
      <path d="M6 11h20l-2.4 15a2 2 0 0 1-2 1.7H10.4a2 2 0 0 1-2-1.7z" fill="currentColor" />
      <path d="M9 11a7 7 0 0 1 14 0" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <path d="M8.4 17c2.4-1.6 4.8 1.6 7.6 0s5.2 1.6 7.6 0" stroke="var(--bg)" strokeWidth="1.8" fill="none" strokeLinecap="round" />
    </svg>
  );
}

export function Shell({ children }: { children: ReactNode }) {
  return (
    <>
      <a className="skip" href="#main">Skip to content</a>
      <div id="shell">{children}</div>
    </>
  );
}

export function TopBar({ crumb, onSearch, live }: { crumb?: ReactNode; onSearch?: () => void; live?: boolean }) {
  const [theme, setTheme] = useTheme();
  const next = theme === "dark" ? "light" : theme === "light" ? "system" : "dark";
  return (
    <header className="top">
      <Link to="/" className="brand" aria-label="pail, all repositories">
        <Logo />
        <span>pail</span>
      </Link>
      {crumb && <span className="top__crumb">{crumb}</span>}
      <span className="top__fill" />
      {live !== undefined && (
        <span className={`live ${live ? "is-on" : ""}`} title={live ? "Live: updates arrive as branches move" : "Not connected to live updates"}>
          <i /> {live ? "live" : "offline"}
        </span>
      )}
      {onSearch && (
        <button className="btn btn--search" onClick={onSearch} aria-label="Search (press /)">
          <Icon name="search" size={14} />
          <span className="btn__text">Search</span>
          <kbd>/</kbd>
        </button>
      )}
      <button className="icon-btn" onClick={() => setTheme(next)} aria-label={`Theme: ${theme}. Switch to ${next}`} title={`Theme: ${theme}`}>
        <Icon name={theme === "dark" ? "moon" : "sun"} />
      </button>
    </header>
  );
}
