package runner

import (
	"fmt"
	"strconv"
	"strings"
	"sync"

	"github.com/ArchAstro/pail/actions/internal/tier2"
)

// Tracker records step progress of one job. act reports steps by id (the YAML `id`, or the
// step's index when absent); the tracker maps those to the entry's steps.
type Tracker struct {
	mu    sync.Mutex
	steps []tier2.Step
	ids   map[string]int
	dirty bool
}

// NewTracker lists the entry's steps, all pending.
func NewTracker(e tier2.QueueEntry) *Tracker {
	t := &Tracker{ids: map[string]int{}}
	for i, s := range e.Steps {
		t.steps = append(t.steps, tier2.Step{Number: i + 1, Name: stepName(s), Status: tier2.StepPending})
		id, _ := s["id"].(string)
		if id == "" {
			id = strconv.Itoa(i)
		}
		t.ids[id] = i
	}
	return t
}

func stepName(s map[string]any) string {
	if n, _ := s["name"].(string); n != "" {
		return n
	}
	if u, _ := s["uses"].(string); u != "" {
		return "Run " + u
	}
	if r, _ := s["run"].(string); r != "" {
		first, _, _ := strings.Cut(strings.TrimSpace(r), "\n")
		return "Run " + first
	}
	return "step"
}

// Start marks step id in_progress.
func (t *Tracker) Start(id string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if i, ok := t.ids[id]; ok {
		t.steps[i].Status = tier2.StepInProgress
		t.dirty = true
	}
}

// Finish marks step id completed with conclusion (success|failure|skipped|cancelled).
func (t *Tracker) Finish(id, conclusion string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if i, ok := t.ids[id]; ok {
		t.steps[i].Status = tier2.StepCompleted
		t.steps[i].Conclusion = conclusion
		t.dirty = true
	}
}

// Snapshot returns a copy of the steps, and whether they changed since the last Snapshot(true).
func (t *Tracker) Snapshot(clear bool) ([]tier2.Step, bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	d := t.dirty
	if clear {
		t.dirty = false
	}
	return append([]tier2.Step(nil), t.steps...), d
}

// Started reports whether any step began.
func (t *Tracker) Started() bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, s := range t.steps {
		if s.Status != tier2.StepPending {
			return true
		}
	}
	return false
}

// Final closes the books: a step still running ends with `running` (failure or cancelled), a
// step that never ran with `rest` (skipped or cancelled). The result has no Status field.
func (t *Tracker) Final(running, rest string) []tier2.Step {
	t.mu.Lock()
	defer t.mu.Unlock()
	out := make([]tier2.Step, len(t.steps))
	for i, s := range t.steps {
		c := s.Conclusion
		switch s.Status {
		case tier2.StepInProgress:
			c = running
		case tier2.StepPending:
			c = rest
		}
		out[i] = tier2.Step{Number: s.Number, Name: s.Name, Conclusion: c}
	}
	return out
}

func (t *Tracker) String() string {
	s, _ := t.Snapshot(false)
	return fmt.Sprint(s)
}
