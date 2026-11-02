package tier2

import (
	"errors"
	"sync"
	"time"
)

// Obj is one listed object.
type Obj struct {
	Key     string
	Size    int64
	Version string
}

// ErrPrecondition is returned by Delete when ifVersion does not match.
var ErrPrecondition = errors.New("tier2: precondition failed")

// Store is the Tier 2 area of a bucket. Versions are opaque tokens (the object generation);
// compare-and-set results are reported as ok=false, not as errors, so callers can tell "lost
// the race" from "the store is down".
type Store interface {
	// List returns objects whose key starts with prefix, sorted by key.
	List(prefix string) ([]Obj, error)
	// Get returns the body and version; found=false if the object is absent.
	Get(key string) (body []byte, version string, found bool, err error)
	// Create writes only if the key is absent (generation 0).
	Create(key string, body []byte) (version string, ok bool, err error)
	// PutIfVersion overwrites only the version read (generation match).
	PutIfVersion(key string, body []byte, version string) (newVersion string, ok bool, err error)
	// Put overwrites unconditionally (safe only for keys with a single writer).
	Put(key string, body []byte) (version string, err error)
	// Delete removes the key; ifVersion "" is unconditional. Absent is not an error.
	// A version mismatch returns ErrPrecondition.
	Delete(key string, ifVersion string) error
}

// Clock is the time source of the runner and tests.
type Clock interface{ Now() time.Time }

// SystemClock is the wall clock.
type SystemClock struct{}

func (SystemClock) Now() time.Time { return time.Now().UTC() }

// FakeClock is a settable clock for tests.
type FakeClock struct {
	mu sync.Mutex
	t  time.Time
}

func NewFakeClock(t time.Time) *FakeClock { return &FakeClock{t: t} }

func (c *FakeClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.t
}

func (c *FakeClock) Advance(d time.Duration) {
	c.mu.Lock()
	c.t = c.t.Add(d)
	c.mu.Unlock()
}
