package tier2

import (
	"fmt"
	"strconv"
	"strings"
)

// QueuePrefix is the prefix a runner lists for one label.
func QueuePrefix(label string) string { return "queue/" + label + "/" }

// QueuePath is queue/<label>/<run>.<att>.<job>.json.
func QueuePath(label string, run int64, att int, job string) string {
	return fmt.Sprintf("queue/%s/%d.%d.%s.json", label, run, att, job)
}

// ParseQueueKey splits a queue key into its parts. Run and attempt are numbers, so the first
// two dots delimit them and the job id may contain anything.
func ParseQueueKey(key string) (label string, run int64, att int, job string, ok bool) {
	rest, found := strings.CutPrefix(key, "queue/")
	if !found {
		return
	}
	i := strings.IndexByte(rest, '/')
	if i < 0 {
		return
	}
	label, rest = rest[:i], rest[i+1:]
	rest, found = strings.CutSuffix(rest, ".json")
	if !found {
		return
	}
	parts := strings.SplitN(rest, ".", 3)
	if len(parts) != 3 {
		return
	}
	r, err1 := strconv.ParseInt(parts[0], 10, 64)
	a, err2 := strconv.Atoi(parts[1])
	if err1 != nil || err2 != nil || parts[2] == "" {
		return
	}
	return label, r, a, parts[2], true
}

func jobKey(run int64, att int, job string) string { return fmt.Sprintf("%d/%d/%s", run, att, job) }

// LeasePath is leases/<run>/<att>/<job>.
func LeasePath(run int64, att int, job string) string { return "leases/" + jobKey(run, att, job) }

// CancelPath is cancel/<run>/<att>/<job>.
func CancelPath(run int64, att int, job string) string { return "cancel/" + jobKey(run, att, job) }

// ProgressPath is progress/<run>/<att>/<job>/<seq>.json.
func ProgressPath(run int64, att int, job string, seq int) string {
	return fmt.Sprintf("progress/%s/%d.json", jobKey(run, att, job), seq)
}

// ProgressPrefix lists every seq's progress file of a job.
func ProgressPrefix(run int64, att int, job string) string {
	return "progress/" + jobKey(run, att, job) + "/"
}

// LogPrefix is logs/<run>/<att>/<job>/<seq>/.
func LogPrefix(run int64, att int, job string, seq int) string {
	return fmt.Sprintf("logs/%s/%d/", jobKey(run, att, job), seq)
}

// LogChunkPath is logs/<run>/<att>/<job>/<seq>/<n>.log (n counts from 1).
func LogChunkPath(run int64, att int, job string, seq, n int) string {
	return fmt.Sprintf("%s%d.log", LogPrefix(run, att, job, seq), n)
}

// FullLogPath is logs/<run>/<att>/<job>/<seq>/full.log.
func FullLogPath(run int64, att int, job string, seq int) string {
	return LogPrefix(run, att, job, seq) + "full.log"
}

// ResultPath is results/<run>/<att>/<job>/<seq>.
func ResultPath(run int64, att int, job string, seq int) string {
	return fmt.Sprintf("results/%s/%d", jobKey(run, att, job), seq)
}

// ResultPrefix lists every seq's result of a job.
func ResultPrefix(run int64, att int, job string) string {
	return "results/" + jobKey(run, att, job) + "/"
}

// RunnerPath is runners/<id>.json.
func RunnerPath(id string) string { return "runners/" + id + ".json" }
