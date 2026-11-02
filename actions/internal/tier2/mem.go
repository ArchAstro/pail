package tier2

import (
	"fmt"
	"sort"
	"strings"
	"sync"
)

// MemStore is an in-memory Store with the semantics of the real one: create-only,
// generation-match, absent-delete is fine. Versions are "g<N>" from one counter.
type MemStore struct {
	mu   sync.Mutex
	objs map[string]memObj
	gen  int

	// Fault, if set, is called before every operation ("list", "get", "create", "putv",
	// "put", "delete") with the key; a non-nil error is returned instead of acting.
	Fault func(op, key string) error
}

type memObj struct {
	body    []byte
	version string
}

func NewMemStore() *MemStore { return &MemStore{objs: map[string]memObj{}} }

func (m *MemStore) fault(op, key string) error {
	if m.Fault != nil {
		return m.Fault(op, key)
	}
	return nil
}

func (m *MemStore) next() string { m.gen++; return fmt.Sprintf("g%d", m.gen) }

func (m *MemStore) List(prefix string) ([]Obj, error) {
	if err := m.fault("list", prefix); err != nil {
		return nil, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	var out []Obj
	for k, o := range m.objs {
		if strings.HasPrefix(k, prefix) {
			out = append(out, Obj{Key: k, Size: int64(len(o.body)), Version: o.version})
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Key < out[j].Key })
	return out, nil
}

func (m *MemStore) Get(key string) ([]byte, string, bool, error) {
	if err := m.fault("get", key); err != nil {
		return nil, "", false, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	o, ok := m.objs[key]
	if !ok {
		return nil, "", false, nil
	}
	return append([]byte(nil), o.body...), o.version, true, nil
}

func (m *MemStore) Create(key string, body []byte) (string, bool, error) {
	if err := m.fault("create", key); err != nil {
		return "", false, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.objs[key]; ok {
		return "", false, nil
	}
	v := m.next()
	m.objs[key] = memObj{append([]byte(nil), body...), v}
	return v, true, nil
}

func (m *MemStore) PutIfVersion(key string, body []byte, version string) (string, bool, error) {
	if err := m.fault("putv", key); err != nil {
		return "", false, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	o, ok := m.objs[key]
	if !ok || o.version != version {
		return "", false, nil
	}
	v := m.next()
	m.objs[key] = memObj{append([]byte(nil), body...), v}
	return v, true, nil
}

func (m *MemStore) Put(key string, body []byte) (string, error) {
	if err := m.fault("put", key); err != nil {
		return "", err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	v := m.next()
	m.objs[key] = memObj{append([]byte(nil), body...), v}
	return v, nil
}

func (m *MemStore) Delete(key, ifVersion string) error {
	if err := m.fault("delete", key); err != nil {
		return err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	o, ok := m.objs[key]
	if !ok {
		return nil
	}
	if ifVersion != "" && o.version != ifVersion {
		return ErrPrecondition
	}
	delete(m.objs, key)
	return nil
}
