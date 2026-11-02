package orchestrator

import (
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/ArchAstro/pail/actions/internal/plan"
	"github.com/ArchAstro/pail/actions/internal/tier1"
)

const ciYML = `name: ci
on: push
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
  deploy:
    needs: test
    runs-on: ubuntu-latest
    environment: prod
    steps: [{run: echo deploy}]
`

var t0 = time.Date(2026, 10, 30, 17, 0, 0, 0, time.UTC)

func planOf(t *testing.T, yml string, in plan.Input) *plan.Plan {
	t.Helper()
	p := filepath.Join(t.TempDir(), "ci.yml")
	if err := os.WriteFile(p, []byte(yml), 0o644); err != nil {
		t.Fatal(err)
	}
	pl, err := plan.Workflow(p, in)
	if err != nil {
		t.Fatal(err)
	}
	return pl
}

func push(t *testing.T, branch, sha string) Trigger {
	return Trigger{
		Key: "ci.yml/push/" + branch + "@" + sha, Workflow: "ci.yml", Event: "push", SHA: sha, Branch: branch,
		Actor: "user:alice", Plan: planOf(t, ciYML, plan.Input{Event: "push", Ref: branch}),
		Vars: map[string]string{"github.ref": "refs/heads/" + branch},
	}
}

func empty() *tier1.Snapshot {
	return &tier1.Snapshot{Branch: "_actions", Files: map[string]tier1.File{}}
}

// land applies a transaction to a snapshot the way a successful `pail put` would, failing
// the test if a guard does not hold (the snapshot moved since the transaction read it).
func land(t *testing.T, cur *tier1.Snapshot, puts []tier1.PutFile) *tier1.Snapshot {
	t.Helper()
	if bad := conflicts(cur, puts); len(bad) > 0 {
		t.Fatalf("unexpected conflict on %v", bad)
	}
	return apply(cur, puts)
}

// conflicts are the paths whose --expect does not hold in cur.
func conflicts(cur *tier1.Snapshot, puts []tier1.PutFile) (bad []string) {
	for _, p := range puts {
		f, ok := cur.Files[p.Path]
		if (p.Expect == "absent" && ok) || (p.Expect != "absent" && (!ok || f.ID != p.Expect)) {
			bad = append(bad, p.Path)
		}
	}
	return bad
}

func apply(cur *tier1.Snapshot, puts []tier1.PutFile) *tier1.Snapshot {
	n := &tier1.Snapshot{Branch: cur.Branch, Commit: cur.Commit + "+", Files: map[string]tier1.File{}}
	for k, v := range cur.Files {
		n.Files[k] = v
	}
	for _, p := range puts {
		sum := sha1.Sum(p.Data)
		f := tier1.File{ID: hex.EncodeToString(sum[:])}
		if LoadActions(p.Path) {
			f.Data = p.Data
		}
		n.Files[p.Path] = f
	}
	return n
}

func tick(t *testing.T, s *tier1.Snapshot, ts ...Trigger) (*tier1.Snapshot, Decision) {
	t.Helper()
	d := Decide(s, Input{Now: t0, Triggers: ts})
	if d.Tx.Empty() {
		return s, d
	}
	return land(t, s, d.Tx.Puts()), d
}

func job(t *testing.T, s *tier1.Snapshot, id, att int, key string) tier1.Job {
	t.Helper()
	var j tier1.Job
	f, ok := s.Files[tier1.JobPath(id, att, key)]
	if !ok {
		t.Fatalf("no job %d/%d/%s", id, att, key)
	}
	if err := json.Unmarshal(f.Data, &j); err != nil {
		t.Fatal(err)
	}
	return j
}

func run(t *testing.T, s *tier1.Snapshot, id int) tier1.Run {
	t.Helper()
	var r tier1.Run
	f, ok := s.Files[tier1.RunPath(id)]
	if !ok {
		t.Fatalf("no run %d", id)
	}
	if err := json.Unmarshal(f.Data, &r); err != nil {
		t.Fatal(err)
	}
	return r
}

// setJob rewrites a job as a runner's accepted result would (Tier 2 intents are not built).
func setJob(s *tier1.Snapshot, id, att int, key string, f func(*tier1.Job)) *tier1.Snapshot {
	var j tier1.Job
	_ = json.Unmarshal(s.Files[tier1.JobPath(id, att, key)].Data, &j)
	f(&j)
	return apply(s, []tier1.PutFile{{Path: tier1.JobPath(id, att, key), Data: marshal(j)}})
}

