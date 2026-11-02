// pail-actions is the T0.4 spike of the pail Actions orchestrator and runner.
//
//	pail-actions plan --event push --ref main .github/workflows
//	pail-actions run-local --store file:///path/to/store --commit <sha> --job test workflow.yml
package main

import (
	"flag"
	"fmt"
	"os"
	"strings"

	"github.com/ArchAstro/pail/actions/internal/plan"
	"github.com/ArchAstro/pail/actions/internal/runlocal"
)

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	var err error
	switch os.Args[1] {
	case "plan":
		err = cmdPlan(os.Args[2:])
	case "run-local":
		err = cmdRunLocal(os.Args[2:])
	case "serve":
		err = cmdServe(os.Args[2:])
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
  pail-actions serve --store <url> [--pail pail] [--replica <name>] [--default-branch main] [--safety-poll 10] [--interval 10s]
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
