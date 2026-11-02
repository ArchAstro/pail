package orchestrator

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ArchAstro/pail/actions/internal/tier1"
)

const wfPush = `name: ci
on:
  push:
    branches: [main]
  pull_request:
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps: [{run: echo build}]
  test:
    needs: build
    runs-on: ubuntu-latest
    steps: [{run: echo test}]
`

type env struct {
	t    *testing.T
	pail string
	url  string
	ctx  context.Context
}

// newEnv makes a file:// store with the real pail binary; the test is skipped without one.
func newEnv(t *testing.T) *env {
	pail := os.Getenv("PAIL_BIN")
	if pail == "" {
		pail, _ = filepath.Abs("../../../target/release/pail")
	}
	if _, err := os.Stat(pail); err != nil {
		t.Skipf("no pail binary (set PAIL_BIN or cargo build --release): %v", err)
	}
	dir := t.TempDir()
	e := &env{t: t, pail: pail, url: "file://" + dir + "/store", ctx: context.Background()}
	c := exec.Command(pail, "init", e.url)
	c.Env = append(os.Environ(), "PAIL_AUTHOR=Alice <a@example.com>", "HOME="+dir)
	if out, err := c.CombinedOutput(); err != nil {
		t.Fatalf("pail init: %v\n%s", err, out)
	}
	t.Setenv("HOME", dir)
	return e
}

func (e *env) pailClient() *tier1.Pail {
	return &tier1.Pail{Bin: e.pail, Store: e.url, Env: []string{"PAIL_AUTHOR=pail-actions <actions@example.com>"}}
}

// commit puts files on a branch as Alice and returns the new commit.
func (e *env) commit(branch string, files map[string]string) string {
	e.t.Helper()
	p := e.pailClient()
	p.Env = []string{"PAIL_AUTHOR=Alice <a@example.com>"}
	var puts []tier1.PutFile
	tip, ok, err := p.Tip(e.ctx, branch)
	if err != nil {
		e.t.Fatal(err)
	}
	var ids map[string]string
	if ok {
		ids, _ = p.Tree(e.ctx, tip)
	}
	for path, data := range files {
		exp := "absent"
		if id, ok := ids[path]; ok {
			exp = id
		}
		puts = append(puts, tier1.PutFile{Path: path, Data: []byte(data), Expect: exp})
	}
	if err := p.Put(e.ctx, branch, "test commit", puts); err != nil {
		e.t.Fatal(err)
	}
	tip, _, _ = p.Tip(e.ctx, branch)
	return tip
}

func (e *env) orch() *Orchestrator {
	o := New(e.pailClient(), "main")
	o.Log = e.t.Logf
	return o
}

func (e *env) actions() *tier1.Snapshot {
	s, err := e.pailClient().Snapshot(e.ctx, "_actions", LoadActions)
	if err != nil {
		e.t.Fatal(err)
	}
	return s
}

func runs(s *tier1.Snapshot) []string { return s.Under("runs/") }

func str(s string) *string { return &s }

