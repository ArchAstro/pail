package runner

import (
	"context"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"

	"github.com/nektos/act/pkg/model"
	actrunner "github.com/nektos/act/pkg/runner"
	"github.com/sirupsen/logrus"
	"gopkg.in/yaml.v3"

	"github.com/ArchAstro/pail/actions/internal/runlocal"
	"github.com/ArchAstro/pail/actions/internal/tier2"
)

// ActExecutor runs a queue entry with nektos/act as a library. The entry is turned into a
// one-job workflow; actions/checkout is swapped for `pail clone --at <sha>`; act's github
// context comes from the entry, not from a local git repository.
type ActExecutor struct {
	Pail      string // pail binary the checkout step runs (host mode: path on this machine)
	Host      bool   // run steps on this machine (act "-self-hosted") instead of in docker
	Image     string // docker image for every runs-on label (docker mode)
	LinuxPail string // linux pail binary mounted into containers (docker mode)
}

// Workflow synthesizes the one-job workflow YAML for an entry.
func Workflow(e tier2.QueueEntry, label string) ([]byte, error) {
	runsOn := e.RunsOn
	if len(runsOn) == 0 {
		runsOn = []string{label}
	}
	job := map[string]any{"runs-on": runsOn, "steps": e.Steps}
	if e.Name != "" {
		job["name"] = e.Name
	}
	if len(e.Env) > 0 {
		job["env"] = e.Env
	}
	if e.Container != nil {
		job["container"] = e.Container
	}
	if len(e.Services) > 0 {
		job["services"] = e.Services
	}
	if len(e.Defaults) > 0 {
		job["defaults"] = e.Defaults
	}
	event := e.Github.EventName
	if event == "" {
		event = "push"
	}
	return yaml.Marshal(map[string]any{
		"name": e.Job,
		"on":   map[string]any{event: map[string]any{}},
		"jobs": map[string]any{e.Job: job},
	})
}

func (a *ActExecutor) Execute(ctx context.Context, j *Job) (string, error) {
	e := j.Entry
	src, err := Workflow(e, j.Label)
	if err != nil {
		return tier2.Failure, err
	}
	w, err := model.ReadWorkflow(strings.NewReader(string(src)), false)
	if err != nil {
		return tier2.Failure, fmt.Errorf("invalid job: %w", err)
	}
	job := w.GetJob(e.Job)
	if job == nil {
		return tier2.Failure, fmt.Errorf("no job %q", e.Job)
	}
	pail := a.Pail
	if pail == "" {
		pail = "pail"
	}
	if !a.Host {
		pail = "/usr/local/bin/pail"
	}
	if runlocal.SwapCheckout(w, pail, e.Store, e.Sha) == 0 {
		fmt.Fprintln(j.Log, "note: the job has no actions/checkout step")
	}
	workdir, err := os.MkdirTemp("", "pail-runner-")
	if err != nil {
		return tier2.Failure, err
	}
	defer os.RemoveAll(workdir)

	g := e.Github
	env := map[string]string{
		"GITHUB_RUN_ID":      strconv.FormatInt(e.Run, 10),
		"GITHUB_RUN_ATTEMPT": strconv.Itoa(e.Attempt),
		"GITHUB_REPOSITORY":  g.Repository,
		"GITHUB_REF":         g.Ref,
		"SHA_REF":            e.Sha, // act's key for github.sha
	}
	if g.RunNumber != 0 {
		env["GITHUB_RUN_NUMBER"] = strconv.FormatInt(g.RunNumber, 10)
	}
	if g.HeadRef != "" {
		env["GITHUB_HEAD_REF"] = g.HeadRef
	}
	if g.BaseRef != "" {
		env["GITHUB_BASE_REF"] = g.BaseRef
	}
	cfg := &actrunner.Config{
		Workdir:        workdir,
		EventName:      g.EventName,
		Actor:          g.Actor,
		Platforms:      map[string]string{},
		LogOutput:      true,
		AutoRemove:     true,
		ConcurrentJobs: 1,
		Env:            env,
	}
	for _, label := range job.RunsOn() {
		if a.Host {
			cfg.Platforms[strings.ToLower(label)] = "-self-hosted"
		} else {
			cfg.Platforms[strings.ToLower(label)] = a.Image
		}
	}
	if !a.Host {
		if a.LinuxPail == "" {
			return tier2.Failure, fmt.Errorf("docker mode needs --linux-pail, a linux pail binary to mount into the container")
		}
		cfg.ContainerOptions = runlocal.DockerOptions(e.Store, a.LinuxPail)
	}

	sink := &noiseFilter{w: j.Log}
	logrus.SetOutput(sink)
	ctx = actrunner.WithJobLoggerFactory(ctx, loggerFactory{w: sink, steps: j.Steps})
	r, err := actrunner.New(cfg)
	if err != nil {
		return tier2.Failure, err
	}
	p := &model.Plan{Stages: []*model.Stage{{Runs: []*model.Run{{Workflow: w, JobID: e.Job}}}}}
	if err := r.NewPlanExecutor(p)(ctx); err != nil {
		return tier2.Failure, err
	}
	return tier2.Success, nil
}

// loggerFactory gives act a logger that writes job output to w and feeds step events to the
// tracker. Debug entries are dropped from the output but still reach the hook, because act
// reports skipped steps only at debug level.
type loggerFactory struct {
	w     io.Writer
	steps *Tracker
}

func (f loggerFactory) WithJobLogger() *logrus.Logger {
	l := logrus.New()
	l.SetOutput(f.w)
	l.SetFormatter(plainFormatter{})
	l.SetLevel(logrus.DebugLevel)
	l.AddHook(stepHook{f.steps})
	return l
}

// plainFormatter prints job output as plain lines (like the GitHub log): step output as it
// came, act's own progress lines as their message. Debug entries print nothing.
type plainFormatter struct{}

func (plainFormatter) Format(e *logrus.Entry) ([]byte, error) {
	if e.Level > logrus.InfoLevel {
		return nil, nil
	}
	if _, raw := e.Data["raw_output"]; raw {
		return []byte(e.Message), nil
	}
	return []byte(e.Message + "\n"), nil
}

type stepHook struct{ t *Tracker }

func (stepHook) Levels() []logrus.Level { return logrus.AllLevels }

func (h stepHook) Fire(e *logrus.Entry) error {
	ids, _ := e.Data["stepID"].([]string)
	if len(ids) != 1 || fmt.Sprint(e.Data["stage"]) != "Main" {
		return nil
	}
	id := ids[0]
	if res, ok := e.Data["stepResult"]; ok {
		h.t.Finish(id, fmt.Sprint(res))
	} else if strings.HasPrefix(e.Message, "⭐ Run Main") {
		h.t.Start(id)
	}
	return nil
}

// noiseFilter drops act's complaints about the workspace not being a git repository: pail
// workspaces are not, and the commit comes from the queue entry.
type noiseFilter struct{ w io.Writer }

func (f *noiseFilter) Write(p []byte) (int, error) {
	s := string(p)
	if strings.Contains(s, "not located inside a git repository") || strings.Contains(s, "unable to get git") {
		return len(p), nil
	}
	return f.w.Write(p)
}
