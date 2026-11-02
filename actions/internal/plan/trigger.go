package plan

import (
	"fmt"
	"regexp"
	"sort"
	"strings"

	"github.com/nektos/act/pkg/exprparser"
	"github.com/nektos/act/pkg/model"
	"github.com/nektos/act/pkg/workflowpattern"
)

func triggers(w *model.Workflow) []string {
	ev := w.On()
	sort.Strings(ev)
	return ev
}

var defaultTypes = map[string][]string{
	"pull_request":        {"opened", "synchronize", "reopened"},
	"pull_request_target": {"opened", "synchronize", "reopened"},
}

// Matches says whether the event triggers the workflow and why. Filters follow GitHub's
// rules: act's model only lists event names, so this is the part we add.
func Matches(w *model.Workflow, in Input) (bool, string) {
	found := false
	for _, e := range w.On() {
		found = found || e == in.Event
	}
	if !found {
		return false, fmt.Sprintf("%s is not in on:", in.Event)
	}
	cfg, _ := w.OnEvent(in.Event).(map[string]any)
	if in.Event == "schedule" {
		return matchSchedule(w, in)
	}
	if t, ok := strs(cfg, "types"); ok || defaultTypes[in.Event] != nil {
		if !ok {
			t = defaultTypes[in.Event]
		}
		if in.Type != "" && !contains(t, in.Type) {
			return false, fmt.Sprintf("type %s is not in types %v", in.Type, t)
		}
	}
	switch in.Event {
	case "push":
		kind, name := refKind(in.Ref)
		if in.Ref == "" {
			return false, "push needs a ref"
		}
		bf, bset := filter(cfg, "branches", "branches-ignore")
		tf, tset := filter(cfg, "tags", "tags-ignore")
		switch {
		case kind == "branch" && bset:
			if !bf(name) {
				return false, fmt.Sprintf("branch %s does not match the branches filter", name)
			}
		case kind == "branch" && tset:
			return false, "only tags are filtered: a branch push does not trigger"
		case kind == "tag" && tset:
			if !tf(name) {
				return false, fmt.Sprintf("tag %s does not match the tags filter", name)
			}
		case kind == "tag" && bset:
			return false, "only branches are filtered: a tag push does not trigger"
		}
		if kind == "tag" {
			return true, "push to tag " + name // path filters are not evaluated for tags
		}
		if ok, why := matchPaths(cfg, in.Paths); !ok {
			return false, why
		}
		return true, "push to branch " + name
	case "pull_request", "pull_request_target":
		if bf, set := filter(cfg, "branches", "branches-ignore"); set && in.Ref != "" {
			_, name := refKind(in.Ref)
			if !bf(name) {
				return false, fmt.Sprintf("base branch %s does not match the branches filter", name)
			}
		}
		if ok, why := matchPaths(cfg, in.Paths); !ok {
			return false, why
		}
		return true, in.Event + " matches"
	}
	return true, in.Event + " is in on:"
}

func matchSchedule(w *model.Workflow, in Input) (bool, string) {
	list, _ := w.OnEvent("schedule").([]any)
	var crons []string
	for _, e := range list {
		if m, ok := e.(map[string]any); ok {
			crons = append(crons, fmt.Sprint(m["cron"]))
		}
	}
	if in.Cron == "" {
		return true, "schedule is in on:"
	}
	if contains(crons, in.Cron) {
		return true, "cron " + in.Cron
	}
	return false, fmt.Sprintf("cron %q is not one of %v", in.Cron, crons)
}

func matchPaths(cfg map[string]any, changed []string) (bool, string) {
	pf, set := filter(cfg, "paths", "paths-ignore")
	if !set || changed == nil {
		return true, ""
	}
	// paths: at least one changed file matches; paths-ignore: at least one file is not ignored.
	for _, f := range changed {
		if pf(f) {
			return true, ""
		}
	}
	return false, "no changed file passes the paths filter"
}

