package orchestrator

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"

	"github.com/ArchAstro/pail/actions/internal/plan"
	"github.com/ArchAstro/pail/actions/internal/tier1"
)

// RefEvent is a line of `pail events` for a ref: {"branch","commit","old","source","at"}.
// (An object line, {"key","version","old","op",...}, has no branch; it only wakes a tick.)
type RefEvent struct {
	Branch string  `json:"branch"`
	Commit *string `json:"commit"`
	Old    *string `json:"old"`
	Source string  `json:"source"`
	Key    string  `json:"key"`

	tries int
}

// ParseEvent reads one line.
func ParseEvent(line []byte) (RefEvent, error) {
	var e RefEvent
	err := json.Unmarshal(line, &e)
	return e, err
}

// IsRef is true for a ref change (as opposed to an object of the Tier 2 area).
func (e RefEvent) IsRef() bool { return e.Branch != "" }

// Git is what the mapper reads from the repository. *tier1.Pail implements it.
type Git interface {
	Tip(ctx context.Context, branch string) (string, bool, error)
	Tree(ctx context.Context, rev string) (map[string]string, error)
	Blobs(ctx context.Context, commit string, want map[string]string) (map[string][]byte, error)
	Changed(ctx context.Context, from, to string) ([]string, error)
	Author(ctx context.Context, commit string) (string, error)
}

// PRRecord is `_meta:prs/<n>.json` (docs/extending.md); only what triggers need.
type PRRecord struct {
	ID     int    `json:"id"`
	Title  string `json:"title"`
	Head   string `json:"head"`
	Base   string `json:"base"`
	Author string `json:"author"`
	State  string `json:"state"` // open, closed, merged (merging while a merge lands)
	Draft  bool   `json:"draft"`
}

func (p PRRecord) open() bool { return p.State == "open" || p.State == "merging" }

// Mapper turns ref changes into triggers: push, pull_request and workflow_dispatch events,
// with GitHub-shaped payloads, planned against the workflows of the right commit.
type Mapper struct {
	Git     Git
	Default string // the default branch, whose workflows run for untrusted writers; default "main"
	Repo    string // repository name for payloads
	Log     func(format string, a ...any)
}

func (m *Mapper) logf(f string, a ...any) {
	if m.Log != nil {
		m.Log(f, a...)
	}
}

func (m *Mapper) base() string {
	if m.Default == "" {
		return "main"
	}
	return m.Default
}

var prPathRE = regexp.MustCompile(`^prs/(\d+)\.json$`)

// untrusted: a `pr/*` branch is written by a PR author, so its own workflow files must not
// decide what runs (D7). The event does not carry the writer, so every `pr/*` branch and
// every pull_request event uses the workflows of the base branch.
func untrusted(branch string) bool { return strings.HasPrefix(branch, "pr/") }

// Map answers one ref change with the triggers it causes. It is stateless; the claim of each
// key makes a repeated or reordered event harmless.
func (m *Mapper) Map(ctx context.Context, ev RefEvent) ([]Trigger, error) {
	if !ev.IsRef() || ev.Commit == nil || strings.HasPrefix(ev.Branch, "_") && ev.Branch != "_meta" {
		return nil, nil // deletions announce nothing; `_*` branches are never built
	}
	if ev.Branch == "_meta" {
		return m.metaChanged(ctx, ev)
	}
	var out []Trigger
	push, err := m.push(ctx, ev)
	if err != nil {
		return nil, err
	}
	out = append(out, push...)
	sync, err := m.synchronize(ctx, ev)
	if err != nil {
		return nil, err
	}
	return append(out, sync...), nil
}

func (m *Mapper) author(ctx context.Context, commit string) string {
	a, err := m.Git.Author(ctx, commit)
	if err != nil || a == "" {
		return "user:unknown"
	}
	return "user:" + a
}

func (m *Mapper) push(ctx context.Context, ev RefEvent) ([]Trigger, error) {
	rev := *ev.Commit
	if untrusted(ev.Branch) {
		tip, ok, err := m.Git.Tip(ctx, m.base())
		if err != nil || !ok {
			return nil, err
		}
		rev = tip
	}
	before := ""
	if ev.Old != nil {
		before = *ev.Old
	}
	var changed []string
	var changedErr error
	loaded := false
	in := plan.Input{Event: "push", Ref: ev.Branch}
	payload := map[string]any{
		"ref": "refs/heads/" + ev.Branch, "before": before, "after": *ev.Commit,
		"repository":  map[string]any{"name": m.Repo, "default_branch": m.base()},
		"head_commit": map[string]any{"id": *ev.Commit},
	}
	actor := m.author(ctx, *ev.Commit)
	vars := map[string]string{
		"github.ref": "refs/heads/" + ev.Branch, "github.ref_name": ev.Branch, "github.event_name": "push",
		"github.sha": *ev.Commit, "github.actor": strings.TrimPrefix(actor, "user:"),
	}
	return m.plans(ctx, rev, in, func(src []byte) plan.Input {
		if bytesContainsPaths(src) { // path filters need the changed files; list them once
			if !loaded {
				loaded = true
				changed, changedErr = m.Git.Changed(ctx, before, *ev.Commit)
			}
			if changedErr == nil {
				in.Paths = changed
			}
		}
		return in
	}, func(file string) Trigger {
		return Trigger{
			Key: file + "/push/" + tier1.Esc(ev.Branch) + "@" + *ev.Commit, Workflow: file, Event: "push",
			SHA: *ev.Commit, Branch: ev.Branch, Actor: actor, Payload: payload, Vars: cloneVars(vars),
		}
	})
}

