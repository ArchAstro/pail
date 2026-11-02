// Package orchestrator is the Tier 1 half of the pail Actions orchestrator (docs/actions.md
// 7): it maps events to triggers, decides every due intent over a snapshot of `_actions`
// (a pure function), and lands the decisions as one guarded `pail put`.
package orchestrator

import (
	"bytes"
	"encoding/json"
	"fmt"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/ArchAstro/pail/actions/internal/plan"
	"github.com/ArchAstro/pail/actions/internal/tier1"
)

// Trigger is one event key turned into a planned workflow: "create a run for this key unless
// its claim exists". The mapper makes them; Decide never reads a workflow file.
type Trigger struct {
	Key      string // the claim key (docs/actions.md 3.3)
	Workflow string // file name, ci.yml
	Event    string
	SHA      string
	Branch   string // head branch
	Actor    string
	Plan     *plan.Plan
	Payload  map[string]any
	Vars     map[string]string // github.* values for `concurrency.group` expressions
}

// Input is everything a decision needs besides the snapshot.
type Input struct {
	Now      time.Time
	Triggers []Trigger
}

// Decision is what a tick decided.
type Decision struct {
	Tx      *tier1.Tx
	Done    []string // trigger keys that need no further attention (created now, or already claimed)
	Intents []string // one line per decided intent, for the log
}

// Pass decides one kind of intent over the decider's view and writes into its transaction.
// Tier 2 intents (sync, start, accept, sweep; T1.4) are further passes.
type Pass func(*Decider)

// DefaultPasses are the Tier 1 intents, in order: trigger, promote, finish. A trigger's
// cancelled jobs are concluded and its run finished in the same commit when nothing of it
// is running.
var DefaultPasses = []Pass{(*Decider).passTriggers, (*Decider).passPromote, (*Decider).passFinish}

// Decider is the working state of one decision: the snapshot plus this transaction's writes.
type Decider struct {
	Snap *tier1.Snapshot
	Tx   *tier1.Tx
	In   Input

	runs    map[int]*runView
	nextID  int
	numbers map[string]int
	done    map[string]bool
	intents []string
}

type runView struct {
	Run  tier1.Run
	Atts map[int]map[string]*tier1.Job // attempt -> job file key (escaped instance id) -> job
}

var (
	runPathRE = regexp.MustCompile(`^runs/(\d+)\.json$`)
	jobPathRE = regexp.MustCompile(`^runs/(\d+)/(\d+)/jobs/(.+)\.json$`)
)

// LoadActions says which `_actions` files a tick needs the content of.
func LoadActions(path string) bool {
	return runPathRE.MatchString(path) || jobPathRE.MatchString(path) ||
		strings.HasPrefix(path, "workflows/") || strings.HasPrefix(path, "concurrency/")
}

// Decide runs the passes over a snapshot of `_actions`.
func Decide(snap *tier1.Snapshot, in Input, passes ...Pass) Decision {
	if len(passes) == 0 {
		passes = DefaultPasses
	}
	d := &Decider{Snap: snap, Tx: tier1.NewTx(snap), In: in, runs: map[int]*runView{}, numbers: map[string]int{}, done: map[string]bool{}}
	d.load()
	for _, p := range passes {
		p(d)
	}
	var done []string
	for k := range d.done {
		done = append(done, k)
	}
	sort.Strings(done)
	return Decision{Tx: d.Tx, Done: done, Intents: d.intents}
}

func (d *Decider) logf(format string, a ...any) {
	d.intents = append(d.intents, fmt.Sprintf(format, a...))
}