func status(t *testing.T, s *tier1.Snapshot, id int, keys ...string) string {
	var out []string
	for _, k := range keys {
		j := job(t, s, id, 1, k)
		o := j.Status
		if j.Conclusion != nil {
			o += "/" + *j.Conclusion
		}
		out = append(out, k+"="+o)
	}
	return strings.Join(out, " ")
}

func TestPushMakesRunJobsClaim(t *testing.T) {
	s, d := tick(t, empty(), push(t, "main", "aaa111"))
	if len(d.Done) != 1 {
		t.Fatalf("done %v", d.Done)
	}
	r := run(t, s, 1)
	if r.RunNumber != 1 || r.Key != "ci.yml/push/main@aaa111" || r.Status != "queued" || r.Event != "push" ||
		r.HeadSHA != "aaa111" || r.RunAttempt != 1 || r.Workflow != ".github/workflows/ci.yml" || r.Name != "ci" {
		t.Fatalf("run %+v", r)
	}
	if r.Concurrency == nil || r.Concurrency.Group != "ci-refs/heads/main" || !r.Concurrency.CancelInProgress {
		t.Fatalf("concurrency %+v", r.Concurrency)
	}
	if got := status(t, s, 1, "build", "test", "deploy"); got != "build=queued test=pending deploy=pending" {
		t.Fatal(got)
	}
	for _, p := range []string{"claims/ci.yml/push/main@aaa111", "numbers/ci.yml/1", "concurrency/ci-refs~heads~main.json"} {
		if !s.Has(p) {
			t.Errorf("missing %s", p)
		}
	}
	var g tier1.Ref
	_ = json.Unmarshal(s.Files["concurrency/ci-refs~heads~main.json"].Data, &g)
	if g.Run != 1 {
		t.Fatalf("group holder %d", g.Run)
	}
}

func TestRepeatedEventAndTickMakeNoSecondRun(t *testing.T) {
	trig := push(t, "main", "aaa111")
	s, _ := tick(t, empty(), trig)
	again, d := tick(t, s, trig, trig) // a duplicate event, delivered twice
	if !d.Tx.Empty() || again != s {
		t.Fatalf("second tick wrote %v", d.Tx.Written())
	}
	if len(d.Done) != 1 || len(s.Under("runs/1.json")) != 1 || s.Has("runs/2.json") {
		t.Fatalf("done %v", d.Done)
	}
}

func TestSameEventTwiceInOneBatch(t *testing.T) {
	trig := push(t, "main", "aaa111")
	s, _ := tick(t, empty(), trig, trig)
	if s.Has("runs/2.json") {
		t.Fatal("a duplicate in one batch made a second run")
	}
}

func TestNeedsChainPromoteAndSkip(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	// build succeeds -> test is queued; deploy still waits on test
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("success") })
	s, _ = tick(t, s)
	if got := status(t, s, 1, "build", "test", "deploy"); got != "build=completed/success test=queued deploy=pending" {
		t.Fatal(got)
	}
	// test succeeds -> deploy is gated by its environment: waiting, not queued
	s = setJob(s, 1, 1, "test", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("success") })
	s, _ = tick(t, s)
	if got := status(t, s, 1, "deploy"); got != "deploy=waiting" {
		t.Fatal(got)
	}
	// an approval by the actor does not open the gate; one by someone else does
	own := apply(s, []tier1.PutFile{{Path: "runs/1/1/approvals/prod/alice.json", Data: []byte("{}")}})
	if _, d := tick(t, own); !d.Tx.Empty() {
		t.Fatalf("actor approved their own run: %v", d.Intents)
	}
	ok := apply(s, []tier1.PutFile{{Path: "runs/1/1/approvals/prod/bob.json", Data: []byte("{}")}})
	ok, _ = tick(t, ok)
	if got := status(t, ok, 1, "deploy"); got != "deploy=queued" {
		t.Fatal(got)
	}
}

