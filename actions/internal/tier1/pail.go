package tier1

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
)

// ErrConflict is `pail put` exiting 4: a guarded record changed since it was read.
var ErrConflict = errors.New("tier1: conflict (a guarded record changed)")

// Pail runs the pail CLI against one store. It is the only thing in the orchestrator that
// talks to the store.
type Pail struct {
	Bin   string   // default "pail"
	Store string   // store URL
	Env   []string // extra environment, e.g. PAIL_AUTHOR=...

	// BeforePut, if set, runs just before each Put (tests use it to land a competing write
	// between a tick's read and its commit).
	BeforePut func()

	mu    sync.Mutex
	cache map[string][]byte // blob id -> content (ids are content hashes, so it is always valid)
}

func (p *Pail) bin() string {
	if p.Bin == "" {
		return "pail"
	}
	return p.Bin
}

func (p *Pail) run(ctx context.Context, stdin []byte, args ...string) ([]byte, int, error) {
	cmd := exec.CommandContext(ctx, p.bin(), append([]string{"--store", p.Store}, args...)...)
	cmd.Env = append(os.Environ(), p.Env...)
	if stdin != nil {
		cmd.Stdin = bytes.NewReader(stdin)
	}
	var out, errb bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errb
	err := cmd.Run()
	if err != nil {
		code := -1
		var ee *exec.ExitError
		if errors.As(err, &ee) {
			code = ee.ExitCode()
		}
		return out.Bytes(), code, fmt.Errorf("pail %s: %w: %s", strings.Join(args[:min(len(args), 2)], " "), err, strings.TrimSpace(errb.String()))
	}
	return out.Bytes(), 0, nil
}

// Tip resolves a branch to its commit id; ok is false if the branch does not exist.
func (p *Pail) Tip(ctx context.Context, branch string) (commit string, ok bool, err error) {
	out, code, err := p.run(ctx, nil, "rev-parse", "--verify", "--quiet", "refs/heads/"+branch)
	if err != nil {
		if code == 1 || code == 128 {
			return "", false, nil
		}
		return "", false, err
	}
	return strings.TrimSpace(string(out)), true, nil
}

// Tree lists every file of a commit as path -> blob id.
func (p *Pail) Tree(ctx context.Context, rev string) (map[string]string, error) {
	out, _, err := p.run(ctx, nil, "ls-tree", "-r", rev)
	if err != nil {
		return nil, err
	}
	files := map[string]string{}
	for _, line := range strings.Split(string(out), "\n") {
		tab := strings.IndexByte(line, '\t')
		if tab < 0 {
			continue
		}
		f := strings.Fields(line[:tab])
		path := line[tab+1:]
		if len(f) != 3 || f[1] != "blob" || strings.HasPrefix(path, `"`) {
			continue
		}
		files[path] = f[2]
	}
	return files, nil
}

// Blobs reads the content of files of a commit with one `cat-file --batch`, as path -> content,
// and caches it by blob id. Missing paths are left out.
func (p *Pail) Blobs(ctx context.Context, commit string, want map[string]string) (map[string][]byte, error) {
	out := map[string][]byte{}
	var need []string
	p.mu.Lock()
	if p.cache == nil || len(p.cache) > 100000 {
		p.cache = map[string][]byte{}
	}
	for path, id := range want {
		if d, ok := p.cache[id]; ok {
			out[path] = d
		} else {
			need = append(need, path)
		}
	}
	p.mu.Unlock()
	if len(need) == 0 {
		return out, nil
	}
	var in bytes.Buffer
	for _, path := range need {
		fmt.Fprintf(&in, "%s:%s\n", commit, path)
	}
	raw, _, err := p.run(ctx, in.Bytes(), "cat-file", "--batch")
	if err != nil {
		return nil, err
	}
	// Each answer: "<id> blob <size>\n<bytes>\n" or "<spec> missing\n".
	r := raw
	for _, path := range need {
		nl := bytes.IndexByte(r, '\n')
		if nl < 0 {
			return nil, fmt.Errorf("cat-file --batch: short output at %s", path)
		}
		head := strings.Fields(string(r[:nl]))
		r = r[nl+1:]
		if len(head) == 2 && head[1] == "missing" {
			continue
		}
		if len(head) != 3 {
			return nil, fmt.Errorf("cat-file --batch: bad header %q", string(r[:nl]))
		}
		size, err := strconv.Atoi(head[2])
		if err != nil || size > len(r) {
			return nil, fmt.Errorf("cat-file --batch: bad size in %v", head)
		}
		data := append([]byte(nil), r[:size]...)
		r = r[min(size+1, len(r)):]
		out[path] = data
		p.mu.Lock()
		p.cache[want[path]] = data
		p.mu.Unlock()
	}
	return out, nil
}

