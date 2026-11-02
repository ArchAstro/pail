package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/nektos/act/pkg/model"

	"github.com/ArchAstro/pail/actions/internal/tier2"
)

type fakeExec func(ctx context.Context, j *Job) (string, error)

func (f fakeExec) Execute(ctx context.Context, j *Job) (string, error) { return f(ctx, j) }

var t0 = time.Date(2026, 10, 30, 17, 0, 0, 0, time.UTC)

func entry() tier2.QueueEntry {
	return tier2.QueueEntry{Run: 4182, Attempt: 1, Job: "test", Store: "file:///s", Sha: "abc",
		Github: tier2.Github{EventName: "push", Ref: "refs/heads/main", Repository: "repo", RunID: 4182},
		Steps:  []map[string]any{{"name": "one", "run": "true"}, {"name": "two", "run": "true"}}}
}

func enqueue(t *testing.T, s tier2.Store, label string, e tier2.QueueEntry) {
	t.Helper()
	b, _ := json.Marshal(e)
	if _, ok, err := s.Create(tier2.QueuePath(label, e.Run, e.Attempt, e.Job), b); !ok || err != nil {
		t.Fatal(ok, err)
	}
}

func newRunner(s tier2.Store, name string, clock tier2.Clock, x Executor) *Runner {
	return New(Config{Store: s, Labels: []string{"ubuntu-latest"}, Name: name, Clock: clock, Exec: x,
		TTL: 300 * time.Millisecond, Refresh: 20 * time.Millisecond, Poll: 10 * time.Millisecond,
		LogFlush: 10 * time.Millisecond, Logf: func(string, ...any) {}})
}

func getJSON[T any](t *testing.T, s tier2.Store, key string) (v T, found bool) {
	t.Helper()
	b, _, found, err := s.Get(key)
	if err != nil {
		t.Fatal(err)
	}
	if found {
		if err := json.Unmarshal(b, &v); err != nil {
			t.Fatal(err)
		}
	}
	return
}

func TestClaimRaceOneWins(t *testing.T) {
	s, clock := tier2.NewMemStore(), tier2.NewFakeClock(t0)
	e := entry()
	const n = 8
	var wg sync.WaitGroup
	var mu sync.Mutex
	wins := 0
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			h, err := newRunner(s, fmt.Sprintf("r%d", i), clock, nil).Claim(e)
			if err != nil {
				t.Error(err)
			}
			if h != nil {
				mu.Lock()
				wins++
				mu.Unlock()
				if h.lease.Seq != 1 {
					t.Errorf("seq %d", h.lease.Seq)
				}
			}
		}(i)
	}
	wg.Wait()
	if wins != 1 {
		t.Fatalf("%d winners", wins)
	}
}

func TestTakeoverAfterExpiryBumpsSeq(t *testing.T) {
	s, clock := tier2.NewMemStore(), tier2.NewFakeClock(t0)
	e := entry()
	r1, r2 := newRunner(s, "r1", clock, nil), newRunner(s, "r2", clock, nil)
	h1, _ := r1.Claim(e)
	if h1 == nil || h1.lease.Seq != 1 {
		t.Fatalf("%+v", h1)
	}
	clock.Advance(200 * time.Millisecond)
	if h, _ := r2.Claim(e); h != nil {
		t.Fatal("took a live lease")
	}
	clock.Advance(200 * time.Millisecond) // past the 300ms ttl
	h2, err := r2.Claim(e)
	if err != nil || h2 == nil || h2.lease.Seq != 2 || h2.lease.Holder != "r2" {
		t.Fatalf("%+v %v", h2, err)
	}
	// r1's version is stale now: its refresh would fail.
	if _, ok, _ := s.PutIfVersion(tier2.LeasePath(e.Run, e.Attempt, e.Job), []byte("{}"), h1.version); ok {
		t.Fatal("stale version accepted")
	}
	l, _ := getJSON[tier2.Lease](t, s, tier2.LeasePath(e.Run, e.Attempt, e.Job))
	if l.Seq != 2 {
		t.Fatalf("%+v", l)
	}
}

func TestClaimSkipsJobWithResult(t *testing.T) {
	s, clock := tier2.NewMemStore(), tier2.NewFakeClock(t0)
	e := entry()
	h, _ := newRunner(s, "r1", clock, nil).Claim(e)
	s.Create(tier2.ResultPath(e.Run, e.Attempt, e.Job, h.lease.Seq), []byte("{}"))
	clock.Advance(time.Hour)
	if h2, _ := newRunner(s, "r2", clock, nil).Claim(e); h2 != nil {
		t.Fatal("re-ran a finished job whose queue entry lingered")
	}
}