func TestFailedNeedSkipsDependents(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("failure") })
	for i := 0; i < 3; i++ { // test, then deploy (a need skipped is a need that did not succeed)
		s, _ = tick(t, s)
	}
	if got := status(t, s, 1, "build", "test", "deploy"); got != "build=completed/failure test=completed/skipped deploy=completed/skipped" {
		t.Fatal(got)
	}
	r := run(t, s, 1)
	if r.Status != "completed" || r.Conclusion == nil || *r.Conclusion != "failure" {
		t.Fatalf("run %+v", r)
	}
}

func TestFinishSuccess(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	for _, k := range []string{"build", "test", "deploy"} {
		s = setJob(s, 1, 1, k, func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("success") })
	}
	s, d := tick(t, s)
	r := run(t, s, 1)
	if r.Status != "completed" || r.Conclusion == nil || *r.Conclusion != "success" || len(d.Intents) != 1 {
		t.Fatalf("run %+v %v", r, d.Intents)
	}
	if _, d := tick(t, s); !d.Tx.Empty() {
		t.Fatal("a finished run is final")
	}
}

func TestInProgressIsReflectedOnTheRun(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Holder, j.Seq = "in_progress", "r1", 1 })
	s, _ = tick(t, s)
	if r := run(t, s, 1); r.Status != "in_progress" || r.RunStartedAt == nil {
		t.Fatalf("run %+v", r)
	}
}

// The model's supersedeRun (UC2): a second push takes the group over in one commit; the
// running build is left to its runner (a marker, Tier 2), the rest is cancelled; when the
// runner reports cancelled the old run concludes cancelled.
func TestConcurrencyCancelInProgress(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Holder, j.Seq = "in_progress", "r1", 1 })
	s, _ = tick(t, s)

	s, d := tick(t, s, push(t, "main", "bbb222"))
	if len(d.Done) != 1 {
		t.Fatal(d.Done)
	}
	r1, r2 := run(t, s, 1), run(t, s, 2)
	if !r1.Cancel || r1.Status == "completed" || r2.Cancel || r2.RunNumber != 2 {
		t.Fatalf("r1 %+v r2 %+v", r1, r2)
	}
	if got := status(t, s, 1, "build", "test", "deploy"); got != "build=in_progress test=completed/cancelled deploy=completed/cancelled" {
		t.Fatal(got)
	}
	if got := status(t, s, 2, "build", "test"); got != "build=queued test=pending" {
		t.Fatal(got)
	}
	var g tier1.Ref
	_ = json.Unmarshal(s.Files["concurrency/ci-refs~heads~main.json"].Data, &g)
	if g.Run != 2 {
		t.Fatalf("group holder %d", g.Run)
	}
	// nothing of the cancelled run may start: promote leaves it alone
	if _, d := tick(t, s); !d.Tx.Empty() {
		t.Fatalf("idle tick wrote %v", d.Intents)
	}
	// the runner stops and its result is accepted (Tier 2, simulated) -> the run is cancelled
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("cancelled") })
	s, _ = tick(t, s)
	if r := run(t, s, 1); r.Status != "completed" || *r.Conclusion != "cancelled" {
		t.Fatalf("r1 %+v", r)
	}
	if r := run(t, s, 2); r.Cancel {
		t.Fatal("the newest run was cancelled")
	}
}

// Other branches and groups are not touched.
func TestConcurrencyIsPerGroup(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s, _ = tick(t, s, push(t, "dev", "ccc333"))
	if run(t, s, 1).Cancel {
		t.Fatal("a push to another branch cancelled main's run")
	}
}

// The model's concurrentPushesRun: two replicas read the empty branch for two pushes; the
// first commit wins the run id and the group; the second's guards fail, and deciding again
// from a fresh read makes run 2 and cancels run 1: never two active runs in the group.
func TestTwoReplicasRacing(t *testing.T) {
	base := empty()
	d1 := Decide(base, Input{Now: t0, Triggers: []Trigger{push(t, "main", "aaa111")}})
	d2 := Decide(base, Input{Now: t0, Triggers: []Trigger{push(t, "main", "bbb222")}})
	s := land(t, base, d1.Tx.Puts())
	bad := conflicts(s, d2.Tx.Puts())
	has := func(p string) bool {
		for _, b := range bad {
			if b == p {
				return true
			}
		}
		return false
	}
	if !has("runs/1.json") || !has("numbers/ci.yml/1") {
		t.Fatalf("second commit should conflict on the run id and number, got %v", bad)
	}
	// the same event for the same key from another replica: claim conflict
	d3 := Decide(base, Input{Now: t0, Triggers: []Trigger{push(t, "main", "aaa111")}})
	if bad := conflicts(s, d3.Tx.Puts()); len(bad) == 0 {
		t.Fatal("a second replica creating the same claim must conflict")
	}
	s, _ = tick(t, s, push(t, "main", "bbb222"))
	if !run(t, s, 1).Cancel || run(t, s, 2).Cancel {
		t.Fatal("run 2 should supersede run 1")
	}
}

