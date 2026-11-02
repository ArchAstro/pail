// Package runner is the pail Actions runner daemon: it pulls queue entries from Tier 2,
// claims a lease, runs the job with an Executor (act), and reports through Tier 2 only.
// See docs/actions.md sections 5 and 8. It never reads or writes Tier 1.
package runner

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sync/atomic"
	"time"

	"github.com/ArchAstro/pail/actions/internal/tier2"
)

const Version = "0.1.0"

// Job is what an Executor runs.
type Job struct {
	Entry tier2.QueueEntry
	Label string // the queue label the entry was claimed under
	Seq   int
	Log   io.Writer // job output; the runner chunks it into Tier 2
	Steps *Tracker  // step progress sink
}

// Executor runs one job to completion. It returns success or failure; a non-nil error is a
// failure with that reason (already logged by the runner). It must stop when ctx is done.
type Executor interface {
	Execute(ctx context.Context, j *Job) (conclusion string, err error)
}

// Config configures a Runner. Zero durations take the section 5.2 defaults.
type Config struct {
	Store   tier2.Store
	Labels  []string
	Name    string
	Once    bool // run one job, then return
	Exec    Executor
	Clock   tier2.Clock
	Version string

	TTL      time.Duration // lease ttl, default 90s
	Refresh  time.Duration // lease refresh / progress / cancel check / heartbeat, default 15s
	Poll     time.Duration // queue poll when idle, default 2s
	LogFlush time.Duration // log chunk flush, default 2s
	LogChunk int           // log chunk size, default 64 KiB

	Logf func(format string, args ...any) // daemon log; default stderr
}

type Runner struct{ c Config }

func New(c Config) *Runner {
	if c.TTL == 0 {
		c.TTL = 90 * time.Second
	}
	if c.Refresh == 0 {
		c.Refresh = 15 * time.Second
	}
	if c.Poll == 0 {
		c.Poll = 2 * time.Second
	}
	if c.LogFlush == 0 {
		c.LogFlush = 2 * time.Second
	}
	if c.Clock == nil {
		c.Clock = tier2.SystemClock{}
	}
	if c.Version == "" {
		c.Version = Version
	}
	if c.Name == "" {
		h, _ := os.Hostname()
		c.Name = fmt.Sprintf("%s-%d", h, os.Getpid())
	}
	if c.Logf == nil {
		c.Logf = func(f string, a ...any) { fmt.Fprintf(os.Stderr, "runner: "+f+"\n", a...) }
	}
	return &Runner{c}
}

// Run polls until ctx is done (or, with Once, until one job has been handled).
func (r *Runner) Run(ctx context.Context) error {
	hbDone := make(chan struct{})
	hbCtx, stopHB := context.WithCancel(ctx)
	go func() { defer close(hbDone); r.heartbeat(hbCtx) }()
	defer func() {
		stopHB()
		<-hbDone
		_ = r.c.Store.Delete(tier2.RunnerPath(r.c.Name), "")
	}()
	for {
		ran, err := r.PollOnce(ctx)
		if err != nil {
			r.c.Logf("poll: %v", err)
		}
		if ran && r.c.Once {
			return nil
		}
		if ran {
			continue
		}
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(r.c.Poll):
		}
	}
}