func TestRunSuccessReportsEverything(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) {
		j.Steps.Start("0")
		fmt.Fprintln(j.Log, "hello from the job")
		j.Steps.Finish("0", "success")
		j.Steps.Start("1")
		time.Sleep(80 * time.Millisecond) // let a refresh write progress
		j.Steps.Finish("1", "success")
		return tier2.Success, nil
	})
	r := newRunner(s, "r1", nil, x)
	if ran, err := r.PollOnce(context.Background()); !ran || err != nil {
		t.Fatal(ran, err)
	}
	res, ok := getJSON[tier2.Result](t, s, tier2.ResultPath(e.Run, e.Attempt, e.Job, 1))
	if !ok || res.Conclusion != "success" || res.Seq != 1 || res.Runner != "r1" || len(res.Steps) != 2 || res.Steps[1].Conclusion != "success" {
		t.Fatalf("%+v", res)
	}
	full, _, _, _ := s.Get(res.Log)
	if !strings.Contains(string(full), "hello from the job") || res.Log != "logs/4182/1/test/1/full.log" {
		t.Fatalf("%s %q", res.Log, full)
	}
	chunks, _ := s.List(tier2.LogPrefix(e.Run, e.Attempt, e.Job, 1))
	if len(chunks) < 2 { // at least one chunk + full.log
		t.Fatalf("%v", chunks)
	}
	p, ok := getJSON[tier2.Progress](t, s, tier2.ProgressPath(e.Run, e.Attempt, e.Job, 1))
	if !ok || p.Holder != "r1" || p.Seq != 1 || p.Steps[1].Status != "completed" {
		t.Fatalf("%+v", p)
	}
}

func TestFailureAndSkippedSteps(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) {
		j.Steps.Start("0")
		j.Steps.Finish("0", "failure")
		return tier2.Failure, errors.New("exit 1")
	})
	newRunner(s, "r1", nil, x).PollOnce(context.Background())
	res, _ := getJSON[tier2.Result](t, s, tier2.ResultPath(e.Run, e.Attempt, e.Job, 1))
	if res.Conclusion != "failure" || res.Steps[0].Conclusion != "failure" || res.Steps[1].Conclusion != "skipped" {
		t.Fatalf("%+v", res)
	}
}

func TestCancelMarkerGivesCancelledResult(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	started := make(chan struct{})
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) {
		j.Steps.Start("0")
		close(started)
		<-ctx.Done()
		return tier2.Failure, ctx.Err()
	})
	go func() {
		<-started
		s.Create(tier2.CancelPath(e.Run, e.Attempt, e.Job), []byte("{}"))
	}()
	newRunner(s, "r1", nil, x).PollOnce(context.Background())
	res, ok := getJSON[tier2.Result](t, s, tier2.ResultPath(e.Run, e.Attempt, e.Job, 1))
	if !ok || res.Conclusion != "cancelled" || res.Steps[0].Conclusion != "cancelled" || res.Steps[1].Conclusion != "cancelled" {
		t.Fatalf("%+v", res)
	}
}

func TestCancelMarkerBeforeStartRunsNothing(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	s.Create(tier2.CancelPath(e.Run, e.Attempt, e.Job), []byte("{}"))
	x := fakeExec(func(context.Context, *Job) (string, error) { t.Error("executed a cancelled job"); return "", nil })
	newRunner(s, "r1", nil, x).PollOnce(context.Background())
	res, _ := getJSON[tier2.Result](t, s, tier2.ResultPath(e.Run, e.Attempt, e.Job, 1))
	if res.Conclusion != "cancelled" {
		t.Fatalf("%+v", res)
	}
}

// A runner whose lease was taken over stops: it writes no result, no full log, and does not
// touch the successor's progress (paths carry the seq anyway).
func TestLostLeaseStopsWriting(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	leaseKey := tier2.LeasePath(e.Run, e.Attempt, e.Job)
	succProgress := tier2.ProgressPath(e.Run, e.Attempt, e.Job, 2)
	started := make(chan struct{})
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) {
		j.Steps.Start("0")
		close(started)
		<-ctx.Done()
		fmt.Fprintln(j.Log, "late output after losing the lease")
		return tier2.Failure, ctx.Err()
	})
	go func() {
		<-started
		_, v, _, _ := s.Get(leaseKey)
		b, _ := json.Marshal(tier2.Lease{Holder: "successor", ExpiresAt: time.Now().Add(time.Hour), Seq: 2})
		s.PutIfVersion(leaseKey, b, v)
		s.Put(succProgress, []byte(`{"holder":"successor"}`))
	}()
	done := make(chan struct{})
	go func() { newRunner(s, "r1", nil, x).PollOnce(context.Background()); close(done) }()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("runner kept running after losing its lease")
	}
	if _, _, found, _ := s.Get(tier2.ResultPath(e.Run, e.Attempt, e.Job, 1)); found {
		t.Fatal("lost runner wrote a result")
	}
	if _, _, found, _ := s.Get(tier2.FullLogPath(e.Run, e.Attempt, e.Job, 1)); found {
		t.Fatal("lost runner wrote full.log")
	}
	b, _, _, _ := s.Get(succProgress)
	if string(b) != `{"holder":"successor"}` {
		t.Fatalf("successor progress overwritten: %s", b)
	}
	objs, _ := s.List(tier2.LogPrefix(e.Run, e.Attempt, e.Job, 1))
	for _, o := range objs {
		if b, _, _, _ := s.Get(o.Key); strings.Contains(string(b), "late output") {
			t.Fatalf("late output uploaded in %s", o.Key)
		}
	}
}