func TestIntegrationPushTick(t *testing.T) {
	e := newEnv(t)
	sha := e.commit("main", map[string]string{".github/workflows/ci.yml": wfPush})
	o := e.orch()
	o.Observe(RefEvent{Branch: "main", Commit: &sha})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	s := e.actions()
	key := "ci.yml/push/main@" + sha
	var r tier1.Run
	if err := json.Unmarshal(s.Files["runs/1.json"].Data, &r); err != nil {
		t.Fatalf("no run 1: %v (%v)", err, runs(s))
	}
	if r.Key != key || r.Event != "push" || r.HeadSHA != sha || r.Actor != "user:Alice" || r.Status != "queued" || r.RunNumber != 1 {
		t.Fatalf("run %+v", r)
	}
	for _, p := range []string{"claims/" + key, "numbers/ci.yml/1", "runs/1/1/jobs/build.json", "runs/1/1/jobs/test.json", "concurrency/ci-refs~heads~main.json"} {
		if !s.Has(p) {
			t.Errorf("missing %s in %v", p, s.Under(""))
		}
	}
	var test tier1.Job
	_ = json.Unmarshal(s.Files["runs/1/1/jobs/test.json"].Data, &test)
	if test.Status != "pending" || len(test.Needs) != 1 {
		t.Fatalf("test %+v", test)
	}
	if len(o.PendingKeys()) != 0 {
		t.Fatalf("pending %v", o.PendingKeys())
	}
	// the same event again, and a restart (a fresh replica reading the same event): no second run
	o.Observe(RefEvent{Branch: "main", Commit: &sha})
	o2 := e.orch()
	o2.Observe(RefEvent{Branch: "main", Commit: &sha, Source: "resume"})
	for _, x := range []*Orchestrator{o, o2} {
		if err := x.Drain(e.ctx); err != nil {
			t.Fatal(err)
		}
	}
	if got := runs(e.actions()); strings.Contains(strings.Join(got, " "), "runs/2") {
		t.Fatalf("a repeated event made a second run: %v", got)
	}

	// a runner's accepted result (Tier 2 is not built: written by hand) -> promote, finish, _checks
	complete := func(job string) {
		s := e.actions()
		var j tier1.Job
		_ = json.Unmarshal(s.Files["runs/1/1/jobs/"+job+".json"].Data, &j)
		j.Status, j.Conclusion = "completed", str("success")
		p := "runs/1/1/jobs/" + job + ".json"
		if err := e.pailClient().Put(e.ctx, "_actions", "result", []tier1.PutFile{{Path: p, Data: marshal(j), Expect: s.Files[p].ID}}); err != nil {
			t.Fatal(err)
		}
		if err := o.Drain(e.ctx); err != nil {
			t.Fatal(err)
		}
	}
	complete("build")
	s = e.actions()
	_ = json.Unmarshal(s.Files["runs/1/1/jobs/test.json"].Data, &test)
	if test.Status != "queued" {
		t.Fatalf("promote: test is %s", test.Status)
	}
	complete("test")
	s = e.actions()
	_ = json.Unmarshal(s.Files["runs/1.json"].Data, &r)
	if r.Status != "completed" || r.Conclusion == nil || *r.Conclusion != "success" {
		t.Fatalf("finish: run %+v", r)
	}
	cs, err := e.pailClient().Snapshot(e.ctx, "_checks", LoadChecks)
	if err != nil {
		t.Fatal(err)
	}
	var c tier1.Check
	if err := json.Unmarshal(cs.Files[sha+"/ci.test.json"].Data, &c); err != nil || c.State != "success" || c.Run != 1 {
		t.Fatalf("check %v %+v (%v)", err, c, cs.Under(""))
	}
	if !cs.Has(sha + "/ci.build.json") {
		t.Fatal("no check for build")
	}
}

// A competing commit lands between a tick's read and its commit: the commit fails its guards
// (exit 4), the tick reads again and decides again: the trigger is not made twice.
func TestIntegrationConflictRetry(t *testing.T) {
	e := newEnv(t)
	sha := e.commit("main", map[string]string{".github/workflows/ci.yml": wfPush})
	a, b := e.orch(), e.orch()
	b.Observe(RefEvent{Branch: "main", Commit: &sha})
	a.Observe(RefEvent{Branch: "main", Commit: &sha})
	puts, conflicts := 0, 0
	a.Pail.BeforePut = func() {
		puts++
		if puts == 1 { // replica b answers the same event first
			a.Pail.BeforePut = nil
			if err := b.Drain(e.ctx); err != nil {
				t.Error(err)
			}
		}
	}
	for i := 0; i < 5; i++ {
		res, err := a.Tick(e.ctx)
		if err != nil {
			t.Fatal(err)
		}
		if res.Conflict {
			conflicts++
		} else if !res.Landed {
			break
		}
	}
	if conflicts != 1 {
		t.Fatalf("expected exactly one conflict, got %d", conflicts)
	}
	s := e.actions()
	n := 0
	for _, p := range s.Under("runs/") {
		if strings.Count(p, "/") == 1 {
			n++
		}
	}
	if n != 1 || s.Has("runs/2.json") {
		t.Fatalf("double trigger: %v", runs(s))
	}
	if len(a.PendingKeys()) != 0 {
		t.Fatalf("pending %v", a.PendingKeys())
	}
}

