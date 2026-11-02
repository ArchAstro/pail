package runner

import (
	"bytes"
	"sync"
)

// Chunker is the log sink of one job run. It buffers writes and hands them to flush as
// numbered chunks when the buffer reaches max bytes, or when Flush is called (the runner
// calls it every couple of seconds). It also keeps the whole log for full.log.
type Chunker struct {
	mu    sync.Mutex
	buf   bytes.Buffer
	full  bytes.Buffer
	n     int
	max   int
	flush func(n int, data []byte)
	mask  func([]byte) []byte
}

// NewChunker returns a Chunker; flush is called with n = 1, 2, ... and must not retain data.
// mask is the secret-masking hook (TODO: secrets are out of scope today); nil is identity.
func NewChunker(max int, flush func(n int, data []byte), mask func([]byte) []byte) *Chunker {
	if max <= 0 {
		max = 64 << 10
	}
	return &Chunker{max: max, flush: flush, mask: mask}
}

func (c *Chunker) Write(p []byte) (int, error) {
	n := len(p)
	if c.mask != nil {
		p = c.mask(p)
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.buf.Write(p)
	c.full.Write(p)
	for c.buf.Len() >= c.max {
		c.emit(c.buf.Next(c.max))
	}
	return n, nil
}

// Flush emits whatever is buffered as one chunk.
func (c *Chunker) Flush() {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.buf.Len() > 0 {
		c.emit(c.buf.Next(c.buf.Len()))
	}
}

func (c *Chunker) emit(data []byte) {
	c.n++
	c.flush(c.n, data)
}

// Full returns everything written so far.
func (c *Chunker) Full() []byte {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]byte(nil), c.full.Bytes()...)
}

// Chunks is the number of chunks emitted.
func (c *Chunker) Chunks() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.n
}