// Refresh errors are tolerated until the lease would have expired; then the runner stops.
func TestRefreshFailureUntilExpiryStopsWriting(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	enqueue(t, s, "ubuntu-latest", e)
	s.Fault = func(op, key string) error {
		if op == "putv" && strings.HasPrefix(key, "leases/") {
			return errors.New("store unreachable")
		}
		return nil
	}
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) {
		<-ctx.Done()
		return tier2.Failure, ctx.Err()
	})
	start := time.Now()
	newRunner(s, "r1", nil, x).PollOnce(context.Background())
	if d := time.Since(start); d < 250*time.Millisecond {
		t.Fatalf("gave up after %v, before the lease expired", d)
	}
	if _, _, found, _ := s.Get(tier2.ResultPath(e.Run, e.Attempt, e.Job, 1)); found {
		t.Fatal("wrote a result after the lease lapsed")
	}
}

func TestOnceWaitsForWorkAndHeartbeats(t *testing.T) {
	s := tier2.NewMemStore()
	e := entry()
	x := fakeExec(func(ctx context.Context, j *Job) (string, error) { return tier2.Success, nil })
	r := newRunner(s, "r1", nil, x)
	r.c.Once = true
	go func() {
		time.Sleep(100 * time.Millisecond)
		if info, ok := getJSON[tier2.RunnerInfo](t, s, tier2.RunnerPath("r1")); !ok || info.Labels[0] != "ubuntu-latest" {
			t.Errorf("no heartbeat: %+v", info)
		}
		enqueue(t, s, "ubuntu-latest", e)
	}()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := r.Run(ctx); err != nil || ctx.Err() != nil {
		t.Fatal(err, ctx.Err())
	}
	if _, ok := getJSON[tier2.Result](t, s, tier2.ResultPath(e.Run, e.Attempt, e.Job, 1)); !ok {
		t.Fatal("no result")
	}
}

func TestSecondRunnerSkipsHeldJob(t *testing.T) {
	s := tier2.NewMemStore()
	enqueue(t, s, "ubuntu-latest", entry())
	if h, _ := newRunner(s, "r1", nil, nil).Claim(entry()); h == nil {
		t.Fatal("no claim")
	}
	ran, _ := newRunner(s, "r2", nil, fakeExec(func(context.Context, *Job) (string, error) {
		t.Error("ran a held job")
		return "", nil
	})).PollOnce(context.Background())
	if ran {
		t.Fatal("ran")
	}
}

func TestChunker(t *testing.T) {
	var got []string
	c := NewChunker(10, func(n int, d []byte) { got = append(got, fmt.Sprintf("%d:%s", n, d)) }, nil)
	c.Write([]byte("0123456789ABCDE")) // 15 bytes: one full chunk, 5 buffered
	if len(got) != 1 || got[0] != "1:0123456789" {
		t.Fatalf("%q", got)
	}
	c.Flush()
	c.Flush() // empty flush emits nothing
	if len(got) != 2 || got[1] != "2:ABCDE" || c.Chunks() != 2 {
		t.Fatalf("%q", got)
	}
	if string(c.Full()) != "0123456789ABCDE" {
		t.Fatal(string(c.Full()))
	}
}

func TestWorkflowSynthesis(t *testing.T) {
	e := entry()
	e.Env = map[string]string{"CI": "true"}
	e.Container = "debian:stable-slim"
	e.Services = map[string]any{"db": map[string]any{"image": "postgres:16"}}
	e.Steps = []map[string]any{{"uses": "actions/checkout@v4"}, {"id": "b", "run": "cargo test", "env": map[string]any{"A": "1"}}}
	src, err := Workflow(e, "ubuntu-latest")
	if err != nil {
		t.Fatal(err)
	}
	w, err := model.ReadWorkflow(strings.NewReader(string(src)), false)
	if err != nil {
		t.Fatalf("%v\n%s", err, src)
	}
	j := w.GetJob("test")
	if j == nil || len(j.Steps) != 2 || j.RunsOn()[0] != "ubuntu-latest" || j.Env.Kind == 0 || len(j.Services) != 1 || j.Container() == nil {
		t.Fatalf("%+v\n%s", j, src)
	}
}