func bytesContainsPaths(src []byte) bool { return strings.Contains(string(src), "paths") }

func cloneVars(v map[string]string) map[string]string {
	o := make(map[string]string, len(v))
	for k, x := range v {
		o[k] = x
	}
	return o
}

// prs reads every `_meta:prs/<n>.json`.
func (m *Mapper) prs(ctx context.Context) ([]PRRecord, error) {
	tip, ok, err := m.Git.Tip(ctx, "_meta")
	if err != nil || !ok {
		return nil, err
	}
	tree, err := m.Git.Tree(ctx, tip)
	if err != nil {
		return nil, err
	}
	want := map[string]string{}
	for p, id := range tree {
		if prPathRE.MatchString(p) {
			want[p] = id
		}
	}
	data, err := m.Git.Blobs(ctx, tip, want)
	if err != nil {
		return nil, err
	}
	var out []PRRecord
	for _, p := range sortedPaths(data) {
		var r PRRecord
		if json.Unmarshal(data[p], &r) == nil {
			if r.ID == 0 {
				r.ID, _ = strconv.Atoi(prPathRE.FindStringSubmatch(p)[1])
			}
			out = append(out, r)
		}
	}
	return out, nil
}

func sortedPaths(m map[string][]byte) []string {
	var ks []string
	for k := range m {
		ks = append(ks, k)
	}
	sort.Strings(ks)
	return ks
}

// synchronize: a branch that is the head of an open PR moved.
func (m *Mapper) synchronize(ctx context.Context, ev RefEvent) ([]Trigger, error) {
	prs, err := m.prs(ctx)
	if err != nil {
		return nil, err
	}
	var out []Trigger
	for _, pr := range prs {
		if pr.open() && pr.Head == ev.Branch {
			ts, err := m.prTriggers(ctx, pr, "synchronize", *ev.Commit)
			if err != nil {
				return nil, err
			}
			out = append(out, ts...)
		}
	}
	return out, nil
}

// metaChanged: PR records that were opened, closed or reopened by this `_meta` commit.
func (m *Mapper) metaChanged(ctx context.Context, ev RefEvent) ([]Trigger, error) {
	old := ""
	if ev.Old != nil {
		old = *ev.Old
	}
	paths, err := m.Git.Changed(ctx, old, *ev.Commit)
	if err != nil {
		return nil, err
	}
	read := func(rev, path string) (*PRRecord, error) {
		if rev == "" {
			return nil, nil
		}
		tree, err := m.Git.Tree(ctx, rev)
		if err != nil {
			return nil, err
		}
		id, ok := tree[path]
		if !ok {
			return nil, nil
		}
		data, err := m.Git.Blobs(ctx, rev, map[string]string{path: id})
		if err != nil {
			return nil, err
		}
		var r PRRecord
		if json.Unmarshal(data[path], &r) != nil {
			return nil, nil
		}
		return &r, nil
	}
	var out []Trigger
	for _, p := range paths {
		mm := prPathRE.FindStringSubmatch(p)
		if mm == nil {
			continue
		}
		cur, err := read(*ev.Commit, p)
		if err != nil || cur == nil {
			return nil, err
		}
		if cur.ID == 0 {
			cur.ID, _ = strconv.Atoi(mm[1])
		}
		prev, err := read(old, p)
		if err != nil {
			return nil, err
		}
		action := ""
		switch {
		case prev == nil && cur.open():
			action = "opened"
		case prev != nil && !prev.open() && cur.open():
			action = "reopened"
		case prev != nil && prev.open() && !cur.open():
			action = "closed"
		}
		if action == "" {
			continue
		}
		sha, ok, err := m.Git.Tip(ctx, cur.Head)
		if err != nil || !ok {
			if err != nil {
				return nil, err
			}
			m.logf("PR %d: head branch %s does not exist; no %s event", cur.ID, cur.Head, action)
			continue
		}
		ts, err := m.prTriggers(ctx, *cur, action, sha)
		if err != nil {
			return nil, err
		}
		out = append(out, ts...)
	}
	return out, nil
}

