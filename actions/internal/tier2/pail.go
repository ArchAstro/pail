package tier2

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
)

// Exit codes of `pail obj` (fixed contract).
const (
	exitAbsent       = 3
	exitPrecondition = 4
)

// PailStore implements Store by shelling out to `pail --store URL obj --area actions ...`.
// One process per call: simple, and pail already owns credentials and retries.
type PailStore struct {
	Pail string   // pail binary; default "pail"
	URL  string   // the repo store url (gs://B/repo), not the .actions area
	Area string   // default "actions"
	Env  []string // extra environment, e.g. credentials; default: os.Environ()
}

func (p *PailStore) bin() string {
	if p.Pail == "" {
		return "pail"
	}
	return p.Pail
}

func (p *PailStore) area() string {
	if p.Area == "" {
		return "actions"
	}
	return p.Area
}

// run executes `pail --store URL obj --area A <args...>`; it returns stdout and the exit code
// (0 on success). A failure to start, or an exit code that is not in ok, is an error.
func (p *PailStore) run(stdin []byte, args ...string) (stdout []byte, code int, err error) {
	full := append([]string{"--store", p.URL, "obj", "--area", p.area()}, args...)
	cmd := exec.Command(p.bin(), full...)
	if p.Env != nil {
		cmd.Env = p.Env
	}
	if stdin != nil {
		cmd.Stdin = bytes.NewReader(stdin)
	}
	var out, errb bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errb
	err = cmd.Run()
	var ee *exec.ExitError
	switch {
	case err == nil:
		return out.Bytes(), 0, nil
	case errors.As(err, &ee):
		return out.Bytes(), ee.ExitCode(), fmt.Errorf("pail obj %s: exit %d: %s", args[0], ee.ExitCode(), strings.TrimSpace(errb.String()))
	default:
		return nil, -1, fmt.Errorf("pail obj %s: %w", args[0], err)
	}
}

func (p *PailStore) List(prefix string) ([]Obj, error) {
	out, _, err := p.run(nil, "ls", prefix)
	if err != nil {
		return nil, err
	}
	var objs []Obj
	for _, line := range strings.Split(string(out), "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		var o struct {
			Key     string `json:"key"`
			Size    int64  `json:"size"`
			Version string `json:"version"`
		}
		if err := json.Unmarshal([]byte(line), &o); err != nil {
			return nil, fmt.Errorf("pail obj ls: bad line %q: %w", line, err)
		}
		objs = append(objs, Obj{Key: o.Key, Size: o.Size, Version: o.Version})
	}
	return objs, nil
}

func (p *PailStore) Get(key string) ([]byte, string, bool, error) {
	f, err := os.CreateTemp("", "pail-obj-*")
	if err != nil {
		return nil, "", false, err
	}
	name := f.Name()
	f.Close()
	defer os.Remove(name)
	out, code, err := p.run(nil, "get", key, "-o", name)
	if code == exitAbsent {
		return nil, "", false, nil
	}
	if err != nil {
		return nil, "", false, err
	}
	body, err := os.ReadFile(name)
	if err != nil {
		return nil, "", false, err
	}
	return body, strings.TrimSpace(string(out)), true, nil
}

func (p *PailStore) put(key string, body []byte, cond ...string) (string, bool, error) {
	if body == nil {
		body = []byte{}
	}
	out, code, err := p.run(body, append([]string{"put", key, "-"}, cond...)...)
	if code == exitPrecondition {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return strings.TrimSpace(string(out)), true, nil
}

func (p *PailStore) Create(key string, body []byte) (string, bool, error) {
	return p.put(key, body, "--if-absent")
}

func (p *PailStore) PutIfVersion(key string, body []byte, version string) (string, bool, error) {
	return p.put(key, body, "--if-version", version)
}

func (p *PailStore) Put(key string, body []byte) (string, error) {
	v, _, err := p.put(key, body)
	return v, err
}

func (p *PailStore) Delete(key, ifVersion string) error {
	args := []string{"rm", key}
	if ifVersion != "" {
		args = append(args, "--if-version", ifVersion)
	}
	_, code, err := p.run(nil, args...)
	if code == exitPrecondition {
		return ErrPrecondition
	}
	return err
}

// HasObj reports whether the pail binary supports `obj` (the integration tests gate on it).
func (p *PailStore) HasObj() bool {
	out, err := exec.Command(p.bin(), "obj", "--help").CombinedOutput()
	return err == nil && strings.Contains(string(out), "ls")
}
