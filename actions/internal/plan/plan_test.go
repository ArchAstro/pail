package plan

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func write(t *testing.T, yml string) string {
	p := filepath.Join(t.TempDir(), "w.yml")
	if err := os.WriteFile(p, []byte(yml), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

// Hand-written expectations, independent of the generated fixtures.
func TestMatrixCounts(t *testing.T) {
	cases := []struct {
		name   string
		matrix string
		want   []string
	}{
		{"product", "{a: [1, 2], b: [x, y]}", []string{"j (1, x)", "j (1, y)", "j (2, x)", "j (2, y)"}},
		{"exclude", "{a: [1, 2], b: [x, y], exclude: [{a: 1, b: y}]}", []string{"j (1, x)", "j (2, x)", "j (2, y)"}},
		{"include extra", "{a: [1], include: [{a: 2, c: z}]}", []string{"j (1)", "j (2, z)"}},
		{"include merge", "{a: [1, 2], include: [{a: 2, c: z}]}", []string{"j (1)", "j (2, z)"}},
		{"include only", "{include: [{a: 1}, {a: 2}]}", []string{"j (1)", "j (2)"}},
		{"dynamic", "${{ fromJSON(needs.x.outputs.m) }}", []string{"j"}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			p, err := Workflow(write(t, "on: push\njobs:\n  j:\n    runs-on: x\n    strategy:\n      matrix: "+c.matrix+"\n    steps: [{run: echo}]\n"), in("push", "main"))
			if err != nil {
				t.Fatal(err)
			}
			var got []string
			for _, j := range p.Jobs {
				got = append(got, j.ID)
			}
			if strings.Join(got, "|") != strings.Join(c.want, "|") {
				t.Fatalf("got %v want %v", got, c.want)
			}
			if c.name == "dynamic" && !p.Jobs[0].MatrixDynamic {
				t.Fatal("dynamic matrix not flagged")
			}
		})
	}
}

func TestNeedsStages(t *testing.T) {
	p, err := Workflow(write(t, `on: push
jobs:
  d: {runs-on: x, needs: [b, c], steps: [{run: e}]}
  b: {runs-on: x, needs: a, steps: [{run: e}]}
  c: {runs-on: x, needs: [a], steps: [{run: e}]}
  a: {runs-on: x, steps: [{run: e}]}
`), in("push", "main"))
	if err != nil {
		t.Fatal(err)
	}
	want := [][]string{{"a"}, {"b", "c"}, {"d"}}
	if len(p.Stages) != 3 {
		t.Fatalf("stages %v", p.Stages)
	}
	for i := range want {
		if strings.Join(p.Stages[i], ",") != strings.Join(want[i], ",") {
			t.Fatalf("stage %d: %v want %v", i, p.Stages[i], want[i])
		}
	}
}

func TestNeedsErrors(t *testing.T) {
	for name, jobs := range map[string]string{
		"unknown": "  a: {runs-on: x, needs: nope, steps: [{run: e}]}",
		"cycle":   "  a: {runs-on: x, needs: b, steps: [{run: e}]}\n  b: {runs-on: x, needs: a, steps: [{run: e}]}",
	} {
		if _, err := Workflow(write(t, "on: push\njobs:\n"+jobs+"\n"), in("push", "main")); err == nil {
			t.Errorf("%s: want an error", name)
		}
	}
}

func TestStaticIf(t *testing.T) {
	cases := []struct {
		expr string
		in   Input
		want string
	}{
		{"github.ref == 'refs/heads/main'", in("push", "main"), "true"},
		{"github.ref == 'refs/heads/main'", in("push", "dev"), "false"},
		{"${{ startsWith(github.ref, 'refs/tags/') }}", in("push", "refs/tags/v1"), "true"},
		{"github.event_name == 'pull_request'", in("pull_request", "main"), "true"},
		{"github.ref == 'refs/heads/main'", in("pull_request", "main"), "deferred"},
		{"always()", in("push", "main"), "deferred"},
		{"needs.a.result == 'success'", in("push", "main"), "deferred"},
		{"github.event.pull_request.draft == false", in("pull_request", "main"), "deferred"},
		{"secrets.X != ''", in("push", "main"), "deferred"},
		{"", in("push", "main"), ""},
	}
	for _, c := range cases {
		if got := staticIf(c.expr, c.in, nil); got != c.want {
			t.Errorf("%q on %v: got %q want %q", c.expr, c.in, got, c.want)
		}
	}
}

func TestInvalidWorkflow(t *testing.T) {
	// Schema violations must fail loudly rather than plan nothing. (act does not require
	// runs-on on a job: a gap the orchestrator has to close itself.)
	if _, err := Workflow(write(t, "on: push\njobs:\n  a:\n    runs-on: x\n    bogus-key: 1\n    steps: []\n"), in("push", "main")); err == nil {
		t.Fatal("unknown job key should be rejected by act's schema")
	}
}
