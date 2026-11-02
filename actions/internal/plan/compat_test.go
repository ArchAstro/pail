package plan

import (
	"bytes"
	"encoding/json"
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

var update = flag.Bool("update", false, "rewrite the plan fixtures in compat/fixtures from the current output")

const compat = "../../compat"

// Scenario is one planned event; Plan is the expected output.
type Scenario struct {
	Name  string          `json:"name"`
	Input Input           `json:"input"`
	Plan  json.RawMessage `json:"plan"`
}

type Fixture struct {
	Workflow  string     `json:"workflow"`
	Scenarios []Scenario `json:"scenarios"`
}

func in(event, ref string, paths ...string) Input {
	return Input{Event: event, Ref: ref, Paths: paths}
}

func fixtureName(workflow string) string {
	dir, file := filepath.Split(workflow)
	prefix := ""
	if strings.HasPrefix(dir, "pail") {
		prefix = "pail-"
	}
	return prefix + strings.TrimSuffix(file, ".yml") + ".json"
}

func workflows(t *testing.T) []string {
	var all []string
	for _, d := range []string{"workflows", "pail"} {
		m, err := filepath.Glob(filepath.Join(compat, d, "*.yml"))
		if err != nil {
			t.Fatal(err)
		}
		for _, f := range m {
			rel, _ := filepath.Rel(compat, f)
			all = append(all, rel)
		}
	}
	return all
}

// TestCorpus plans every scenario of every corpus fixture and compares it with the fixture.
// Run `go test ./internal/plan -update` to regenerate after an intended change, then review
// the diff: the fixtures are the specification.
func TestCorpus(t *testing.T) {
	files := workflows(t)
	if len(files) < 20 {
		t.Fatalf("corpus has %d workflows", len(files))
	}
	for _, rel := range files {
		t.Run(rel, func(t *testing.T) {
			fxPath := filepath.Join(compat, "fixtures", fixtureName(rel))
			var fx Fixture
			if b, err := os.ReadFile(fxPath); err == nil {
				if err := json.Unmarshal(b, &fx); err != nil {
					t.Fatal(err)
				}
			} else if *update {
				fx = Fixture{Workflow: rel}
				for _, s := range seed[rel] {
					fx.Scenarios = append(fx.Scenarios, s)
				}
			} else {
				t.Fatalf("no fixture %s (run with -update)", fxPath)
			}
			if len(fx.Scenarios) == 0 {
				t.Fatalf("%s has no scenarios", fxPath)
			}
			for i, sc := range fx.Scenarios {
				got, err := Workflow(filepath.Join(compat, rel), sc.Input)
				if err != nil {
					t.Fatalf("%s: %v", sc.Name, err)
				}
				gb, _ := JSON(got)
				if *update {
					fx.Scenarios[i].Plan = gb
					continue
				}
				var a, b bytes.Buffer
				json.Compact(&a, gb)
				json.Compact(&b, sc.Plan)
				if a.String() != b.String() {
					t.Errorf("%s: plan differs from fixture\n got: %s\nwant: %s", sc.Name, a.String(), b.String())
				}
			}
			if *update {
				out, _ := JSON(fx)
				if err := os.MkdirAll(filepath.Dir(fxPath), 0o755); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(fxPath, append(out, '\n'), 0o644); err != nil {
					t.Fatal(err)
				}
			}
		})
	}
}

// TestNoStaleFixtures fails on a fixture whose workflow was removed.
func TestNoStaleFixtures(t *testing.T) {
	want := map[string]bool{}
	for _, rel := range workflows(t) {
		want[fixtureName(rel)] = true
	}
	m, _ := filepath.Glob(filepath.Join(compat, "fixtures", "*.json"))
	for _, f := range m {
		if !want[filepath.Base(f)] {
			t.Errorf("stale fixture %s", f)
		}
	}
}