func (d *Decider) load() {
	for _, path := range d.Snap.Under("runs/") {
		f := d.Snap.Files[path]
		if m := runPathRE.FindStringSubmatch(path); m != nil {
			id, _ := strconv.Atoi(m[1])
			if id > d.nextID {
				d.nextID = id
			}
			var r tier1.Run
			if f.Data == nil || json.Unmarshal(f.Data, &r) != nil {
				continue
			}
			rv := d.view(id)
			rv.Run = r
		} else if m := jobPathRE.FindStringSubmatch(path); m != nil {
			id, _ := strconv.Atoi(m[1])
			att, _ := strconv.Atoi(m[2])
			var j tier1.Job
			if f.Data == nil || json.Unmarshal(f.Data, &j) != nil {
				continue
			}
			rv := d.view(id)
			if rv.Atts[att] == nil {
				rv.Atts[att] = map[string]*tier1.Job{}
			}
			rv.Atts[att][m[3]] = &j
		}
	}
	for id, rv := range d.runs {
		if rv.Run.ID == 0 { // jobs without a readable run file
			delete(d.runs, id)
		}
	}
	for _, path := range d.Snap.Under("numbers/") {
		parts := strings.Split(path, "/")
		if len(parts) == 3 {
			if n, err := strconv.Atoi(parts[2]); err == nil && n > d.numbers[parts[1]] {
				d.numbers[parts[1]] = n
			}
		}
	}
}

func (d *Decider) view(id int) *runView {
	rv := d.runs[id]
	if rv == nil {
		rv = &runView{Atts: map[int]map[string]*tier1.Job{}}
		d.runs[id] = rv
	}
	return rv
}

func (d *Decider) runIDs() []int {
	var ids []int
	for id := range d.runs {
		ids = append(ids, id)
	}
	sort.Ints(ids)
	return ids
}

func marshal(v any) []byte {
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	_ = enc.Encode(v)
	return b.Bytes()
}

func (d *Decider) putRun(r tier1.Run) {
	r.UpdatedAt = tier1.Stamp(d.In.Now)
	d.view(r.ID).Run = r
	d.Tx.Write(tier1.RunPath(r.ID), marshal(r))
}

func (d *Decider) putJob(id, att int, key string, j *tier1.Job) {
	rv := d.view(id)
	if rv.Atts[att] == nil {
		rv.Atts[att] = map[string]*tier1.Job{}
	}
	rv.Atts[att][key] = j
	d.Tx.Write(fmt.Sprintf("runs/%d/%d/jobs/%s.json", id, att, key), marshal(j))
}

func (d *Decider) workflowEnabled(file string) bool {
	if b, ok := d.Tx.Get(tier1.WorkflowPath(file)); ok {
		var w tier1.WorkflowSetting
		if json.Unmarshal(b, &w) == nil {
			return w.Enabled
		}
	}
	return true
}

// ---------- trigger ----------

func (d *Decider) passTriggers() {
	for _, t := range d.In.Triggers {
		d.trigger(t)
	}
}

func runName(t Trigger) string {
	if t.Plan != nil && t.Plan.Name != "" {
		return t.Plan.Name
	}
	return strings.TrimSuffix(strings.TrimSuffix(t.Workflow, ".yml"), ".yaml")
}

