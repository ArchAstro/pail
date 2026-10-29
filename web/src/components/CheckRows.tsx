import { useState } from "react";
import type { CheckResult } from "../../shared/types";
import { duration } from "../lib/format";
import { CheckStrip, StateIcon } from "./Checks";
import { Icon } from "./Icon";

const WORD = { success: "Passed", failure: "Failed", pending: "Pending" } as const;

export function CheckRows({ checks, empty }: { checks: CheckResult[]; empty?: string }) {
  if (!checks.length) return <p className="muted pad">{empty ?? "No checks have run for this commit."}</p>;
  return (
    <div className="checks">
      <div className="checks__strip"><CheckStrip checks={checks} /></div>
      <ul className="checks__list">
        {checks.map((c) => <CheckRow key={c.name} check={c} />)}
      </ul>
    </div>
  );
}

function CheckRow({ check: c }: { check: CheckResult }) {
  const [open, setOpen] = useState(c.state === "failure");
  const hasDetail = Boolean(c.log_tail || c.command);
  return (
    <li className={`check is-${c.state}`}>
      <button className="check__head" aria-expanded={open} disabled={!hasDetail} onClick={() => setOpen(!open)}>
        <span className="check__icon"><StateIcon state={c.state} /></span>
        <span className="check__name">{c.name}</span>
        <span className={`pill pill--${c.required ? "req" : "quiet"}`}>{c.required ? "required" : "optional"}</span>
        <span className="check__state">{c.recorded === false ? "Not run yet" : WORD[c.state]}</span>
        {c.duration_s !== undefined && <span className="check__time"><Icon name="clock" size={13} />{duration(c.duration_s)}</span>}
        {hasDetail && <Icon name="chevron" size={14} className={open ? "rot" : ""} />}
      </button>
      {open && hasDetail && (
        <div className="check__detail">
          {c.command && <p className="check__cmd"><span className="muted">ran</span> <code>{c.command}</code>{c.runner ? <span className="muted"> on {c.runner}</span> : null}{typeof c.exit === "number" && c.exit !== 0 ? <span className="del"> exit {c.exit as number}</span> : null}</p>}
          {c.log_tail && <pre className="check__log" tabIndex={0} aria-label={`Log tail for ${c.name}`}>{c.log_tail}</pre>}
        </div>
      )}
    </li>
  );
}