func (r *Runner) heartbeat(ctx context.Context) {
	for {
		info := tier2.RunnerInfo{Labels: r.c.Labels, Version: r.c.Version, LastSeen: r.c.Clock.Now()}
		b, _ := json.Marshal(info)
		if _, err := r.c.Store.Put(tier2.RunnerPath(r.c.Name), b); err != nil {
			r.c.Logf("heartbeat: %v", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(r.c.Refresh):
		}
	}
}

// PollOnce looks through the queues for one claimable entry and runs it. It reports whether a
// job was claimed (and handled).
func (r *Runner) PollOnce(ctx context.Context) (bool, error) {
	for _, label := range r.c.Labels {
		objs, err := r.c.Store.List(tier2.QueuePrefix(label))
		if err != nil {
			return false, err
		}
		for _, o := range objs {
			if ctx.Err() != nil {
				return false, nil
			}
			body, _, found, err := r.c.Store.Get(o.Key)
			if err != nil {
				return false, err
			}
			if !found {
				continue
			}
			var e tier2.QueueEntry
			if err := json.Unmarshal(body, &e); err != nil {
				r.c.Logf("skip %s: %v", o.Key, err)
				continue
			}
			h, err := r.Claim(e)
			if err != nil {
				return false, err
			}
			if h == nil {
				continue
			}
			r.c.Logf("claimed %s seq %d", o.Key, h.lease.Seq)
			r.runJob(ctx, label, e, h)
			return true, nil
		}
	}
	return false, nil
}

// held is a lease this runner owns: the record and the version it was last written at.
type held struct {
	lease   tier2.Lease
	version string
}

// Claim tries to take the job's lease (section 5.2): create if absent, or takeover by version
// match with seq+1 if expired. It returns nil if someone else holds it, or the job already
// has a result under the current seq (the queue entry outlived the job).
func (r *Runner) Claim(e tier2.QueueEntry) (*held, error) {
	s, now := r.c.Store, r.c.Clock.Now()
	key := tier2.LeasePath(e.Run, e.Attempt, e.Job)
	body, ver, found, err := s.Get(key)
	if err != nil {
		return nil, err
	}
	next := tier2.Lease{Holder: r.c.Name, ExpiresAt: now.Add(r.c.TTL), Seq: 1}
	var nv string
	var ok bool
	if !found {
		b, _ := json.Marshal(next)
		nv, ok, err = s.Create(key, b)
	} else {
		var old tier2.Lease
		if err := json.Unmarshal(body, &old); err != nil {
			return nil, fmt.Errorf("%s: %w", key, err)
		}
		if !old.Expired(now) {
			return nil, nil
		}
		_, _, done, err := s.Get(tier2.ResultPath(e.Run, e.Attempt, e.Job, old.Seq))
		if err != nil {
			return nil, err
		}
		if done {
			return nil, nil
		}
		next.Seq = old.Seq + 1
		b, _ := json.Marshal(next)
		nv, ok, err = s.PutIfVersion(key, b, ver)
	}
	if err != nil || !ok {
		return nil, err
	}
	return &held{lease: next, version: nv}, nil
}

// runJob executes a claimed job and reports it. All writes carry the lease seq in their
// path, so a runner that lost the lease cannot overwrite its successor.
func (r *Runner) runJob(ctx context.Context, label string, e tier2.QueueEntry, h *held) {
	s, seq := r.c.Store, h.lease.Seq
	var lost, cancelled atomic.Bool
	jctx, cancelJob := context.WithCancel(ctx)
	defer cancelJob()
	if e.TimeoutMinutes > 0 {
		var c context.CancelFunc
		jctx, c = context.WithTimeout(jctx, time.Duration(e.TimeoutMinutes)*time.Minute)
		defer c()
	}

	chunks := NewChunker(r.c.LogChunk, func(n int, data []byte) {
		if lost.Load() {
			return
		}
		if _, _, err := s.Create(tier2.LogChunkPath(e.Run, e.Attempt, e.Job, seq, n), data); err != nil {
			r.c.Logf("log chunk %d: %v", n, err)
		}
	}, nil)
	steps := NewTracker(e)
	fmt.Fprintf(chunks, "runner %s: claimed %d.%d.%s seq %d\n", r.c.Name, e.Run, e.Attempt, e.Job, seq)
	if len(e.Secrets) > 0 {
		fmt.Fprintf(chunks, "runner: ignoring %d secret reference(s): secrets are not supported yet\n", len(e.Secrets))
	}

	writeProgress := func() {
		if lost.Load() || !steps.Started() {
			return
		}
		if st, dirty := steps.Snapshot(true); dirty {
			b, _ := json.Marshal(tier2.Progress{Holder: r.c.Name, Seq: seq, Steps: st, UpdatedAt: r.c.Clock.Now()})
			if _, err := s.Put(tier2.ProgressPath(e.Run, e.Attempt, e.Job, seq), b); err != nil {
				r.c.Logf("progress: %v", err)
			}
		}
	}
	checkCancel := func() {
		_, _, found, err := s.Get(tier2.CancelPath(e.Run, e.Attempt, e.Job))
		if err == nil && found && !cancelled.Swap(true) {
			fmt.Fprintf(chunks, "runner: cancel marker seen, stopping\n")
			cancelJob()
		}
	}
	refresh := func() {
		now := r.c.Clock.Now()
		l := h.lease
		l.ExpiresAt = now.Add(r.c.TTL)
		b, _ := json.Marshal(l)
		nv, ok, err := s.PutIfVersion(tier2.LeasePath(e.Run, e.Attempt, e.Job), b, h.version)
		switch {
		case err != nil:
			// Transient: keep going until our own expiry passes, then we cannot claim it.
			r.c.Logf("lease refresh: %v", err)
			if !h.lease.ExpiresAt.After(now) {
				ok = false
			} else {
				return
			}
		case ok:
			h.lease, h.version = l, nv
			return
		}
		if !ok && !lost.Swap(true) {
			r.c.Logf("lost lease %d.%d.%s seq %d", e.Run, e.Attempt, e.Job, seq)
			cancelJob()
		}
	}

	stop, stopped := make(chan struct{}), make(chan struct{})
	go func() {
		defer close(stopped)
		rt, ft := time.NewTicker(r.c.Refresh), time.NewTicker(r.c.LogFlush)
		defer rt.Stop()
		defer ft.Stop()
		for {
			select {
			case <-stop:
				return
			case <-rt.C:
				refresh()
				if !lost.Load() {
					checkCancel()
					writeProgress()
				}
			case <-ft.C:
				chunks.Flush()
			}
		}
	}()

	checkCancel()
	var conclusion string
	var execErr error
	if !cancelled.Load() {
		conclusion, execErr = r.c.Exec.Execute(jctx, &Job{Entry: e, Label: label, Seq: seq, Log: chunks, Steps: steps})
	}
	timedOut := jctx.Err() == context.DeadlineExceeded && !cancelled.Load() && !lost.Load()
	close(stop)
	<-stopped

	if lost.Load() {
		r.c.Logf("%d.%d.%s seq %d: lost the lease, writing nothing more", e.Run, e.Attempt, e.Job, seq)
		return
	}
	switch {
	case cancelled.Load():
		conclusion = tier2.Cancelled
	case timedOut:
		fmt.Fprintf(chunks, "runner: job exceeded timeout of %d minutes\n", e.TimeoutMinutes)
		conclusion = tier2.Failure
	case execErr != nil:
		fmt.Fprintf(chunks, "runner: job failed: %v\n", execErr)
		conclusion = tier2.Failure
	case conclusion == "":
		conclusion = tier2.Failure
	}
	chunks.Flush()
	fullKey := tier2.FullLogPath(e.Run, e.Attempt, e.Job, seq)
	if _, _, err := s.Create(fullKey, chunks.Full()); err != nil {
		r.c.Logf("full log: %v", err)
	}
	writeProgress()
	running, rest := tier2.Failure, "skipped"
	if conclusion == tier2.Cancelled {
		running, rest = tier2.Cancelled, tier2.Cancelled
	}
	if conclusion == tier2.Success {
		running = tier2.Success
	}
	res := tier2.Result{
		Runner: r.c.Name, Seq: seq, Conclusion: conclusion, Outputs: map[string]string{},
		Steps: steps.Final(running, rest), Log: fullKey, FinishedAt: r.c.Clock.Now(),
	}
	b, _ := json.Marshal(res)
	if _, ok, err := s.Create(tier2.ResultPath(e.Run, e.Attempt, e.Job, seq), b); err != nil {
		r.c.Logf("result: %v", err)
	} else if !ok {
		r.c.Logf("result for seq %d already exists", seq)
	}
	r.c.Logf("%d.%d.%s seq %d: %s", e.Run, e.Attempt, e.Job, seq, conclusion)
}
