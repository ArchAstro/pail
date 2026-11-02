// Package plan turns a workflow file plus an event into the plan the orchestrator would
// queue: which jobs exist, the needs graph, matrix expansion and whether the event triggers
// the workflow. It parses with act's workflow model (github.com/nektos/act/pkg/model) and adds
// what act does not do: trigger filters, matrix naming, plan-time `if`, and the job keys act
// drops (environment, concurrency, continue-on-error, permissions).
package plan

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/nektos/act/pkg/model"
	"gopkg.in/yaml.v3"
)

// Input is the event a plan is made for.
type Input struct {
	Event string   `json:"event"`
	Ref   string   `json:"ref,omitempty"`   // branch or tag; for pull_request the base branch
	Paths []string `json:"paths,omitempty"` // changed files; nil means unknown (path filters are not applied)
	Cron  string   `json:"cron,omitempty"`  // for schedule: the cron entry that fired
	Type  string   `json:"type,omitempty"`  // activity type (opened, labeled, ...); empty means the event's default types
}

// Plan is the JSON the CLI prints.
type Plan struct {
	Workflow    string       `json:"workflow"`
	Name        string       `json:"name,omitempty"`
	Triggers    []string     `json:"triggers"` // events named by `on:`
	Input       Input        `json:"input"`
	Triggered   bool         `json:"triggered"`
	Reason      string       `json:"reason"`
	Concurrency *Concurrency `json:"concurrency,omitempty"`
	Jobs        []Job        `json:"jobs,omitempty"`
	Stages      [][]string   `json:"stages,omitempty"`  // job ids by dependency depth
	Actions     []string     `json:"actions,omitempty"` // every `uses:` step reference, sorted, unique
}

type Concurrency struct {
	Group            string `json:"group"`
	CancelInProgress string `json:"cancel_in_progress,omitempty"`
}

// Job is one job instance: a matrix job appears once per combination.
type Job struct {
	ID            string         `json:"id"` // "build" or "build (ubuntu-latest, 20)"
	JobID         string         `json:"job_id"`
	Name          string         `json:"name,omitempty"`
	Needs         []string       `json:"needs,omitempty"` // job ids (every instance of a needed job)
	RunsOn        []string       `json:"runs_on,omitempty"`
	If            string         `json:"if,omitempty"`
	IfStatic      string         `json:"if_static,omitempty"` // "true"/"false" when decidable at plan time; "deferred" otherwise
	Matrix        map[string]any `json:"matrix,omitempty"`
	MatrixDynamic bool           `json:"matrix_dynamic,omitempty"` // expanded at run time (fromJSON); one placeholder instance
	FailFast      *bool          `json:"fail_fast,omitempty"`
	MaxParallel   int            `json:"max_parallel,omitempty"`
	Timeout       string         `json:"timeout_minutes,omitempty"`
	ContinueOnErr string         `json:"continue_on_error,omitempty"`
	Environment   string         `json:"environment,omitempty"`
	Concurrency   *Concurrency   `json:"concurrency,omitempty"`
	Permissions   string         `json:"permissions,omitempty"`
	Services      []string       `json:"services,omitempty"`
	Container     string         `json:"container,omitempty"`
	Outputs       []string       `json:"outputs,omitempty"`
	Steps         int            `json:"steps,omitempty"`
	Uses          string         `json:"uses,omitempty"` // reusable workflow
	Children      *Plan          `json:"children,omitempty"`
}

// extras holds the keys act's model drops.
type extras struct {
	Concurrency yaml.Node `yaml:"concurrency"`
	Jobs        map[string]struct {
		If            yaml.Node `yaml:"if"`
		Environment   yaml.Node `yaml:"environment"`
		Concurrency   yaml.Node `yaml:"concurrency"`
		ContinueOnErr yaml.Node `yaml:"continue-on-error"`
		Permissions   yaml.Node `yaml:"permissions"`
		Strategy      struct {
			Matrix yaml.Node `yaml:"matrix"`
		} `yaml:"strategy"`
	} `yaml:"jobs"`
}

// Workflow plans one workflow file.
func Workflow(path string, in Input) (*Plan, error) {
	return workflow(path, in, 0)
}

