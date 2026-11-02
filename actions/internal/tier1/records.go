// Package tier1 holds the Tier 1 records of docs/actions.md (3 and 4: `_actions` and
// `_checks`), their paths, the guarded transaction the orchestrator decides in, and the
// pail-subprocess reader and writer.
package tier1

import (
	"fmt"
	"strconv"
	"strings"
	"time"
)

// Branches.
const (
	ActionsBranch = "_actions"
	ChecksBranch  = "_checks"
)

// Status and conclusion values (docs/actions.md 2).
const (
	Pending    = "pending"
	Queued     = "queued"
	Waiting    = "waiting"
	InProgress = "in_progress"
	Completed  = "completed"

	Success   = "success"
	Failure   = "failure"
	Cancelled = "cancelled"
	Skipped   = "skipped"
)

type Concurrency struct {
	Group            string `json:"group"`
	CancelInProgress bool   `json:"cancel_in_progress"`
}

// Run is `runs/<id>.json` (3.1). Payload is an addition: the GitHub-shaped event payload, so
// the Tier 2 queue entry can be rebuilt from Tier 1 alone.
type Run struct {
	ID           int            `json:"id"`
	RunNumber    int            `json:"run_number"`
	Workflow     string         `json:"workflow"`
	Name         string         `json:"name"`
	Event        string         `json:"event"`
	HeadSHA      string         `json:"head_sha"`
	HeadBranch   string         `json:"head_branch"`
	Actor        string         `json:"actor"`
	Key          string         `json:"key"`
	RunAttempt   int            `json:"run_attempt"`
	Status       string         `json:"status"`
	Conclusion   *string        `json:"conclusion"`
	Cancel       bool           `json:"cancel"`
	Concurrency  *Concurrency   `json:"concurrency"`
	CreatedAt    string         `json:"created_at"`
	RunStartedAt *string        `json:"run_started_at"`
	UpdatedAt    string         `json:"updated_at"`
	Payload      map[string]any `json:"payload,omitempty"`
}

// Job is `runs/<id>/<att>/jobs/<job>.json` (3.2). JobID is an addition: the workflow's job id
// (`build`), where the file name is the instance (`build (ubuntu-latest, 20)`), so `needs`
// can be resolved to every instance of a needed job.
type Job struct {
	Name        string   `json:"name"`
	JobID       string   `json:"job_id"`
	Status      string   `json:"status"`
	Conclusion  *string  `json:"conclusion"`
	RunsOn      []string `json:"runs_on"`
	Needs       []string `json:"needs"`
	Environment *string  `json:"environment"`
	Holder      string   `json:"holder"`
	Seq         int      `json:"seq"`
	Lost        int      `json:"lost"`
	Accepted    any      `json:"accepted"`
	CopiedFrom  any      `json:"copied_from"`
	StartedAt   *string  `json:"started_at"`
	CompletedAt *string  `json:"completed_at"`
	Steps       []any    `json:"steps"`
	Log         *string  `json:"log"`
}

// Ref is `claims/<key>` and `concurrency/<group>.json`: {"run": id}.
type Ref struct {
	Run int `json:"run"`
}

// Check is `_checks:<sha>/ci.<job>.json` (4).
type Check struct {
	Status     string `json:"status"`
	Conclusion string `json:"conclusion"`
	State      string `json:"state"`
	Workflow   string `json:"workflow"`
	Job        string `json:"job"`
	Run        int    `json:"run"`
	Attempt    int    `json:"attempt"`
	URL        string `json:"url"`
	LogTail    string `json:"log_tail"`
}

// WorkflowSetting is `workflows/<file>.json`.
type WorkflowSetting struct {
	Enabled bool   `json:"enabled"`
	By      string `json:"by,omitempty"`
	At      string `json:"at,omitempty"`
}

func Str(s string) *string { return &s }

// Stamp formats a time as the records do.
func Stamp(t time.Time) string { return t.UTC().Format("2006-01-02T15:04:05Z") }

// Esc makes a name safe as one path part: `/` becomes `~` (docs/actions.md 1, rule 3), and
// anything outside [A-Za-z0-9._@+=-] other than that becomes %XX.
func Esc(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c == '/':
			b.WriteByte('~')
		case c >= 'a' && c <= 'z', c >= 'A' && c <= 'Z', c >= '0' && c <= '9', strings.IndexByte("._@+=-", c) >= 0:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

// Paths.
func RunPath(id int) string { return "runs/" + strconv.Itoa(id) + ".json" }
func JobPath(id, att int, job string) string {
	return fmt.Sprintf("runs/%d/%d/jobs/%s.json", id, att, Esc(job))
}
func JobsDir(id, att int) string { return fmt.Sprintf("runs/%d/%d/jobs/", id, att) }
func ApprovalsDir(id, att int, env string) string {
	return fmt.Sprintf("runs/%d/%d/approvals/%s/", id, att, Esc(env))
}
func ClaimPath(key string) string        { return "claims/" + key }
func GroupPath(group string) string      { return "concurrency/" + Esc(group) + ".json" }
func NumberPath(wf string, n int) string { return fmt.Sprintf("numbers/%s/%d", wf, n) }
func NumbersDir(wf string) string        { return "numbers/" + wf + "/" }
func WorkflowPath(file string) string    { return "workflows/" + file + ".json" }

// CheckPath takes the job as it is named in its file path (already escaped).
func CheckPath(sha, jobKey string) string { return sha + "/ci." + jobKey + ".json" }
