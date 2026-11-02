package plan

import (
	"fmt"
	"sort"
	"strings"

	"github.com/nektos/act/pkg/model"
	"gopkg.in/yaml.v3"
)

// expand returns the matrix combinations of a job using act's own expansion (include,
// exclude, cross product). Matrices that depend on an expression (fromJSON) cannot be
// expanded before the job's needs ran: those come back as one placeholder with dynamic set.
// order is the key order used in instance names (declaration order, then include-only keys).
func expand(j *model.Job, node *yaml.Node) (ms []map[string]any, dynamic bool, order []string, err error) {
	if j.Strategy == nil || node.Kind == 0 {
		return []map[string]any{nil}, false, nil, nil
	}
	if hasExpression(node) {
		return []map[string]any{nil}, true, nil, nil
	}
	got, err := j.GetMatrixes()
	if err != nil {
		return nil, false, nil, err
	}
	seen := map[string]bool{}
	if node.Kind == yaml.MappingNode {
		for i := 0; i+1 < len(node.Content); i += 2 {
			k := node.Content[i].Value
			if k != "include" && k != "exclude" {
				order = append(order, k)
				seen[k] = true
			}
		}
	}
	var rest []string
	for _, m := range got {
		for k := range m {
			if !seen[k] {
				seen[k] = true
				rest = append(rest, k)
			}
		}
	}
	sort.Strings(rest)
	order = append(order, rest...)
	for _, m := range got {
		if len(m) == 0 {
			m = nil
		}
		ms = append(ms, m)
	}
	if len(ms) == 0 {
		return nil, false, nil, fmt.Errorf("matrix expands to no jobs")
	}
	return ms, false, order, nil
}

func hasExpression(n *yaml.Node) bool {
	if n.Kind == yaml.ScalarNode {
		return strings.Contains(n.Value, "${{")
	}
	for _, c := range n.Content {
		if hasExpression(c) {
			return true
		}
	}
	return false
}

// instanceID names a matrix instance as GitHub does: "job (v1, v2)" in key order, skipping
// keys an instance does not have.
func instanceID(jobID string, m map[string]any, order []string) string {
	if len(m) == 0 {
		return jobID
	}
	var vs []string
	for _, k := range order {
		if v, ok := m[k]; ok {
			vs = append(vs, fmt.Sprint(v))
		}
	}
	return jobID + " (" + strings.Join(vs, ", ") + ")"
}