func workflow(path string, in Input, depth int) (*Plan, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	w, err := model.ReadWorkflow(strings.NewReader(string(raw)), false)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	var ex extras
	if err := yaml.Unmarshal(raw, &ex); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	w.File = filepath.Base(path)
	p := &Plan{Workflow: filepath.Base(path), Name: w.Name, Input: in, Triggers: triggers(w)}
	p.Triggered, p.Reason = Matches(w, in)
	p.Concurrency = concurrency(&ex.Concurrency)
	if !p.Triggered {
		return p, nil
	}

	stages, err := stages(w)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	actions := map[string]bool{}
	byID := map[string][]Job{}
	for _, jobID := range w.GetJobIDs() {
		j := w.GetJob(jobID)
		x := ex.Jobs[jobID]
		for _, s := range j.Steps {
			if s.Uses != "" {
				actions[s.Uses] = true
			}
		}
		if j.Uses != "" {
			actions[j.Uses] = true
		}
		// act fills in the job id as the name and success() as the condition.
		name := j.Name
		if name == jobID {
			name = ""
		}
		ms, dynamic, order, err := expand(j, &x.Strategy.Matrix)
		if err != nil {
			return nil, fmt.Errorf("%s: job %s: %w", path, jobID, err)
		}
		for _, m := range ms {
			job := Job{
				ID: instanceID(jobID, m, order), JobID: jobID, Name: name, Needs: j.Needs(), RunsOn: j.RunsOn(),
				If: strings.TrimSpace(x.If.Value), Matrix: m, MatrixDynamic: dynamic, Timeout: j.TimeoutMinutes,
				ContinueOnErr: scalar(&x.ContinueOnErr), Environment: environment(&x.Environment),
				Concurrency: concurrency(&x.Concurrency), Permissions: permissions(&x.Permissions),
				Steps: len(j.Steps), Uses: j.Uses,
			}
			if j.Strategy != nil {
				ff := j.Strategy.GetFailFast()
				job.FailFast = &ff
				if j.Strategy.MaxParallelString != "" {
					job.MaxParallel = j.Strategy.GetMaxParallel()
				}
			}
			if c := j.Container(); c != nil {
				job.Container = c.Image
			}
			for name := range j.Services {
				job.Services = append(job.Services, name)
			}
			sort.Strings(job.Services)
			for name := range j.Outputs {
				job.Outputs = append(job.Outputs, name)
			}
			sort.Strings(job.Outputs)
			job.IfStatic = staticIf(job.If, in, m)
			if strings.HasPrefix(j.Uses, "./") && depth < 4 {
				child, err := workflow(filepath.Join(filepath.Dir(path), filepath.Base(j.Uses)), Input{Event: "workflow_call"}, depth+1)
				if err == nil {
					job.Children = child
				}
			}
			byID[jobID] = append(byID[jobID], job)
		}
	}
	for _, st := range stages {
		var ids []string
		for _, jobID := range st {
			sort.Slice(byID[jobID], func(a, b int) bool { return byID[jobID][a].ID < byID[jobID][b].ID })
			for _, j := range byID[jobID] {
				p.Jobs = append(p.Jobs, j)
				ids = append(ids, j.ID)
			}
		}
		p.Stages = append(p.Stages, ids)
	}
	for a := range actions {
		p.Actions = append(p.Actions, a)
	}
	sort.Strings(p.Actions)
	return p, nil
}

// stages is the needs graph as dependency depths, job ids sorted within a stage. It uses the
// same rule as act's planner (a job runs once all its needs ran) but is deterministic and
// rejects unknown needs by name.
func stages(w *model.Workflow) ([][]string, error) {
	need := map[string][]string{}
	for _, id := range w.GetJobIDs() {
		need[id] = w.GetJob(id).Needs()
		for _, n := range need[id] {
			if w.GetJob(n) == nil {
				return nil, fmt.Errorf("job %q needs unknown job %q", id, n)
			}
		}
	}
	done := map[string]bool{}
	var out [][]string
	for len(done) < len(need) {
		var st []string
		for id, ns := range need {
			if done[id] {
				continue
			}
			ok := true
			for _, n := range ns {
				ok = ok && done[n]
			}
			if ok {
				st = append(st, id)
			}
		}
		if len(st) == 0 {
			return nil, fmt.Errorf("needs cycle among jobs")
		}
		sort.Strings(st)
		for _, id := range st {
			done[id] = true
		}
		out = append(out, st)
	}
	return out, nil
}

func scalar(n *yaml.Node) string {
	if n.Kind == yaml.ScalarNode {
		return n.Value
	}
	return ""
}

func environment(n *yaml.Node) string {
	switch n.Kind {
	case yaml.ScalarNode:
		return n.Value
	case yaml.MappingNode:
		for i := 0; i+1 < len(n.Content); i += 2 {
			if n.Content[i].Value == "name" {
				return n.Content[i+1].Value
			}
		}
	}
	return ""
}

func permissions(n *yaml.Node) string {
	switch n.Kind {
	case yaml.ScalarNode:
		return n.Value
	case yaml.MappingNode:
		var kv []string
		for i := 0; i+1 < len(n.Content); i += 2 {
			kv = append(kv, n.Content[i].Value+":"+n.Content[i+1].Value)
		}
		sort.Strings(kv)
		return strings.Join(kv, ",")
	}
	return ""
}

func concurrency(n *yaml.Node) *Concurrency {
	switch n.Kind {
	case yaml.ScalarNode:
		return &Concurrency{Group: n.Value}
	case yaml.MappingNode:
		c := &Concurrency{}
		for i := 0; i+1 < len(n.Content); i += 2 {
			switch n.Content[i].Value {
			case "group":
				c.Group = n.Content[i+1].Value
			case "cancel-in-progress":
				c.CancelInProgress = n.Content[i+1].Value
			}
		}
		return c
	}
	return nil
}
