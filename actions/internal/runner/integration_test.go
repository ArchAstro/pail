package runner

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/ArchAstro/pail/actions/internal/tier2"
	"github.com/ArchAstro/pail/actions/internal/tier2/fakepail"
)

// env builds a file:// pail store with one commit (hello.txt) and returns a PailStore on it
// plus the pail binary to use. It needs a real pail binary (PAIL_BIN, or target/{release,debug}).
// If that binary has no `obj` subcommand yet, `pail obj` is served by the contract fake
// (fakepail) and everything else (init, clone, commit) still runs the real pail.
func env(t *testing.T) (ps *tier2.PailStore, pail, store, commit string) {
	real := os.Getenv("PAIL_BIN")
	if real == "" {
		for _, p := range []string{"../../../target/release/pail", "../../../target/debug/pail"} {
			if abs, _ := filepath.Abs(p); fileExists(abs) {
				real = abs
				break
			}
		}
	}
	if real == "" {
		t.Skip("no pail binary (set PAIL_BIN or cargo build)")
	}
	dir := t.TempDir()
	pail = real
	if out, err := exec.Command(real, "obj", "--help").CombinedOutput(); err != nil || !strings.Contains(string(out), "ls") {
		t.Logf("pail has no `obj` yet: using the contract fake for Tier 2")
		var err error
		if pail, err = fakepail.Write(dir); err != nil {
			t.Fatal(err)
		}
		t.Setenv("PAIL_FAKE_REAL", real)
	}
	store = "file://" + dir + "/store"
	run := func(cwd string, args ...string) string {
		c := exec.Command(real, args...)
		c.Dir = cwd
		c.Env = append(os.Environ(), "PAIL_AUTHOR=T <t@example.com>", "HOME="+dir)
		out, err := c.CombinedOutput()
		if err != nil {
			t.Fatalf("pail %v: %v\n%s", args, err, out)
		}
		return strings.TrimSpace(string(out))
	}
	run(dir, "init", store)
	run(dir, "clone", store, "wc")
	wc := filepath.Join(dir, "wc")
	if err := os.WriteFile(filepath.Join(wc, "hello.txt"), []byte("hello from pail\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(wc, "add", "-A")
	run(wc, "commit", "-m", "first")
	return &tier2.PailStore{Pail: pail, URL: store}, pail, store, run(wc, "rev-parse", "HEAD")
}

func fileExists(p string) bool { _, err := os.Stat(p); return err == nil }

func actEntry(store, sha string, steps ...map[string]any) tier2.QueueEntry {
	return tier2.QueueEntry{Run: 7, Attempt: 1, Job: "show", Store: store, Sha: sha,
		Github: tier2.Github{EventName: "push", Ref: "refs/heads/main", Repository: "acme/repo", RunID: 7},
		Steps:  steps, Env: map[string]string{"CI": "true"}}
}

func runOnce(t *testing.T, ps *tier2.PailStore, pail string, e tier2.QueueEntry, during func()) {
	t.Helper()
	b, _ := json.Marshal(e)
	if _, ok, err := ps.Create(tier2.QueuePath("ubuntu-latest", e.Run, e.Attempt, e.Job), b); !ok || err != nil {
		t.Fatal(ok, err)
	}
	r := New(Config{Store: ps, Labels: []string{"ubuntu-latest"}, Name: "it-1", Once: true,
		Exec: &ActExecutor{Pail: pail, Host: true}, Refresh: 300 * time.Millisecond, Poll: 100 * time.Millisecond,
		LogFlush: 200 * time.Millisecond, Logf: t.Logf})
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	if during != nil {
		go during()
	}
	if err := r.Run(ctx); err != nil || ctx.Err() != nil {
		t.Fatal(err, ctx.Err())
	}
}

func result(t *testing.T, ps *tier2.PailStore, e tier2.QueueEntry) (tier2.Result, string) {
	t.Helper()
	b, _, found, err := ps.Get(tier2.ResultPath(e.Run, e.Attempt, e.Job, 1))
	if err != nil || !found {
		t.Fatalf("no result: %v %v", found, err)
	}
	var res tier2.Result
	if err := json.Unmarshal(b, &res); err != nil {
		t.Fatal(err)
	}
	full, _, _, _ := ps.Get(res.Log)
	return res, string(full)
}

// TestActHostSuccess: runner --once over a file:// store, host-mode act, checkout by pail clone --at.
func TestActHostSuccess(t *testing.T) {
	ps, pail, store, sha := env(t)
	e := actEntry(store, sha,
		map[string]any{"uses": "actions/checkout@v4", "with": map[string]any{"fetch-depth": 0}},
		map[string]any{"name": "Show", "run": `echo "file-says: $(cat hello.txt)"; echo "ctx: $GITHUB_SHA $GITHUB_REF $GITHUB_REPOSITORY $GITHUB_RUN_ID $CI"`})
	runOnce(t, ps, pail, e, nil)
	res, full := result(t, ps, e)
	if res.Conclusion != "success" || res.Seq != 1 || res.Runner != "it-1" || len(res.Steps) != 2 {
		t.Fatalf("%+v\n%s", res, full)
	}
	for _, s := range res.Steps {
		if s.Conclusion != "success" {
			t.Fatalf("step %+v\n%s", s, full)
		}
	}
	for _, want := range []string{"file-says: hello from pail", "ctx: " + sha + " refs/heads/main acme/repo 7 true"} {
		if !strings.Contains(full, want) {
			t.Fatalf("full.log lacks %q:\n%s", want, full)
		}
	}
	if _, _, found, _ := ps.Get(tier2.RunnerPath("it-1")); found {
		t.Fatal("heartbeat not removed at exit")
	}
	if _, _, found, _ := ps.Get(tier2.ProgressPath(e.Run, e.Attempt, e.Job, 1)); !found {
		t.Fatal("no progress file")
	}
}

func TestActHostFailure(t *testing.T) {
	ps, pail, store, sha := env(t)
	e := actEntry(store, sha,
		map[string]any{"name": "Boom", "run": "echo before; exit 3"},
		map[string]any{"name": "Never", "run": "echo never-ran"})
	runOnce(t, ps, pail, e, nil)
	res, full := result(t, ps, e)
	if res.Conclusion != "failure" || res.Steps[0].Conclusion != "failure" || res.Steps[1].Conclusion != "skipped" || strings.Contains(full, "never-ran") {
		t.Fatalf("%+v\n%s", res, full)
	}
}

func TestActHostCancel(t *testing.T) {
	ps, pail, store, sha := env(t)
	e := actEntry(store, sha, map[string]any{"name": "Sleep", "run": "echo started; sleep 60"})
	start := time.Now()
	runOnce(t, ps, pail, e, func() {
		for i := 0; i < 600; i++ { // wait for the step to start, then cancel
			if b, _, found, _ := ps.Get(tier2.ProgressPath(e.Run, e.Attempt, e.Job, 1)); found && strings.Contains(string(b), "in_progress") {
				ps.Create(tier2.CancelPath(e.Run, e.Attempt, e.Job), []byte("{}"))
				return
			}
			time.Sleep(100 * time.Millisecond)
		}
	})
	res, full := result(t, ps, e)
	if res.Conclusion != "cancelled" || time.Since(start) > 40*time.Second {
		t.Fatalf("%+v after %v\n%s", res, time.Since(start), full)
	}
}
