package orchestrator

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"math/rand"
	"os/exec"
	"sort"
	"sync"
	"time"

	"github.com/ArchAstro/pail/actions/internal/tier1"
)

// Orchestrator is one stateless replica. Everything it remembers (the triggers it mapped but
// has not yet landed, a cache of blobs by id) is an optimisation or a to-do list that a restart
// rebuilds from the next events; correctness lives in the guards of the commit.
type Orchestrator struct {
	Pail   *tier1.Pail
	Mapper *Mapper
	Tier2  Tier2View
	Now    func() time.Time
	Log    func(format string, a ...any)

	mu      sync.Mutex
	raw     []RefEvent        // events not yet mapped
	pending []Trigger         // mapped, not yet resolved (claimed or dropped)
	acks    map[string]string // trigger key -> dispatch uuid to delete once resolved
	wake    chan struct{}
}

func New(p *tier1.Pail, defaultBranch string) *Orchestrator {
	o := &Orchestrator{Pail: p, Tier2: NoTier2{}, Now: time.Now, acks: map[string]string{}, wake: make(chan struct{}, 1)}
	o.Mapper = &Mapper{Git: p, Default: defaultBranch, Log: o.logf}
	return o
}

func (o *Orchestrator) logf(format string, a ...any) {
	if o.Log != nil {
		o.Log(format, a...)
	}
}

// Notify raises the wake flag. A tick answers every pending wake-up at once, so repeats and
// reorderings change nothing (docs/actions.md 7.1).
func (o *Orchestrator) Notify() {
	select {
	case o.wake <- struct{}{}:
	default:
	}
}

// Observe takes one `pail events` line: a ref change is kept for mapping, anything wakes.
func (o *Orchestrator) Observe(ev RefEvent) {
	if ev.IsRef() {
		o.mu.Lock()
		o.raw = append(o.raw, ev)
		o.mu.Unlock()
	}
	o.Notify()
}

// TickResult says what a tick did.
type TickResult struct {
	Landed   bool     // a commit landed on _actions
	Checks   bool     // a commit landed on _checks
	Conflict bool     // a guard failed: tick again from a fresh read
	Intents  []string // what was decided
}

// Tick reads, decides and commits once (docs/actions.md 7.1).
func (o *Orchestrator) Tick(ctx context.Context) (TickResult, error) {
	var res TickResult
	if err := o.mapEvents(ctx); err != nil {
		return res, err
	}
	if err := o.mapDispatches(ctx); err != nil {
		return res, err
	}
	snap, err := o.Pail.Snapshot(ctx, tier1.ActionsBranch, LoadActions)
	if err != nil {
		return res, err
	}
	o.mu.Lock()
	triggers := append([]Trigger(nil), o.pending...)
	o.mu.Unlock()
	dec := Decide(snap, Input{Now: o.Now(), Triggers: triggers})
	res.Intents = dec.Intents
	if !dec.Tx.Empty() {
		err := o.Pail.Put(ctx, tier1.ActionsBranch, commitMessage(dec), dec.Tx.Puts())
		if errors.Is(err, tier1.ErrConflict) {
			res.Conflict = true // the triggers stay pending; the next tick decides again
			o.logf("conflict on %s; re-reading", tier1.ActionsBranch)
			return res, nil
		}
		if err != nil {
			return res, err
		}
		res.Landed = true
		o.logf("landed %d file(s) on %s: %v", len(dec.Tx.Written()), tier1.ActionsBranch, dec.Intents)
	}
	o.resolve(ctx, dec.Done)

	chk, err := o.projectChecks(ctx, snap)
	res.Checks = chk
	return res, err
}

func commitMessage(d Decision) string {
	msg := "actions: tick"
	if len(d.Intents) == 1 {
		msg = "actions: " + d.Intents[0]
	} else if len(d.Intents) > 1 {
		msg = fmt.Sprintf("actions: %d intents", len(d.Intents))
		for _, i := range d.Intents {
			msg += "\n- " + i
		}
	}
	return msg
}

func (o *Orchestrator) projectChecks(ctx context.Context, actions *tier1.Snapshot) (bool, error) {
	if len(actions.Files) == 0 {
		return false, nil
	}
	cs, err := o.Pail.Snapshot(ctx, tier1.ChecksBranch, LoadChecks)
	if err != nil {
		return false, err
	}
	tx, intents := DecideChecks(actions, cs, o.Now())
	if tx.Empty() {
		return false, nil
	}
	msg := fmt.Sprintf("checks: %d projected", len(intents))
	err = o.Pail.Put(ctx, tier1.ChecksBranch, msg, tx.Puts())
	if errors.Is(err, tier1.ErrConflict) {
		return false, nil // the next tick reconciles from a fresh read
	}
	if err == nil {
		o.logf("%s", msg)
	}
	return err == nil, err
}

// resolve drops the triggers that are claimed or void, and acknowledges their dispatch requests.
func (o *Orchestrator) resolve(ctx context.Context, done []string) {
	set := map[string]bool{}
	for _, k := range done {
		set[k] = true
	}
	o.mu.Lock()
	keep := o.pending[:0]
	var ack []string
	for _, t := range o.pending {
		if set[t.Key] {
			if u, ok := o.acks[t.Key]; ok {
				ack = append(ack, u)
				delete(o.acks, t.Key)
			}
			continue
		}
		keep = append(keep, t)
	}
	o.pending = keep
	o.mu.Unlock()
	for _, u := range ack {
		if err := o.Tier2.Ack(ctx, u); err != nil {
			o.logf("ack dispatch %s: %v", u, err)
		}
	}
}

