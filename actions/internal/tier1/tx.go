package tier1

import (
	"sort"
	"strings"
)

// File is one blob of a snapshot. Data is nil for files that were listed but not loaded
// (claims, numbers, approvals: only their names matter).
type File struct {
	ID   string
	Data []byte
}

// Snapshot is a branch tip: every path with its blob id, and the content of the record files.
type Snapshot struct {
	Branch string
	Commit string // "" when the branch does not exist yet
	Files  map[string]File
}

// Has reports whether a path exists.
func (s *Snapshot) Has(path string) bool { _, ok := s.Files[path]; return ok }

// Under lists the paths with the prefix, sorted.
func (s *Snapshot) Under(prefix string) []string {
	var out []string
	for p := range s.Files {
		if strings.HasPrefix(p, prefix) {
			out = append(out, p)
		}
	}
	sort.Strings(out)
	return out
}

// Tx is the guarded transaction of docs/actions.md 7.2: it reads records from a snapshot,
// overlays its own writes, and remembers what it read so the commit can guard every record
// read, not only those written.
type Tx struct {
	snap   *Snapshot
	writes map[string][]byte
	reads  map[string]bool
}

func NewTx(s *Snapshot) *Tx {
	return &Tx{snap: s, writes: map[string][]byte{}, reads: map[string]bool{}}
}

// Read returns a record's content as this transaction sees it (its own write, else the
// snapshot) and guards it: the commit lands only if the record is still as read.
func (t *Tx) Read(path string) ([]byte, bool) {
	t.reads[path] = true
	return t.Get(path)
}

// Get is Read without the guard, for scanning records the decision does not depend on.
func (t *Tx) Get(path string) ([]byte, bool) {
	if d, ok := t.writes[path]; ok {
		return d, true
	}
	if f, ok := t.snap.Files[path]; ok && f.Data != nil {
		return f.Data, true
	}
	return nil, false
}

// Exists is true for a path in the snapshot or written by this transaction. It does not
// guard the path (a path that is created is guarded by --if-absent anyway).
func (t *Tx) Exists(path string) bool {
	if _, ok := t.writes[path]; ok {
		return true
	}
	return t.snap.Has(path)
}

// Write sets a record.
func (t *Tx) Write(path string, data []byte) { t.writes[path] = data }

// Empty is true when nothing is written.
func (t *Tx) Empty() bool { return len(t.writes) == 0 }

// Written lists the written paths, sorted.
func (t *Tx) Written() []string {
	var out []string
	for p := range t.writes {
		out = append(out, p)
	}
	sort.Strings(out)
	return out
}

// Write is one file of a `pail put`.
type PutFile struct {
	Path   string
	Data   []byte
	Expect string // blob id, or "absent"
}

// Puts turns the transaction into the files of one `pail put`, each with its guard:
//
//	written, existed          --expect PATH=<blob id read>
//	written, did not exist    --expect PATH=absent (--if-absent)
//	read, not written         --expect PATH=<blob id read>, on a rewrite of the same content
//
// `pail put --expect` only accepts paths it also writes, so the third is an identical
// rewrite: the commit lands only if the record is still as read.
func (t *Tx) Puts() []PutFile {
	paths := map[string]bool{}
	for p := range t.writes {
		paths[p] = true
	}
	for p := range t.reads {
		if f, ok := t.snap.Files[p]; ok && f.Data != nil {
			paths[p] = true
		}
	}
	var out []PutFile
	for p := range paths {
		f, existed := t.snap.Files[p]
		pf := PutFile{Path: p, Expect: "absent"}
		if existed {
			pf.Expect = f.ID
		}
		if d, ok := t.writes[p]; ok {
			pf.Data = d
		} else {
			pf.Data = f.Data
		}
		out = append(out, pf)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}
