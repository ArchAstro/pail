// Package tier2 is the Go view of the Tier 2 objects of docs/actions.md section 5: the
// records, their key paths, and a Store interface over the `<repo>.actions/` area of a
// bucket. The orchestrator and the runner both import it, so the key layout lives here once.
//
// Keys are relative to the area (`queue/ubuntu-latest/4182.1.test.json`); the Store adds the
// `<repo>.actions/` prefix.
package tier2

import "time"

// Job and result conclusions a runner reports (section 5.3).
const (
	Success   = "success"
	Failure   = "failure"
	Cancelled = "cancelled"
)

// Step statuses in Progress and Result.
const (
	StepPending    = "pending"
	StepInProgress = "in_progress"
	StepCompleted  = "completed"
)

// QueueEntry is the job message of section 5.1: everything a runner needs, no Tier 1 reads.
// The fields after Secrets (RunsOn, Name, Container, Services, Defaults) are additions to the
// section 5.1 example, carrying the job keys act needs that are not steps.
type QueueEntry struct {
	Run            int64             `json:"run"`
	Attempt        int               `json:"attempt"`
	Job            string            `json:"job"`
	Store          string            `json:"store"`
	Sha            string            `json:"sha"`
	Github         Github            `json:"github"`
	Steps          []map[string]any  `json:"steps"`
	Env            map[string]string `json:"env,omitempty"`
	TimeoutMinutes int               `json:"timeout_minutes,omitempty"`
	Secrets        []SecretRef       `json:"secrets,omitempty"`

	RunsOn    []string       `json:"runs_on,omitempty"`   // default: the label the entry was queued under
	Name      string         `json:"name,omitempty"`      // display name of the job
	Container any            `json:"container,omitempty"` // job container: image string or map, as in YAML
	Services  map[string]any `json:"services,omitempty"`  // service containers, as in YAML
	Defaults  map[string]any `json:"defaults,omitempty"`  // job `defaults:` (run.shell, run.working-directory)
}

// Github is the subset of the github context the orchestrator knows. The runner sets act's
// context from it, never from a local git repository.
type Github struct {
	EventName  string `json:"event_name"`
	Ref        string `json:"ref"`
	Repository string `json:"repository"`
	RunID      int64  `json:"run_id"`
	RunNumber  int64  `json:"run_number,omitempty"`
	Actor      string `json:"actor,omitempty"`
	HeadRef    string `json:"head_ref,omitempty"`
	BaseRef    string `json:"base_ref,omitempty"`
}

// SecretRef names a secret by reference (section 8.4). Values never enter the bucket.
type SecretRef struct {
	Name string `json:"name"`
	Ref  string `json:"ref"`
}

// Lease is leases/<run>/<att>/<job> (section 5.2).
type Lease struct {
	Holder    string    `json:"holder"`
	ExpiresAt time.Time `json:"expires_at"`
	Seq       int       `json:"seq"`
}

// Expired reports whether the lease no longer protects its holder at now.
func (l Lease) Expired(now time.Time) bool { return !l.ExpiresAt.After(now) }

// Step is one step in Progress and Result.
type Step struct {
	Number     int    `json:"number"`
	Name       string `json:"name"`
	Status     string `json:"status,omitempty"`     // progress only
	Conclusion string `json:"conclusion,omitempty"` // success|failure|skipped|cancelled once completed
}

// Progress is progress/<run>/<att>/<job>/<seq>.json. Its existence with a step in_progress or
// later means "a step started under seq".
type Progress struct {
	Holder    string    `json:"holder"`
	Seq       int       `json:"seq"`
	Steps     []Step    `json:"steps"`
	UpdatedAt time.Time `json:"updated_at"`
}

// Result is results/<run>/<att>/<job>/<seq> (section 5.3): a claim, accepted only by the
// orchestrator's fence.
type Result struct {
	Runner     string            `json:"runner"`
	Seq        int               `json:"seq"`
	Conclusion string            `json:"conclusion"`
	Outputs    map[string]string `json:"outputs"`
	Steps      []Step            `json:"steps"`
	Log        string            `json:"log"`
	FinishedAt time.Time         `json:"finished_at"`
}

// RunnerInfo is runners/<id>.json.
type RunnerInfo struct {
	Labels   []string  `json:"labels"`
	Version  string    `json:"version"`
	LastSeen time.Time `json:"last_seen"`
}

// Cancel is the (free-form) body of cancel/<run>/<att>/<job>; only its existence matters.
type Cancel struct {
	At time.Time `json:"at,omitempty"`
}
