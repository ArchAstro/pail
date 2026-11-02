package orchestrator

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/ArchAstro/pail/actions/internal/tier1"
)

// LoadChecks says which `_checks` files a tick needs the content of.
func LoadChecks(path string) bool { return strings.HasSuffix(path, ".json") }

// checkState is the legacy `state` field `pail pr merge` reads: skipped counts as passing,
// every other conclusion that is not success fails, a job not concluded is pending.
func checkState(status string, conclusion *string) string {
	if status != tier1.Completed || conclusion == nil {
		return "pending"
	}
	switch *conclusion {
	case tier1.Success, tier1.Skipped:
		return "success"
	}
	return "failure"
}

// DecideChecks projects every completed job of every run's current attempt to
// `_checks:<sha>/ci.<job>.json` (docs/actions.md 4) and returns the transaction on `_checks`
// holding the ones that differ from what is there ("reconcile": a missed or lagging
// projection is repaired by the next tick). The source is the accepted job in `_actions`,
// never Tier 2. `_checks` is another ref, so the run cannot be guarded atomically; a stale
// projection is overwritten by the next tick.
func DecideChecks(actions *tier1.Snapshot, checks *tier1.Snapshot, now time.Time) (*tier1.Tx, []string) {
	d := &Decider{Snap: actions, Tx: tier1.NewTx(actions), In: Input{Now: now}, runs: map[int]*runView{}, numbers: map[string]int{}, done: map[string]bool{}}
	d.load()
	tx := tier1.NewTx(checks)
	var intents []string
	for _, id := range d.runIDs() {
		rv := d.runs[id]
		r := rv.Run
		if r.HeadSHA == "" {
			continue
		}
		jobs := rv.Atts[r.RunAttempt]
		for _, key := range sortedKeys(jobs) {
			j := jobs[key]
			if j.Status != tier1.Completed {
				continue
			}
			want := tier1.Check{
				Status: tier1.Completed, Conclusion: *orEmpty(j.Conclusion), State: checkState(j.Status, j.Conclusion),
				Workflow: r.Name, Job: j.Name, Run: r.ID, Attempt: r.RunAttempt,
				URL: fmt.Sprintf("/actions/runs/%d", r.ID),
			}
			path := tier1.CheckPath(r.HeadSHA, key)
			if b, ok := tx.Get(path); ok {
				var have tier1.Check
				if json.Unmarshal(b, &have) == nil {
					want.LogTail = have.LogTail // not ours to flap on
					if have == want {
						continue
					}
				}
			}
			tx.Write(path, marshal(want))
			intents = append(intents, fmt.Sprintf("check %s <- run %d job %s: %s", r.HeadSHA[:min(12, len(r.HeadSHA))], r.ID, key, want.State))
		}
	}
	return tx, intents
}

func orEmpty(s *string) *string {
	if s == nil {
		e := ""
		return &e
	}
	return s
}