// A trigger guards the records it read but does not write: the previous holder when it is
// done (a rerun landing meanwhile must be seen), and every job of the run it cancels.
func TestTriggerGuardsWhatItRead(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Holder, j.Seq = "in_progress", "r1", 1 })
	d := Decide(s, Input{Now: t0, Triggers: []Trigger{push(t, "main", "bbb222")}})
	exp := map[string]string{}
	for _, p := range d.Tx.Puts() {
		exp[p.Path] = p.Expect
	}
	for _, p := range []string{"runs/1.json", "runs/1/1/jobs/build.json", "runs/1/1/jobs/test.json", "concurrency/ci-refs~heads~main.json"} {
		if exp[p] == "" || exp[p] == "absent" {
			t.Errorf("%s must be --expect'ed with the id read, got %q", p, exp[p])
		}
	}
	for _, p := range []string{"runs/2.json", "claims/ci.yml/push/main@bbb222", "numbers/ci.yml/2", "runs/2/1/jobs/build.json"} {
		if exp[p] != "absent" {
			t.Errorf("%s must be --if-absent, got %q", p, exp[p])
		}
	}
	// the in-progress build was read to decide the marker, not written: an identical rewrite
	b := d.Tx.Puts()
	for _, p := range b {
		if p.Path == "runs/1/1/jobs/build.json" && string(p.Data) != string(s.Files[p.Path].Data) {
			t.Error("a guard-only record must be rewritten unchanged")
		}
	}
	// a runner's start landing in between conflicts the commit
	moved := setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Lost++ })
	if bad := conflicts(moved, d.Tx.Puts()); len(bad) != 1 || bad[0] != "runs/1/1/jobs/build.json" {
		t.Fatalf("conflicts %v", bad)
	}
}

func TestDisabledWorkflowAndNoMatchAreVoid(t *testing.T) {
	s := apply(empty(), []tier1.PutFile{{Path: "workflows/ci.yml.json", Data: []byte(`{"enabled": false}`)}})
	s2, d := tick(t, s, push(t, "main", "aaa111"))
	if s2 != s || len(d.Done) != 1 {
		t.Fatalf("disabled workflow made a run: %v", d.Intents)
	}
	nt := push(t, "main", "ddd444")
	nt.Plan = planOf(t, "name: x\non: pull_request\njobs: {a: {runs-on: x, steps: [{run: a}]}}\n", plan.Input{Event: "push", Ref: "main"})
	_, d = tick(t, empty(), nt)
	if !d.Tx.Empty() || len(d.Done) != 1 {
		t.Fatal("a plan that is not triggered must be dropped, not run")
	}
}

func TestStaticFalseIfIsSkippedAtCreation(t *testing.T) {
	yml := "name: x\non: push\njobs:\n  a: {runs-on: x, if: \"github.ref == 'refs/heads/never'\", steps: [{run: a}]}\n  b: {runs-on: x, steps: [{run: b}]}\n"
	tr := push(t, "main", "aaa111")
	tr.Plan = planOf(t, yml, plan.Input{Event: "push", Ref: "main"})
	s, _ := tick(t, empty(), tr)
	if got := status(t, s, 1, "a", "b"); got != "a=completed/skipped b=queued" {
		t.Fatal(got)
	}
}

func TestMatrixJobNamesAreEscaped(t *testing.T) {
	yml := "name: m\non: push\njobs:\n  t:\n    runs-on: x\n    strategy: {matrix: {os: [a/b, c]}}\n    steps: [{run: t}]\n"
	tr := push(t, "main", "aaa111")
	tr.Plan = planOf(t, yml, plan.Input{Event: "push", Ref: "main"})
	s, _ := tick(t, empty(), tr)
	n := 0
	for p := range s.Files {
		if strings.HasPrefix(p, "runs/1/1/jobs/") {
			n++
			if strings.Count(p, "/") != 4 {
				t.Errorf("job path %q has an unescaped slash", p)
			}
		}
	}
	if n != 2 {
		t.Fatalf("%d jobs", n)
	}
}

