package tier2_test

import (
	"os"
	"testing"

	"github.com/ArchAstro/pail/actions/internal/tier2"
	"github.com/ArchAstro/pail/actions/internal/tier2/fakepail"
)

func TestMemStore(t *testing.T) { conformance(t, tier2.NewMemStore()) }

// TestPailStoreContract drives PailStore against a fake `pail obj` that follows the fixed
// contract (exit 3 absent, exit 4 precondition, version token on stdout).
func TestPailStoreContract(t *testing.T) {
	dir := t.TempDir()
	bin, err := fakepail.Write(dir)
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("PAIL_FAKE_REAL", "/bin/false")
	conformance(t, &tier2.PailStore{Pail: bin, URL: "file://" + dir + "/store", Env: os.Environ()})
}

func conformance(t *testing.T, s tier2.Store) {
	if _, v, found, err := s.Get("leases/1/1/a"); found || err != nil || v != "" {
		t.Fatalf("absent: %v %v", found, err)
	}
	v1, ok, err := s.Create("leases/1/1/a", []byte("one"))
	if !ok || err != nil || v1 == "" {
		t.Fatal(v1, ok, err)
	}
	if _, ok, err := s.Create("leases/1/1/a", []byte("dup")); ok || err != nil {
		t.Fatalf("create over existing: %v %v", ok, err)
	}
	body, v, found, _ := s.Get("leases/1/1/a")
	if !found || string(body) != "one" || v != v1 {
		t.Fatalf("%q %q %v", body, v, found)
	}
	if _, ok, err := s.PutIfVersion("leases/1/1/a", []byte("x"), "stale"); ok || err != nil {
		t.Fatalf("stale put: %v %v", ok, err)
	}
	if _, ok, err := s.PutIfVersion("leases/1/1/missing", []byte("x"), v1); ok || err != nil {
		t.Fatalf("put to absent: %v %v", ok, err)
	}
	v2, ok, err := s.PutIfVersion("leases/1/1/a", []byte("two"), v1)
	if !ok || err != nil || v2 == v1 {
		t.Fatal(v2, ok, err)
	}
	if _, err := s.Put("leases/1/1/b", []byte("b")); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Put("queue/x/1.1.j.json", []byte("q")); err != nil {
		t.Fatal(err)
	}
	objs, err := s.List("leases/1/")
	if err != nil || len(objs) != 2 || objs[0].Key != "leases/1/1/a" || objs[0].Size != 3 || objs[0].Version != v2 || objs[1].Key != "leases/1/1/b" {
		t.Fatalf("%+v %v", objs, err)
	}
	if err := s.Delete("leases/1/1/a", "stale"); err != tier2.ErrPrecondition {
		t.Fatalf("delete with stale version: %v", err)
	}
	if err := s.Delete("leases/1/1/a", v2); err != nil {
		t.Fatal(err)
	}
	if err := s.Delete("leases/1/1/a", ""); err != nil { // absent is fine
		t.Fatal(err)
	}
	if _, _, found, _ := s.Get("leases/1/1/a"); found {
		t.Fatal("still there")
	}
	// create after delete works (a fresh object)
	if _, ok, _ := s.Create("leases/1/1/a", []byte("again")); !ok {
		t.Fatal("recreate failed")
	}
}

func TestQueueKeys(t *testing.T) {
	k := tier2.QueuePath("ubuntu-latest", 4182, 2, "build.linux")
	label, run, att, job, ok := tier2.ParseQueueKey(k)
	if !ok || label != "ubuntu-latest" || run != 4182 || att != 2 || job != "build.linux" {
		t.Fatal(k, label, run, att, job, ok)
	}
	if _, _, _, _, ok := tier2.ParseQueueKey("queue/x/notakey.json"); ok {
		t.Fatal("parsed garbage")
	}
	if got := tier2.ResultPath(4182, 1, "test", 2); got != "results/4182/1/test/2" {
		t.Fatal(got)
	}
	if got := tier2.ProgressPath(4182, 1, "test", 2); got != "progress/4182/1/test/2.json" {
		t.Fatal(got)
	}
	if got := tier2.LogChunkPath(4182, 1, "test", 2, 3); got != "logs/4182/1/test/2/3.log" {
		t.Fatal(got)
	}
}
