import type { CheckResult, CheckRollup, CheckState } from "../../shared/types";
import { Icon } from "./Icon";

const LABEL: Record<CheckState | "none", string> = { success: "Checks passed", failure: "Checks failed", pending: "Checks running", none: "No checks" };

/** A small status mark for a commit, branch or PR: icon plus passed/total. Renders nothing without results. */
export function CheckBadge({ rollup, compact }: { rollup?: CheckRollup; compact?: boolean }) {
  if (!rollup || rollup.state === "none") return null;
  const text = `${LABEL[rollup.state]}: ${rollup.passed} of ${rollup.total} passed${rollup.failed ? `, ${rollup.failed} failed` : ""}${rollup.pending ? `, ${rollup.pending} pending` : ""}`;
  return (
    <span className={`check-badge is-${rollup.state}`} title={text} role="img" aria-label={text}>
      <StateIcon state={rollup.state} />
      {!compact && <span className="check-badge__n">{rollup.passed}/{rollup.total}</span>}
    </span>
  );
}

export function StateIcon({ state }: { state: CheckState | "none" | "missing" }) {
  if (state === "success") return <Icon name="check" size={13} />;
  if (state === "failure") return <Icon name="x" size={13} />;
  return <span className="pulse-dot" aria-hidden="true" />;
}

/** One segment per check, so the shape of a run reads at a glance. */
export function CheckStrip({ checks }: { checks: CheckResult[] }) {
  if (!checks.length) return null;
  return (
    <span className="check-strip" aria-hidden="true">
      {checks.map((c) => (
        <i key={c.name} className={`is-${c.state}${c.required ? " req" : ""}`} title={`${c.name}: ${c.state}`} />
      ))}
    </span>
  );
}
