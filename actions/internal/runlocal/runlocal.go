// Package runlocal runs one workflow job on this machine with act's runner, with
// actions/checkout swapped for `pail clone --at <commit>`.
package runlocal

import (
	"context"
	"fmt"
	"io"
	"net/url"
	"os"
	"strings"

	"github.com/nektos/act/pkg/model"
	"github.com/nektos/act/pkg/runner"
	"github.com/sirupsen/logrus"
)

type Options struct {
	Workflow  string
	Job       string
	Event     string
	Store     string // pail store url
	Commit    string // revision for pail clone --at
	Pail      string // pail binary as the job sees it (host mode: on PATH or absolute)
	Host      bool   // run steps on this machine (act's "-self-hosted" platform)
	Image     string // docker image for every runs-on label (docker mode)
	LinuxPail string // linux pail binary mounted at /usr/local/bin/pail (docker mode)
	Workdir   string // empty directory the workspace is copied from; default: a new temp dir
	Out       io.Writer
}

// SwapCheckout replaces every `uses: actions/checkout@...` step of the workflow with a run
// step that clones the store at the commit into the workspace. It returns the number of
// steps swapped. `with:` inputs (ref, path, fetch-depth, token, submodules) are dropped: the
// commit is fixed by the queue item, which is the point.
func SwapCheckout(w *model.Workflow, pail, store, commit string) int {
	n := 0
	for _, j := range w.Jobs {
		for _, s := range j.Steps {
			if strings.HasPrefix(s.Uses, "actions/checkout@") {
				s.Uses = ""
				s.With = nil
				s.Run = fmt.Sprintf(`%s clone --at %s %s "$GITHUB_WORKSPACE"`, quote(pail), quote(commit), quote(store))
				if s.Name == "" {
					s.Name = "pail clone"
				}
				n++
			}
		}
	}
	return n
}

// DockerOptions are the container options that make `pail clone` work inside the job
// container: the linux pail binary, and the store directory for file:// stores.
func DockerOptions(store, linuxPail string) string {
	opts := []string{"-v " + linuxPail + ":/usr/local/bin/pail:ro"}
	if u, err := url.Parse(store); err == nil && u.Scheme == "file" {
		opts = append(opts, "-v "+u.Path+":"+u.Path+":ro")
	}
	return strings.Join(opts, " ")
}

func quote(s string) string { return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'" }

func Run(o Options) error {
	if o.Out == nil {
		o.Out = os.Stdout
	}
	f, err := os.Open(o.Workflow)
	if err != nil {
		return err
	}
	w, err := model.ReadWorkflow(f, false)
	f.Close()
	if err != nil {
		return err
	}
	job := w.GetJob(o.Job)
	if job == nil {
		return fmt.Errorf("no job %q in %s", o.Job, o.Workflow)
	}
	pail := o.Pail
	if !o.Host {
		pail = "/usr/local/bin/pail"
	}
	if SwapCheckout(w, pail, o.Store, o.Commit) == 0 {
		fmt.Fprintln(o.Out, "note: the workflow has no actions/checkout step")
	}
	workdir := o.Workdir
	if workdir == "" {
		if workdir, err = os.MkdirTemp("", "pail-actions-run-"); err != nil {
			return err
		}
		defer os.RemoveAll(workdir)
	}

	cfg := &runner.Config{
		Workdir:        workdir,
		EventName:      o.Event,
		Platforms:      map[string]string{},
		LogOutput:      true,
		AutoRemove:     true,
		ConcurrentJobs: 1,
		Env:            map[string]string{},
	}
	for _, label := range job.RunsOn() {
		switch {
		case o.Host:
			cfg.Platforms[strings.ToLower(label)] = "-self-hosted"
		default:
			cfg.Platforms[strings.ToLower(label)] = o.Image
		}
	}
	if !o.Host {
		if o.LinuxPail == "" {
			return fmt.Errorf("docker mode needs --linux-pail, a linux pail binary to mount into the container")
		}
		cfg.ContainerOptions = DockerOptions(o.Store, o.LinuxPail)
	}

	// act's JobLoggerFactory is the seam where a runner captures job logs (here: o.Out; in the
	// product: Tier 2 chunks).
	sink := &filter{w: o.Out}
	ctx := runner.WithJobLoggerFactory(context.Background(), factory{sink})
	logrus.SetOutput(sink)
	r, err := runner.New(cfg)
	if err != nil {
		return err
	}
	p := &model.Plan{Stages: []*model.Stage{{Runs: []*model.Run{{Workflow: w, JobID: o.Job}}}}}
	return r.NewPlanExecutor(p)(ctx)
}

type factory struct{ w io.Writer }

func (f factory) WithJobLogger() *logrus.Logger {
	l := logrus.New()
	l.SetOutput(f.w)
	l.SetFormatter(&logrus.TextFormatter{DisableTimestamp: true, DisableColors: true})
	l.SetLevel(logrus.InfoLevel)
	return l
}

// filter drops act's complaints about the workspace not being a git repository: pail
// workspaces are not, and the commit comes from the queue item instead.
type filter struct{ w io.Writer }

func (f *filter) Write(p []byte) (int, error) {
	s := string(p)
	if strings.Contains(s, "not located inside a git repository") || strings.Contains(s, "unable to get git") {
		return len(p), nil
	}
	return f.w.Write(p)
}