// Snapshot reads a branch tip: the listing of all files, and the content of those `load`
// accepts. A missing branch is an empty snapshot.
func (p *Pail) Snapshot(ctx context.Context, branch string, load func(path string) bool) (*Snapshot, error) {
	s := &Snapshot{Branch: branch, Files: map[string]File{}}
	commit, ok, err := p.Tip(ctx, branch)
	if err != nil || !ok {
		return s, err
	}
	s.Commit = commit
	ids, err := p.Tree(ctx, commit)
	if err != nil {
		return nil, err
	}
	want := map[string]string{}
	for path, id := range ids {
		s.Files[path] = File{ID: id}
		if load(path) {
			want[path] = id
		}
	}
	data, err := p.Blobs(ctx, commit, want)
	if err != nil {
		return nil, err
	}
	for path, d := range data {
		f := s.Files[path]
		f.Data = d
		s.Files[path] = f
	}
	return s, nil
}

// Show reads one file at a revision; ok is false if it does not exist.
func (p *Pail) Show(ctx context.Context, rev, path string) ([]byte, bool, error) {
	out, code, err := p.run(ctx, nil, "cat", rev+":"+path)
	if err != nil {
		if code == 1 || code == 128 {
			return nil, false, nil
		}
		return nil, false, err
	}
	return out, true, nil
}

// Put lands one commit with its guards. ErrConflict means a guarded record changed.
func (p *Pail) Put(ctx context.Context, branch, message string, files []PutFile) error {
	if len(files) == 0 {
		return nil
	}
	if p.BeforePut != nil {
		p.BeforePut()
	}
	dir, err := os.MkdirTemp("", "pail-actions-put-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(dir)
	args := []string{"put", branch, "-m", message}
	for i, f := range files {
		src := filepath.Join(dir, strconv.Itoa(i))
		if err := os.WriteFile(src, f.Data, 0o600); err != nil {
			return err
		}
		args = append(args, "--file", f.Path+"="+src, "--expect", f.Path+"="+f.Expect)
	}
	_, code, err := p.run(ctx, nil, args...)
	if err != nil {
		if code == 4 {
			return fmt.Errorf("%w: %v", ErrConflict, err)
		}
		return err
	}
	return nil
}

// Changed lists the paths that differ between two commits (`pail diff` needs a working copy,
// so the two trees are compared). An empty from means every path of to.
func (p *Pail) Changed(ctx context.Context, from, to string) ([]string, error) {
	var old map[string]string
	if from != "" {
		var err error
		if old, err = p.Tree(ctx, from); err != nil {
			return nil, err
		}
	}
	cur, err := p.Tree(ctx, to)
	if err != nil {
		return nil, err
	}
	var paths []string
	for path, id := range cur {
		if old[path] != id {
			paths = append(paths, path)
		}
	}
	for path := range old {
		if _, ok := cur[path]; !ok {
			paths = append(paths, path)
		}
	}
	sort.Strings(paths)
	return paths, nil
}

// Author is the author name of a commit.
func (p *Pail) Author(ctx context.Context, commit string) (string, error) {
	out, _, err := p.run(ctx, nil, "log", "-1", "--format=%an", commit)
	return strings.TrimSpace(string(out)), err
}