// prTriggers plans a pull_request event with the BASE branch's workflows (D7: the PR author
// controls the head, so the head's workflow files do not decide what runs).
func (m *Mapper) prTriggers(ctx context.Context, pr PRRecord, action, sha string) ([]Trigger, error) {
	baseTip, ok, err := m.Git.Tip(ctx, pr.Base)
	if err != nil || !ok {
		return nil, err
	}
	in := plan.Input{Event: "pull_request", Ref: pr.Base, Type: action}
	payload := map[string]any{
		"action": action, "number": pr.ID,
		"pull_request": map[string]any{
			"number": pr.ID, "title": pr.Title, "state": map[bool]string{true: "open", false: "closed"}[pr.open()],
			"draft": pr.Draft, "merged": pr.State == "merged",
			"user": map[string]any{"login": pr.Author},
			"head": map[string]any{"ref": pr.Head, "sha": sha},
			"base": map[string]any{"ref": pr.Base},
		},
		"repository": map[string]any{"name": m.Repo, "default_branch": m.base()},
	}
	vars := map[string]string{
		"github.ref": fmt.Sprintf("refs/pull/%d/merge", pr.ID), "github.event_name": "pull_request",
		"github.head_ref": pr.Head, "github.base_ref": pr.Base, "github.sha": sha,
		"github.event.pull_request.number": strconv.Itoa(pr.ID), "github.event.number": strconv.Itoa(pr.ID),
		"github.actor": pr.Author,
	}
	return m.plans(ctx, baseTip, in, func([]byte) plan.Input { return in }, func(file string) Trigger {
		key := fmt.Sprintf("%s/pull_request/%d@%s", file, pr.ID, sha)
		if action != "opened" && action != "synchronize" {
			// Closing or reopening a PR at the same head must not hit the claim of the push to
			// it: say what happened. (Two reopens at one head are one event.)
			key += "/" + action
		}
		return Trigger{
			Key: key, Workflow: file, Event: "pull_request", SHA: sha, Branch: pr.Head,
			Actor: "user:" + pr.Author, Payload: payload, Vars: cloneVars(vars),
		}
	})
}

// Dispatch plans a workflow_dispatch request (docs/actions.md 6): the workflow file of the
// request, from the ref's commit (the base branch's for a `pr/*` ref).
func (m *Mapper) Dispatch(ctx context.Context, d DispatchRequest) ([]Trigger, error) {
	branch := strings.TrimPrefix(d.Ref, "refs/heads/")
	tip, ok, err := m.Git.Tip(ctx, branch)
	if err != nil || !ok {
		return nil, err
	}
	rev := tip
	if untrusted(branch) {
		if rev, ok, err = m.Git.Tip(ctx, m.base()); err != nil || !ok {
			return nil, err
		}
	}
	file := filepath.Base(d.Workflow)
	in := plan.Input{Event: "workflow_dispatch", Ref: branch}
	payload := map[string]any{"ref": "refs/heads/" + branch, "inputs": d.Inputs, "workflow": d.Workflow,
		"repository": map[string]any{"name": m.Repo, "default_branch": m.base()}}
	vars := map[string]string{"github.ref": "refs/heads/" + branch, "github.ref_name": branch, "github.event_name": "workflow_dispatch", "github.sha": tip}
	ts, err := m.plans(ctx, rev, in, func([]byte) plan.Input { return in }, func(f string) Trigger {
		return Trigger{
			Key: f + "/dispatch/" + d.UUID, Workflow: f, Event: "workflow_dispatch", SHA: tip, Branch: branch,
			Actor: d.Actor, Payload: payload, Vars: cloneVars(vars),
		}
	})
	var out []Trigger
	for _, t := range ts {
		if t.Workflow == file {
			out = append(out, t)
		}
	}
	return out, err
}

// plans reads the workflow files of a commit and returns a trigger for each one the event
// triggers. A workflow that does not parse is logged and skipped.
func (m *Mapper) plans(ctx context.Context, rev string, in plan.Input, input func(src []byte) plan.Input, mk func(file string) Trigger) ([]Trigger, error) {
	tree, err := m.Git.Tree(ctx, rev)
	if err != nil {
		return nil, err
	}
	want := map[string]string{}
	for p, id := range tree {
		if strings.HasPrefix(p, ".github/workflows/") && strings.Count(p, "/") == 2 &&
			(strings.HasSuffix(p, ".yml") || strings.HasSuffix(p, ".yaml")) {
			want[p] = id
		}
	}
	if len(want) == 0 {
		return nil, nil
	}
	data, err := m.Git.Blobs(ctx, rev, want)
	if err != nil {
		return nil, err
	}
	dir, err := os.MkdirTemp("", "pail-actions-wf-")
	if err != nil {
		return nil, err
	}
	defer os.RemoveAll(dir)
	var out []Trigger
	for _, p := range sortedPaths(data) {
		file := filepath.Base(p)
		path := filepath.Join(dir, file)
		if err := os.WriteFile(path, data[p], 0o600); err != nil {
			return nil, err
		}
		pl, err := safePlan(path, input(data[p]))
		if err != nil {
			m.logf("workflow %s at %.12s: %v", file, rev, err)
			continue
		}
		if !pl.Triggered {
			continue
		}
		t := mk(file)
		t.Plan = pl
		out = append(out, t)
	}
	return out, nil
}

// safePlan parses behind recover: act's model can panic or exit on hostile input (SPIKE.md,
// risk 1). log.Fatal cannot be recovered, so a hostile workflow is still a risk to replicas;
// the planner runs here, in process, until it is moved behind a subprocess.
func safePlan(path string, in plan.Input) (p *plan.Plan, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("planner panic: %v", r)
		}
	}()
	return plan.Workflow(path, in)
}