func (d *Decider) trigger(t Trigger) {
	claim := tier1.ClaimPath(t.Key)
	if d.Tx.Exists(claim) { // one trigger, one run: across replicas and restarts
		d.done[t.Key] = true
		return
	}
	if t.Plan == nil || !t.Plan.Triggered || len(t.Plan.Jobs) == 0 || !d.workflowEnabled(t.Workflow) {
		d.done[t.Key] = true
		return
	}
	now := tier1.Stamp(d.In.Now)
	d.nextID++
	id := d.nextID
	d.numbers[t.Workflow]++
	num := d.numbers[t.Workflow]

	run := tier1.Run{
		ID: id, RunNumber: num, Workflow: ".github/workflows/" + t.Workflow, Name: runName(t),
		Event: t.Event, HeadSHA: t.SHA, HeadBranch: t.Branch, Actor: t.Actor, Key: t.Key,
		RunAttempt: 1, Status: tier1.Queued, CreatedAt: now, Payload: t.Payload,
	}
	group := ""
	cancel := false
	if c := t.Plan.Concurrency; c != nil && c.Group != "" {
		group = expand(c.Group, t.Vars)
		cancel = strings.EqualFold(strings.TrimSpace(expand(c.CancelInProgress, t.Vars)), "true")
		run.Concurrency = &tier1.Concurrency{Group: group, CancelInProgress: cancel}
	}
	d.putRun(run)
	d.Tx.Write(claim, marshal(tier1.Ref{Run: id}))
	d.Tx.Write(tier1.NumberPath(t.Workflow, num), marshal(tier1.Ref{Run: id}))
	for _, pj := range t.Plan.Jobs {
		d.putJob(id, 1, tier1.Esc(pj.ID), newJob(pj, now))
	}
	d.logf("trigger %s -> run %d (#%d, %d jobs)", t.Key, id, num, len(t.Plan.Jobs))

	if group != "" {
		gp := tier1.GroupPath(group)
		if b, ok := d.Tx.Read(gp); ok { // guarded even when the group is not overwritten
			var prev tier1.Ref
			if json.Unmarshal(b, &prev) == nil && prev.Run != 0 && prev.Run != id {
				d.Tx.Read(tier1.RunPath(prev.Run)) // the previous holder: done or not, a rerun landing meanwhile must be seen
				if cancel {
					d.cancelRun(prev.Run)
				}
			}
		}
		d.Tx.Write(gp, marshal(tier1.Ref{Run: id}))
	}
	d.done[t.Key] = true
}

// newJob is a job's first state: concluded if its `if` is statically false, queued if nothing
// holds it back, else pending (docs/actions.md 2; the model's freshJob).
func newJob(pj plan.Job, now string) *tier1.Job {
	j := &tier1.Job{
		Name: pj.Name, JobID: pj.JobID, Status: tier1.Pending, RunsOn: pj.RunsOn, Needs: pj.Needs,
		Steps: []any{},
	}
	if j.Name == "" {
		j.Name = pj.ID
	}
	if j.RunsOn == nil {
		j.RunsOn = []string{}
	}
	if j.Needs == nil {
		j.Needs = []string{}
	}
	if pj.Environment != "" {
		j.Environment = tier1.Str(pj.Environment)
	}
	switch {
	case pj.IfStatic == "false":
		j.Status, j.Conclusion, j.CompletedAt = tier1.Completed, tier1.Str(tier1.Skipped), &now
	case len(pj.Needs) == 0 && pj.Environment == "":
		j.Status = tier1.Queued
	}
	return j
}

// cancelRun makes a run cancelled: nothing of it may start; its jobs not yet started are
// concluded cancelled (a running one is left to its runner and the Tier 2 marker). It reads,
// and so guards, every job of the attempt.
func (d *Decider) cancelRun(id int) {
	rv := d.runs[id]
	if rv == nil {
		return
	}
	for _, key := range sortedKeys(rv.Atts[rv.Run.RunAttempt]) {
		d.Tx.Read(fmt.Sprintf("runs/%d/%d/jobs/%s.json", id, rv.Run.RunAttempt, key))
	}
	if rv.Run.Status == tier1.Completed || rv.Run.Cancel {
		return
	}
	now := tier1.Stamp(d.In.Now)
	for _, key := range sortedKeys(rv.Atts[rv.Run.RunAttempt]) {
		j := *rv.Atts[rv.Run.RunAttempt][key]
		switch j.Status {
		case tier1.Pending, tier1.Queued, tier1.Waiting:
			j.Status, j.Conclusion, j.CompletedAt = tier1.Completed, tier1.Str(tier1.Cancelled), &now
			d.putJob(id, rv.Run.RunAttempt, key, &j)
		}
	}
	r := rv.Run
	r.Cancel = true
	d.putRun(r)
	d.logf("cancel run %d (superseded in its concurrency group)", id)
}

func sortedKeys(m map[string]*tier1.Job) []string {
	var ks []string
	for k := range m {
		ks = append(ks, k)
	}
	sort.Strings(ks)
	return ks
}