// ---------- _checks ----------

func TestChecksProjection(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	cs := &tier1.Snapshot{Branch: "_checks", Files: map[string]tier1.File{}}
	if tx, _ := DecideChecks(s, cs, t0); !tx.Empty() {
		t.Fatal("no job is completed: nothing to project")
	}
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("success") })
	tx, in := DecideChecks(s, cs, t0)
	if len(in) != 1 || len(tx.Puts()) != 1 {
		t.Fatalf("%v", in)
	}
	p := tx.Puts()[0]
	if p.Path != "aaa111/ci.build.json" || p.Expect != "absent" {
		t.Fatalf("%+v", p)
	}
	var c tier1.Check
	_ = json.Unmarshal(p.Data, &c)
	if c != (tier1.Check{Status: "completed", Conclusion: "success", State: "success", Workflow: "ci", Job: "build", Run: 1, Attempt: 1, URL: "/actions/runs/1"}) {
		t.Fatalf("%+v", c)
	}
	cs = apply(cs, tx.Puts())
	cs.Files[p.Path] = tier1.File{ID: cs.Files[p.Path].ID, Data: p.Data} // checks are all loaded
	if tx, _ := DecideChecks(s, cs, t0); !tx.Empty() {
		t.Fatal("an up-to-date check is not rewritten")
	}
	// reconcile: a check that differs (a crash left a stale one, or a rerun) is rewritten, CAS'd
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Conclusion = tier1.Str("failure") })
	tx, _ = DecideChecks(s, cs, t0)
	if len(tx.Puts()) != 1 || tx.Puts()[0].Expect != cs.Files[p.Path].ID {
		t.Fatalf("%+v", tx.Puts())
	}
	_ = json.Unmarshal(tx.Puts()[0].Data, &c)
	if c.State != "failure" || c.Conclusion != "failure" {
		t.Fatalf("%+v", c)
	}
}

// checksEarned: a check is projected from the job file, whatever Tier 2 says; a job that is
// not completed has no check, and a skipped job passes.
func TestChecksOnlyFromCompletedJobs(t *testing.T) {
	s, _ := tick(t, empty(), push(t, "main", "aaa111"))
	s = setJob(s, 1, 1, "build", func(j *tier1.Job) { j.Status = "in_progress" })
	s = setJob(s, 1, 1, "test", func(j *tier1.Job) { j.Status, j.Conclusion = "completed", tier1.Str("skipped") })
	tx, _ := DecideChecks(s, &tier1.Snapshot{Files: map[string]tier1.File{}}, t0)
	puts := tx.Puts()
	if len(puts) != 1 || puts[0].Path != "aaa111/ci.test.json" || !strings.Contains(string(puts[0].Data), `"state": "success"`) {
		t.Fatalf("%+v", puts)
	}
}

func TestTxGuards(t *testing.T) {
	s := apply(empty(), []tier1.PutFile{
		{Path: "runs/1.json", Data: []byte(`{"id":1}`)}, {Path: "runs/2.json", Data: []byte(`{"id":2}`)},
		{Path: "runs/3.json", Data: []byte(`{"id":3}`)},
	})
	tx := tier1.NewTx(s)
	tx.Get("runs/3.json") // scanning is not guarding
	tx.Write("runs/1.json", []byte(`{"id":1,"x":1}`))
	tx.Write("runs/9.json", []byte(`{"id":9}`))
	tx.Read("runs/2.json")
	got := map[string]tier1.PutFile{}
	for _, p := range tx.Puts() {
		got[p.Path] = p
	}
	if len(got) != 3 || got["runs/1.json"].Expect != s.Files["runs/1.json"].ID || got["runs/9.json"].Expect != "absent" ||
		got["runs/2.json"].Expect != s.Files["runs/2.json"].ID || string(got["runs/2.json"].Data) != `{"id":2}` {
		t.Fatalf("%+v", got)
	}
}

func TestExpand(t *testing.T) {
	v := map[string]string{"github.ref": "refs/heads/x", "github.head_ref": "h"}
	if got := expand("a-${{ github.ref }}-${{github.head_ref}}-${{ github.nope }}", v); got != "a-refs/heads/x-h-" {
		t.Fatal(got)
	}
}
