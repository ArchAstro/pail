package plan

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
)

// Path plans a workflow file, or every *.yml/*.yaml file of a directory, sorted by name.
func Path(path string, in Input) ([]*Plan, error) {
	st, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	files := []string{path}
	if st.IsDir() {
		files = nil
		for _, ext := range []string{"*.yml", "*.yaml"} {
			m, _ := filepath.Glob(filepath.Join(path, ext))
			files = append(files, m...)
		}
		sort.Strings(files)
	}
	var out []*Plan
	for _, f := range files {
		p, err := Workflow(f, in)
		if err != nil {
			return nil, err
		}
		out = append(out, p)
	}
	return out, nil
}

// JSON renders v as indented JSON without HTML escaping (expressions contain & and <).
func JSON(v any) ([]byte, error) {
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimRight(b.Bytes(), "\n"), nil
}
