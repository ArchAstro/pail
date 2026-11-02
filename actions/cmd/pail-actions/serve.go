package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"time"

	"github.com/ArchAstro/pail/actions/internal/orchestrator"
	"github.com/ArchAstro/pail/actions/internal/tier1"
)

// cmdServe runs one orchestrator replica (docs/actions.md 7). Replicas are interchangeable;
// give each its own --replica (its event cursor), and keep it across restarts.
func cmdServe(args []string) error {
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	var store, bin, replica, def string
	var poll int
	var interval time.Duration
	host, _ := os.Hostname()
	fs.StringVar(&store, "store", "", "pail store url (file://, s3://, gs://)")
	fs.StringVar(&bin, "pail", "pail", "pail binary")
	fs.StringVar(&replica, "replica", host, "replica name: its events cursor is actions/cursors/<replica>")
	fs.StringVar(&def, "default-branch", "main", "branch whose workflows run for pr/* branches and PRs (D7)")
	fs.IntVar(&poll, "safety-poll", 10, "pail events --safety-poll seconds (must stay on, 7.1)")
	fs.DurationVar(&interval, "interval", 10*time.Second, "tick at least this often (a lapsed lease announces nothing)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if store == "" || replica == "" {
		usage()
	}
	env := []string{}
	if os.Getenv("PAIL_AUTHOR") == "" {
		env = append(env, "PAIL_AUTHOR=pail-actions <actions@pail.local>")
	}
	p := &tier1.Pail{Bin: bin, Store: store, Env: env}
	o := orchestrator.New(p, def)
	o.Mapper.Repo = filepath.Base(store)
	o.Log = log.New(os.Stderr, "pail-actions: ", log.LstdFlags).Printf
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	o.Log("serving %s as replica %s", store, replica)
	if err := o.Serve(ctx, replica, poll, interval); err != nil {
		return fmt.Errorf("serve: %w", err)
	}
	return nil
}
