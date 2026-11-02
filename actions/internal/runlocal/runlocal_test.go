package runlocal

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/nektos/act/pkg/model"
)

const wf = `name: local
on: push
jobs:
  show:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Show checkout
        run: |
          echo "file-says: $(cat hello.txt)"
          test "$(ls | wc -l)" -ge 1
`

func TestSwapCheckout(t *testing.T) {
	w, err := model.ReadWorkflow(strings.NewReader(wf), false)
	if err != nil {
		t.Fatal(err)
	}
	if n := SwapCheckout(w, "pail", "file:///s", "abc"); n != 1 {
		t.Fatalf("swapped %d", n)
	}
	s := w.GetJob("show").Steps[0]
	if s.Uses != "" || s.With != nil || !strings.Contains(s.Run, "'pail' clone --at 'abc' 'file:///s' \"$GITHUB_WORKSPACE\"") {
		t.Fatalf("step: %+v", s)
	}
}

// store makes a file:// pail store with one commit that has hello.txt, using the real pail binary.
func store(t *testing.T) (pail, url, commit string) {
	pail = os.Getenv("PAIL_BIN")
	if pail == "" {
		pail, _ = filepath.Abs("../../../target/release/pail")
	}
	if _, err := os.Stat(pail); err != nil {
		t.Skipf("no pail binary (set PAIL_BIN or cargo build --release): %v", err)
	}
	dir := t.TempDir()
	url = "file://" + dir + "/store"
	run := func(cwd string, args ...string) string {
		c := exec.Command(pail, args...)
		c.Dir = cwd
		c.Env = append(os.Environ(), "PAIL_AUTHOR=T <t@example.com>", "HOME="+dir)
		out, err := c.CombinedOutput()
		if err != nil {
			t.Fatalf("pail %v: %v\n%s", args, err, out)
		}
		return strings.TrimSpace(string(out))
	}
	run(dir, "init", url)
	run(dir, "clone", url, "wc")
	wc := filepath.Join(dir, "wc")
	if err := os.WriteFile(filepath.Join(wc, "hello.txt"), []byte("hello from pail\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(wc, "add", "-A")
	run(wc, "commit", "-m", "first")
	return pail, url, run(wc, "rev-parse", "HEAD")
}

func workflowFile(t *testing.T) string {
	p := filepath.Join(t.TempDir(), "local.yml")
	if err := os.WriteFile(p, []byte(wf), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

// TestRunHost runs the job with act's host executor (no docker): checkout is `pail clone --at`.
func TestRunHost(t *testing.T) {
	pail, url, commit := store(t)
	var out bytes.Buffer
	err := Run(Options{Workflow: workflowFile(t), Job: "show", Event: "push", Store: url, Commit: commit, Pail: pail, Host: true, Out: &out})
	if err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	if !strings.Contains(out.String(), "file-says: hello from pail") {
		t.Fatalf("checkout did not materialise the commit:\n%s", out.String())
	}
}

// TestRunDocker runs the job in a container. It needs docker and a linux pail binary
// (PAIL_LINUX_BIN); it is skipped otherwise, and never runs in CI.
func TestRunDocker(t *testing.T) {
	linux := os.Getenv("PAIL_LINUX_BIN")
	if linux == "" {
		t.Skip("PAIL_LINUX_BIN not set")
	}
	if err := exec.Command("docker", "info").Run(); err != nil {
		t.Skip("docker is not available")
	}
	_, url, commit := store(t)
	var out bytes.Buffer
	err := Run(Options{Workflow: workflowFile(t), Job: "show", Event: "push", Store: url, Commit: commit, Image: "debian:stable-slim", LinuxPail: linux, Out: &out})
	if err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	if !strings.Contains(out.String(), "file-says: hello from pail") {
		t.Fatalf("checkout did not materialise the commit:\n%s", out.String())
	}
}
