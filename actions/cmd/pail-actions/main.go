// pail-actions is the T0.4 spike of the pail Actions orchestrator and runner.
//
//	pail-actions plan --event push --ref main .github/workflows
//	pail-actions run-local --store file:///path/to/store --commit <sha> --job test workflow.yml
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/ArchAstro/pail/actions/internal/plan"
	"github.com/ArchAstro/pail/actions/internal/runlocal"
	"github.com/ArchAstro/pail/actions/internal/runner"
	"github.com/ArchAstro/pail/actions/internal/tier2"
)

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	var err error
	switch os.Args[1] {
	case "plan":
		err = cmdPlan(os.Args[2:])
	case "runner":
		err = cmdRunner(os.Args[2:])
	case "run-local":
		err = cmdRunLocal(os.Args[2:])
	default:
		usage()
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "pail-actions:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintln(os.Stderr, `usage:
  pail-actions plan --event <name> [--ref <branch|refs/tags/x>] [--paths a,b] [--cron "<expr>"] [--type <activity>] <workflow file or dir>
  pail-actions runner --store <url> --labels a,b [--name id] [--once] [--pail pail] [--host | --image <img> --linux-pail <bin>] [--ttl 90s --refresh 15s]
  pail-actions run-local --store <url> --commit <rev> --job <id> [--pail pail] [--host | --image <img>] <workflow file>`)
	os.Exit(2)
}

func cmdPlan(args []string) error {
	fs := flag.NewFlagSet("plan", flag.ExitOnError)
	in := plan.Input{}
	var paths string
	fs.StringVar(&in.Event, "event", "push", "event name")
	fs.StringVar(&in.Ref, "ref", "", "branch (or refs/tags/x); for pull_request the base branch")
	fs.StringVar(&paths, "paths", "", "comma-separated changed files; omit to skip path filters")
	fs.StringVar(&in.Cron, "cron", "", "for schedule: the cron entry that fired")
	fs.StringVar(&in.Type, "type", "", "activity type (opened, labeled, ...)")
	// Flags may follow the path.
	var rest []string
	if err := parseInterspersed(fs, args, &rest); err != nil {
		return err
	}
	if len(rest) != 1 {
		usage()
	}
	if paths != "" {
		in.Paths = strings.Split(paths, ",")
	}
	plans, err := plan.Path(rest[0], in)
	if err != nil {
		return err
	}
	out, err := plan.JSON(map[string]any{"input": in, "workflows": plans})
	if err != nil {
		return err
	}
	fmt.Println(string(out))
	return nil
}

func parseInterspersed(fs *flag.FlagSet, args []string, rest *[]string) error {
	for len(args) > 0 {
		if err := fs.Parse(args); err != nil {
			return err
		}
		args = fs.Args()
		if len(args) > 0 {
			*rest = append(*rest, args[0])
			args = args[1:]
		}
	}
	return nil
}

func cmdRunLocal(args []string) error {
	fs := flag.NewFlagSet("run-local", flag.ExitOnError)
	o := runlocal.Options{}
	fs.StringVar(&o.Store, "store", "", "pail store url (file://, s3://, gs://)")
	fs.StringVar(&o.Commit, "commit", "", "commit to check out (pail clone --at)")
	fs.StringVar(&o.Job, "job", "", "job id to run")
	fs.StringVar(&o.Event, "event", "push", "event name")
	fs.StringVar(&o.Pail, "pail", "pail", "pail binary used for checkout")
	fs.BoolVar(&o.Host, "host", false, "run steps on this machine instead of in docker")
	fs.StringVar(&o.Image, "image", "debian:stable-slim", "docker image for runs-on")
	fs.StringVar(&o.LinuxPail, "linux-pail", "", "linux pail binary to mount into the container (docker mode)")
	var rest []string
	if err := parseInterspersed(fs, args, &rest); err != nil {
		return err
	}
	if len(rest) != 1 || o.Store == "" || o.Commit == "" || o.Job == "" {
		usage()
	}
	o.Workflow = rest[0]
	return runlocal.Run(o)
}

func cmdRunner(args []string) error {
	fs := flag.NewFlagSet("runner", flag.ExitOnError)
	var store, labels, name, pail, image, linux string
	var once, host bool
	var ttl, refresh, poll time.Duration
	fs.StringVar(&store, "store", "", "repo store url (gs://B/repo, file:///path); its .actions area is Tier 2")
	fs.StringVar(&labels, "labels", "", "comma-separated runs-on labels to serve")
	fs.StringVar(&name, "name", "", "runner id (default host-pid)")
	fs.BoolVar(&once, "once", false, "run one job, then exit")
	fs.StringVar(&pail, "pail", envOr("PAIL_BIN", "pail"), "pail binary (Tier 2 access and checkout)")
	fs.BoolVar(&host, "host", false, "run steps on this machine instead of in docker")
	fs.StringVar(&image, "image", "debian:stable-slim", "docker image for runs-on (docker mode)")
	fs.StringVar(&linux, "linux-pail", "", "linux pail binary to mount into containers (docker mode)")
	fs.DurationVar(&ttl, "ttl", 90*time.Second, "lease ttl")
	fs.DurationVar(&refresh, "refresh", 15*time.Second, "lease refresh / progress / cancel-check interval")
	fs.DurationVar(&poll, "poll", 2*time.Second, "queue poll interval when idle")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if store == "" || labels == "" {
		usage()
	}
	ps := &tier2.PailStore{Pail: pail, URL: store}
	r := runner.New(runner.Config{
		Store: ps, Labels: strings.Split(labels, ","), Name: name, Once: once,
		TTL: ttl, Refresh: refresh, Poll: poll,
		Exec: &runner.ActExecutor{Pail: pail, Host: host, Image: image, LinuxPail: linux},
	})
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return r.Run(ctx)
}

func envOr(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}