// filter returns a matcher for an include key (with ! negations) or its -ignore twin, and
// whether either is set. GitHub rejects both set; the include key wins here.
func filter(cfg map[string]any, include, ignore string) (func(string) bool, bool) {
	if p, ok := strs(cfg, include); ok {
		pats, err := workflowpattern.CompilePatterns(p...)
		if err != nil {
			return func(string) bool { return false }, true
		}
		return func(s string) bool { return !workflowpattern.Skip(pats, []string{s}, nopTrace{}) }, true
	}
	if p, ok := strs(cfg, ignore); ok {
		pats, err := workflowpattern.CompilePatterns(p...)
		if err != nil {
			return func(string) bool { return false }, true
		}
		return func(s string) bool { return !workflowpattern.Filter(pats, []string{s}, nopTrace{}) }, true
	}
	return nil, false
}

type nopTrace struct{}

func (nopTrace) Info(string, ...any) {}

func strs(cfg map[string]any, key string) ([]string, bool) {
	v, ok := cfg[key]
	if !ok {
		return nil, false
	}
	switch t := v.(type) {
	case string:
		return []string{t}, true
	case []any:
		var out []string
		for _, e := range t {
			out = append(out, fmt.Sprint(e))
		}
		return out, true
	}
	return nil, true
}

func contains(l []string, s string) bool {
	for _, e := range l {
		if e == s {
			return true
		}
	}
	return false
}

func refKind(ref string) (kind, name string) {
	switch {
	case strings.HasPrefix(ref, "refs/tags/"):
		return "tag", strings.TrimPrefix(ref, "refs/tags/")
	case strings.HasPrefix(ref, "refs/heads/"):
		return "branch", strings.TrimPrefix(ref, "refs/heads/")
	}
	return "branch", ref
}

var (
	githubRef      = regexp.MustCompile(`\bgithub\.(\w+)`)
	deferredCtx    = regexp.MustCompile(`\b(needs|steps|job|jobs|runner|secrets|vars|inputs|env)\.|\b(success|failure|always|cancelled)\s*\(|\bgithub\.event\.|\bgithub\[`)
	staticGithub   = map[string]bool{"event_name": true, "ref": true, "ref_name": true, "ref_type": true}
	matrixRef      = regexp.MustCompile(`\bmatrix\.`)
	pullRequestish = map[string]bool{"pull_request": true, "pull_request_target": true}
)

// staticIf evaluates a job `if:` with act's expression engine when it only reads the event
// name, the ref and the job's own matrix values. Anything else is deferred to run time
// (needs results, step outputs, event payload, secrets, status functions).
func staticIf(expr string, in Input, matrix map[string]any) string {
	if expr == "" {
		return ""
	}
	expr = strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(expr), "${{")), "}}"))
	if deferredCtx.MatchString(expr) || (matrixRef.MatchString(expr) && matrix == nil) {
		return "deferred"
	}
	for _, m := range githubRef.FindAllStringSubmatch(expr, -1) {
		if !staticGithub[m[1]] {
			return "deferred"
		}
		if m[1] != "event_name" && pullRequestish[in.Event] {
			return "deferred"
		}
	}
	kind, name := refKind(in.Ref)
	ref := "refs/heads/" + name
	if kind == "tag" {
		ref = "refs/tags/" + name
	}
	env := &exprparser.EvaluationEnvironment{
		Github: &model.GithubContext{EventName: in.Event, Ref: ref, RefName: name, RefType: map[string]string{"branch": "branch", "tag": "tag"}[kind]},
		Matrix: matrix,
	}
	v, err := exprparser.NewInterpeter(env, exprparser.Config{}).Evaluate(expr, exprparser.DefaultStatusCheckNone)
	if err != nil {
		return "deferred"
	}
	if exprparser.IsTruthy(v) {
		return "true"
	}
	return "false"
}
