package plan

import (
	"strings"
	"testing"

	"github.com/nektos/act/pkg/model"
)

// These tests pin what act's model does and does not do at the pinned version, so a bump of
// github.com/nektos/act shows which of our workarounds can go. See actions/SPIKE.md.

func readAct(t *testing.T, yml string) *model.Workflow {
	w, err := model.ReadWorkflow(strings.NewReader(yml), false)
	if err != nil {
		t.Fatal(err)
	}
	return w
}

// act expands `matrix: ${{ fromJSON(...) }}` to one job, silently. (With the expression
// under a key, `matrix: {p: ${{ ... }}}`, act's GetMatrixes calls log.Fatal and exits the
// process, which is why expand() checks for expressions first and cannot be tested here.)
func TestActIgnoresDynamicMatrix(t *testing.T) {
	w := readAct(t, "on: push\njobs:\n  j:\n    runs-on: x\n    strategy:\n      matrix: ${{ fromJSON(needs.a.outputs.m) }}\n    steps: [{run: e}]\n")
	ms, err := w.GetJob("j").GetMatrixes()
	if err != nil || len(ms) != 1 || len(ms[0]) != 0 {
		t.Fatalf("matrixes %v err %v: act changed, revisit expand()", ms, err)
	}
}

// act's Workflow.On() lists event names only: no branches, paths or types.
func TestActHasNoTriggerFilters(t *testing.T) {
	w := readAct(t, "on:\n  push:\n    branches: [main]\njobs:\n  j: {runs-on: x, steps: [{run: e}]}\n")
	if ev := w.On(); len(ev) != 1 || ev[0] != "push" {
		t.Fatalf("On() = %v", ev)
	}
}

// act's Job drops environment, concurrency and job-level continue-on-error; extras in plan.go
// re-reads them from the YAML.
func TestActDropsJobKeys(t *testing.T) {
	p, err := Workflow(write(t, "on: push\njobs:\n  j:\n    runs-on: x\n    environment: prod\n    continue-on-error: true\n    concurrency: g\n    steps: [{run: e}]\n"), in("push", "main"))
	if err != nil {
		t.Fatal(err)
	}
	j := p.Jobs[0]
	if j.Environment != "prod" || j.ContinueOnErr != "true" || j.Concurrency == nil || j.Concurrency.Group != "g" {
		t.Fatalf("%+v", j)
	}
}

// act accepts a job without runs-on, and defaults name to the id and `if` to success().
func TestActDefaults(t *testing.T) {
	w := readAct(t, "on: push\njobs:\n  j:\n    steps: [{run: e}]\n")
	j := w.GetJob("j")
	if len(j.RunsOn()) == 0 && j.If.Value != "success()" {
		t.Fatalf("defaults changed: runs-on %v if %q", j.RunsOn(), j.If.Value)
	}
}
