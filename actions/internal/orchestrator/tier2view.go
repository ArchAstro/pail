package orchestrator

import "context"

// DispatchRequest is `requests/dispatch/<uuid>.json` (docs/actions.md 6).
type DispatchRequest struct {
	UUID     string         `json:"-"`
	Workflow string         `json:"workflow"`
	Ref      string         `json:"ref"`
	Inputs   map[string]any `json:"inputs"`
	Actor    string         `json:"actor"`
}

// Tier2View is what the orchestrator reads from, and acknowledges in, the Tier 2 area
// (`<repo>.actions/`). Today it only carries workflow_dispatch requests.
//
// TODO(T1.4): the Tier 2 intents slot in here as further methods and as Passes appended to
// the decider's passes (decide.go), each reading this view from the Decider:
//
//	QueuedEntries() -> sync: keep queue/ equal to the queued jobs; cancel markers for runs with cancel: true
//	Leases()        -> start: queued job whose lease a runner holds becomes in_progress; sweep: expired leases
//	Results()       -> accept: results/<run>/<att>/<job>/<seq> fenced on the job's holder and seq (7.3)
//
// Its writes are not part of the Tier 1 commit; the tick applies them after the commit lands
// (create/delete are idempotent, 5.1).
type Tier2View interface {
	// Dispatches lists the pending dispatch requests.
	Dispatches(ctx context.Context) ([]DispatchRequest, error)
	// Ack deletes a dispatch request once its run exists (its uuid is its claim key, so a
	// crash before Ack cannot fire it twice).
	Ack(ctx context.Context, uuid string) error
}

// NoTier2 is a Tier2View with nothing in it, until the Tier 2 package exists.
type NoTier2 struct{}

func (NoTier2) Dispatches(context.Context) ([]DispatchRequest, error) { return nil, nil }
func (NoTier2) Ack(context.Context, string) error                     { return nil }