var exprRE = regexp.MustCompile(`\$\{\{\s*([^}]*?)\s*\}\}`)

// expand substitutes `${{ github.x }}` references from vars; anything unknown becomes empty.
// (The planner leaves concurrency expressions alone; this is the narrow subset a group needs.)
func expand(s string, vars map[string]string) string {
	return exprRE.ReplaceAllStringFunc(s, func(m string) string {
		return vars[strings.TrimSpace(exprRE.FindStringSubmatch(m)[1])]
	})
}

// ---------- promote ----------

func actorName(a string) string { return tier1.Esc(strings.TrimPrefix(a, "user:")) }

func (d *Decider) approved(rv *runView, env string) bool {
	for _, p := range d.Snap.Under(tier1.ApprovalsDir(rv.Run.ID, rv.Run.RunAttempt, env)) {
		who := strings.TrimSuffix(p[strings.LastIndex(p, "/")+1:], ".json")
		if who != actorName(rv.Run.Actor) {
			return true
		}
	}
	return false
}

// passPromote: a pending job whose needs all succeeded becomes queued (waiting if its
// environment lacks an approval from someone other than the actor); one whose need did not
// succeed is skipped (docs/actions.md 7.2; the model's decidePromote).
func (d *Decider) passPromote() {
	for _, id := range d.runIDs() {
		rv := d.runs[id]
		if rv.Run.Status == tier1.Completed {
			continue
		}
		att := rv.Run.RunAttempt
		jobs := rv.Atts[att]
		now := tier1.Stamp(d.In.Now)
		for _, key := range sortedKeys(jobs) {
			j := *jobs[key]
			if j.Status != tier1.Pending && j.Status != tier1.Waiting {
				continue
			}
			met, failed := true, false
			for _, need := range j.Needs {
				found := false
				for _, nj := range jobs {
					if nj.JobID != need {
						continue
					}
					found = true
					if nj.Status != tier1.Completed {
						met = false
					} else if nj.Conclusion == nil || *nj.Conclusion != tier1.Success {
						failed, met = true, false
					}
				}
				met = met && found
			}
			switch {
			case failed && j.Status == tier1.Pending:
				j.Status, j.Conclusion, j.CompletedAt = tier1.Completed, tier1.Str(tier1.Skipped), &now
				d.logf("promote run %d job %s -> skipped (a need did not succeed)", id, key)
			case !met:
				continue
			default:
				target := tier1.Queued
				if j.Environment != nil && !d.approved(rv, *j.Environment) {
					target = tier1.Waiting
				}
				if j.Status == target {
					continue
				}
				j.Status = target
				d.logf("promote run %d job %s -> %s", id, key, target)
			}
			d.putJob(id, att, key, &j)
		}
	}
}

// ---------- finish ----------

func (d *Decider) passFinish() {
	for _, id := range d.runIDs() {
		rv := d.runs[id]
		r := rv.Run
		if r.Status == tier1.Completed {
			continue
		}
		jobs := rv.Atts[r.RunAttempt]
		all, anyFailed, running := len(jobs) > 0, false, false
		for _, j := range jobs {
			if j.Status != tier1.Completed {
				all = false
			}
			if j.Status == tier1.InProgress {
				running = true
			}
			if j.Conclusion != nil && (*j.Conclusion == tier1.Failure || *j.Conclusion == "timed_out") {
				anyFailed = true
			}
		}
		now := tier1.Stamp(d.In.Now)
		switch {
		case all:
			concl := tier1.Success
			if r.Cancel {
				concl = tier1.Cancelled
			} else if anyFailed {
				concl = tier1.Failure
			}
			r.Status, r.Conclusion = tier1.Completed, tier1.Str(concl)
			d.logf("finish run %d -> %s", id, concl)
		case running && r.Status != tier1.InProgress:
			r.Status = tier1.InProgress
			if r.RunStartedAt == nil {
				r.RunStartedAt = &now
			}
			d.logf("run %d in progress", id)
		default:
			continue
		}
		d.putRun(r)
	}
}