// A competing push to the same group between read and commit: the retry supersedes it.
func TestIntegrationSupersede(t *testing.T) {
	e := newEnv(t)
	sha1 := e.commit("main", map[string]string{".github/workflows/ci.yml": wfPush})
	o := e.orch()
	o.Observe(RefEvent{Branch: "main", Commit: &sha1})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	sha2 := e.commit("main", map[string]string{"a.txt": "x"})
	o.Observe(RefEvent{Branch: "main", Commit: &sha2, Old: &sha1})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	s := e.actions()
	var r1, r2 tier1.Run
	_ = json.Unmarshal(s.Files["runs/1.json"].Data, &r1)
	_ = json.Unmarshal(s.Files["runs/2.json"].Data, &r2)
	if r2.HeadSHA != sha2 || r2.RunNumber != 2 || r2.Cancel || !r1.Cancel || r1.Status != "completed" {
		t.Fatalf("r1 %+v r2 %+v", r1, r2)
	}
}

// A PR opened in _meta runs the BASE branch's workflows (D7), keyed by number and head sha;
// the PR branch's own copy of the workflow is ignored, and a push to the PR branch is a
// synchronize.
func TestIntegrationPullRequest(t *testing.T) {
	e := newEnv(t)
	e.commit("main", map[string]string{".github/workflows/ci.yml": wfPush})
	evil := strings.Replace(wfPush, "echo build", "echo pwned", 1)
	head := e.commit("pr/x", map[string]string{".github/workflows/ci.yml": evil, "f.txt": "1"})
	meta := e.commit("_meta", map[string]string{"prs/1.json": `{"id":1,"title":"T","head":"pr/x","base":"main","author":"Bob","state":"open"}`})
	o := e.orch()
	o.Observe(RefEvent{Branch: "_meta", Commit: &meta})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	s := e.actions()
	if !s.Has("claims/ci.yml/pull_request/1@" + head) {
		t.Fatalf("no PR claim: %v", s.Under("claims/"))
	}
	var r tier1.Run
	if err := json.Unmarshal(s.Files["runs/1.json"].Data, &r); err != nil {
		t.Fatal(err)
	}
	pr := r.Payload["pull_request"].(map[string]any)
	if r.Event != "pull_request" || r.HeadSHA != head || r.HeadBranch != "pr/x" || r.Actor != "user:Bob" || pr["number"].(float64) != 1 || r.Payload["action"] != "opened" {
		t.Fatalf("run %+v", r)
	}
	// pushing to the PR branch: synchronize (the pr/* branch's push itself is not main's)
	head2 := e.commit("pr/x", map[string]string{"f.txt": "2"})
	o.Observe(RefEvent{Branch: "pr/x", Commit: &head2, Old: &head})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	s = e.actions()
	if !s.Has("claims/ci.yml/pull_request/1@" + head2) {
		t.Fatalf("no synchronize claim: %v", s.Under("claims/"))
	}
	for _, p := range s.Under("claims/") {
		if strings.Contains(p, "/push/") {
			t.Fatalf("push on a pr/* branch must not match the main-only push filter: %s", p)
		}
	}
	// closing the PR: a closed event with its own key (same head sha as the last sync)
	meta2 := e.commit("_meta", map[string]string{"prs/1.json": `{"id":1,"title":"T","head":"pr/x","base":"main","author":"Bob","state":"closed"}`})
	o.Observe(RefEvent{Branch: "_meta", Commit: &meta2, Old: &meta})
	if err := o.Drain(e.ctx); err != nil {
		t.Fatal(err)
	}
	// `on: pull_request` without types does not include closed: no run
	if e.actions().Has("claims/ci.yml/pull_request/1@" + head2 + "/closed") {
		t.Fatal("closed is not a default pull_request type")
	}
}

func TestParseEvent(t *testing.T) {
	ev, err := ParseEvent([]byte(`{"branch":"pr/x","commit":"abc","old":null,"source":"resume","at":1793653787400}`))
	if err != nil || !ev.IsRef() || ev.Branch != "pr/x" || *ev.Commit != "abc" || ev.Old != nil {
		t.Fatalf("%v %+v", err, ev)
	}
	ev, _ = ParseEvent([]byte(`{"key":"queue/x/1.json","version":"1","old":null,"op":"put","source":"gcs","at":1}`))
	if ev.IsRef() || ev.Key == "" {
		t.Fatalf("%+v", ev)
	}
}