func (o *Orchestrator) addPending(ts []Trigger) {
	o.mu.Lock()
	defer o.mu.Unlock()
	have := map[string]bool{}
	for _, t := range o.pending {
		have[t.Key] = true
	}
	for _, t := range ts {
		if !have[t.Key] {
			o.pending = append(o.pending, t)
			have[t.Key] = true
		}
	}
}

func (o *Orchestrator) mapEvents(ctx context.Context) error {
	for {
		o.mu.Lock()
		if len(o.raw) == 0 {
			o.mu.Unlock()
			return nil
		}
		ev := o.raw[0]
		o.mu.Unlock()
		ts, err := o.Mapper.Map(ctx, ev)
		if err != nil {
			// Stays queued and is retried next tick; a poison event is dropped after a few
			// tries so that it cannot block the rest (a lost trigger costs a re-push).
			o.mu.Lock()
			o.raw[0].tries++
			if o.raw[0].tries >= 5 {
				o.logf("dropping %s event after %d failures: %v", ev.Branch, o.raw[0].tries, err)
				o.raw = o.raw[1:]
			}
			o.mu.Unlock()
			return fmt.Errorf("map %s event: %w", ev.Branch, err)
		}
		o.addPending(ts)
		o.mu.Lock()
		o.raw = o.raw[1:]
		o.mu.Unlock()
	}
}

func (o *Orchestrator) mapDispatches(ctx context.Context) error {
	reqs, err := o.Tier2.Dispatches(ctx)
	if err != nil {
		return err
	}
	for _, r := range reqs {
		ts, err := o.Mapper.Dispatch(ctx, r)
		if err != nil {
			return err
		}
		o.mu.Lock()
		if len(ts) == 0 { // a request that triggers nothing is answered (and deleted) too
			o.acks["void/"+r.UUID] = r.UUID
		}
		for _, t := range ts {
			o.acks[t.Key] = r.UUID
		}
		o.mu.Unlock()
		o.addPending(ts)
		if len(ts) == 0 {
			o.resolveVoid(ctx, "void/"+r.UUID)
		}
	}
	return nil
}

func (o *Orchestrator) resolveVoid(ctx context.Context, key string) {
	o.mu.Lock()
	u := o.acks[key]
	delete(o.acks, key)
	o.mu.Unlock()
	if u != "" {
		_ = o.Tier2.Ack(ctx, u)
	}
}

// Drain ticks until nothing lands, riding out conflicts (each retry re-reads everything).
func (o *Orchestrator) Drain(ctx context.Context) error {
	conflicts := 0
	for i := 0; i < 50; i++ {
		res, err := o.Tick(ctx)
		if err != nil {
			return err
		}
		switch {
		case res.Conflict:
			if conflicts++; conflicts > 10 {
				return errors.New("orchestrator: too many conflicts")
			}
			select {
			case <-time.After(time.Duration(20+rand.Intn(200)) * time.Millisecond):
			case <-ctx.Done():
				return ctx.Err()
			}
		case res.Landed || res.Checks:
			// our own writes may have made more work due (a promotion, a finish): go again
		default:
			return nil
		}
	}
	return nil
}

// Serve runs the replica: `pail events` wakes it, a timer wakes it too (a lapsed lease
// announces nothing, 7.1), and each wake is one Drain.
func (o *Orchestrator) Serve(ctx context.Context, replica string, safetyPoll int, interval time.Duration) error {
	go o.watchEvents(ctx, replica, safetyPoll)
	t := time.NewTicker(interval)
	defer t.Stop()
	o.Notify() // start-up: tick once
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-o.wake:
		case <-t.C:
		}
		if err := o.Drain(ctx); err != nil && ctx.Err() == nil {
			o.logf("tick: %v", err)
		}
	}
}

// watchEvents runs `pail events` for this replica and restarts it when it dies.
func (o *Orchestrator) watchEvents(ctx context.Context, replica string, safetyPoll int) {
	for ctx.Err() == nil {
		if err := o.runEvents(ctx, replica, safetyPoll); err != nil && ctx.Err() == nil {
			o.logf("pail events: %v (restarting)", err)
		}
		select {
		case <-ctx.Done():
		case <-time.After(2 * time.Second):
		}
	}
}

func (o *Orchestrator) runEvents(ctx context.Context, replica string, safetyPoll int) error {
	args := []string{"--store", o.Pail.Store, "events", "--branch", "*", "--prefix", "actions/",
		"--cursor", "bucket:actions/cursors/" + replica, "--safety-poll", fmt.Sprint(safetyPoll)}
	cmd := exec.CommandContext(ctx, binOr(o.Pail.Bin), args...)
	cmd.Env = append(cmd.Environ(), o.Pail.Env...)
	out, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		return err
	}
	sc := bufio.NewScanner(out)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	for sc.Scan() {
		ev, err := ParseEvent(sc.Bytes())
		if err != nil {
			o.logf("pail events: unparseable line %q", sc.Text())
			o.Notify()
			continue
		}
		o.Observe(ev)
	}
	return cmd.Wait()
}

func binOr(b string) string {
	if b == "" {
		return "pail"
	}
	return b
}

// PendingKeys lists the triggers waiting to be claimed (for tests and logs).
func (o *Orchestrator) PendingKeys() []string {
	o.mu.Lock()
	defer o.mu.Unlock()
	var ks []string
	for _, t := range o.pending {
		ks = append(ks, t.Key)
	}
	sort.Strings(ks)
	return ks
}
