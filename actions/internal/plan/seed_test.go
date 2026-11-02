package plan

// seed is the list of events each corpus workflow is planned for. It only seeds fixtures
// that do not exist yet (-update); afterwards the fixture is the source of truth.
var seed = map[string][]Scenario{
	"workflows/triggers-push-pr.yml": {
		{Name: "push main, src changed", Input: in("push", "main", "src/lib.rs")},
		{Name: "push main, only docs and src markdown", Input: in("push", "main", "docs/a.md", "src/readme.md")},
		{Name: "push release/1.x", Input: in("push", "refs/heads/release/1.x", "Cargo.toml")},
		{Name: "push release/old-1 is excluded", Input: in("push", "release/old-1", "src/lib.rs")},
		{Name: "push tag, paths not evaluated", Input: in("push", "refs/tags/v1.2.3")},
		{Name: "push other branch", Input: in("push", "feature/x", "src/lib.rs")},
		{Name: "pull_request to main, src changed", Input: in("pull_request", "main", "src/lib.rs")},
		{Name: "pull_request to main, docs only", Input: in("pull_request", "main", "docs/a.md", "README.md")},
		{Name: "pull_request to dev", Input: in("pull_request", "dev", "src/lib.rs")},
		{Name: "pull_request labeled is not a listed type", Input: Input{Event: "pull_request", Ref: "main", Type: "labeled"}},
	},
	"workflows/triggers-branch-ignore.yml": {
		{Name: "push dependabot branch", Input: in("push", "dependabot/npm/x", "src/a.js")},
		{Name: "push main, markdown only", Input: in("push", "main", "README.md", "LICENSE")},
		{Name: "push main, code", Input: in("push", "main", "src/a.js", "README.md")},
		{Name: "pull_request_target labeled", Input: Input{Event: "pull_request_target", Ref: "main", Type: "labeled"}},
		{Name: "pull_request_target synchronize is not listed", Input: Input{Event: "pull_request_target", Ref: "main", Type: "synchronize"}},
	},
	"workflows/triggers-dispatch.yml": {
		{Name: "workflow_dispatch", Input: in("workflow_dispatch", "main")},
		{Name: "push does not trigger", Input: in("push", "main")},
	},
	"workflows/triggers-schedule.yml": {
		{Name: "nightly cron", Input: Input{Event: "schedule", Ref: "main", Cron: "17 3 * * *"}},
		{Name: "weekly cron", Input: Input{Event: "schedule", Ref: "main", Cron: "0 12 * * 1"}},
		{Name: "unknown cron", Input: Input{Event: "schedule", Ref: "main", Cron: "5 5 * * *"}},
		{Name: "workflow_dispatch", Input: in("workflow_dispatch", "main")},
	},
	"workflows/needs-graph.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "pull_request", Input: in("pull_request", "main")},
		{Name: "release event does not trigger", Input: in("release", "main")},
	},
	"workflows/matrix-basic.yml":        {{Name: "push", Input: in("push", "main")}},
	"workflows/matrix-include-only.yml": {{Name: "push", Input: in("push", "main")}},
	"workflows/matrix-dynamic.yml":      {{Name: "pull_request", Input: in("pull_request", "main")}},
	"workflows/concurrency-cancel.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "push other branch", Input: in("push", "dev")},
		{Name: "pull_request", Input: in("pull_request", "main")},
	},
	"workflows/environments-approval.yml": {
		{Name: "push tag", Input: in("push", "refs/tags/v1.0.0")},
		{Name: "push branch", Input: in("push", "main")},
	},
	"workflows/reusable-callee.yml": {
		{Name: "workflow_call", Input: in("workflow_call", "")},
		{Name: "push does not trigger", Input: in("push", "main")},
	},
	"workflows/reusable-caller.yml":   {{Name: "push main", Input: in("push", "main")}},
	"workflows/services-postgres.yml": {{Name: "pull_request", Input: in("pull_request", "main")}},
	"workflows/cache-artifacts.yml":   {{Name: "push", Input: in("push", "main")}},
	"workflows/contexts-env.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "push tag", Input: in("push", "refs/tags/v1")},
		{Name: "pull_request", Input: in("pull_request", "main")},
		{Name: "workflow_dispatch", Input: in("workflow_dispatch", "main")},
	},
	"workflows/continue-on-error-timeouts.yml": {{Name: "push", Input: in("push", "main")}},
	"workflows/release-tag.yml": {
		{Name: "tag v1.2.3", Input: in("push", "refs/tags/v1.2.3")},
		{Name: "tag v1.2 does not match the pattern", Input: in("push", "refs/tags/v1.2")},
		{Name: "branch push", Input: in("push", "main")},
	},
	"workflows/oss-rust-ci.yml": {
		{Name: "push master", Input: in("push", "master")},
		{Name: "pull_request to master", Input: in("pull_request", "master")},
		{Name: "push main is not master", Input: in("push", "main")},
	},
	"workflows/oss-node-monorepo.yml": {
		{Name: "push main, package changed", Input: in("push", "main", "packages/a/index.ts")},
		{Name: "push main, only docs", Input: in("push", "main", "docs/x.md")},
		{Name: "pull_request, lockfile changed", Input: in("pull_request", "main", "pnpm-lock.yaml")},
	},
	"workflows/oss-python-lib.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "release published", Input: Input{Event: "release", Type: "published"}},
		{Name: "release created is not listed", Input: Input{Event: "release", Type: "created"}},
	},
	"workflows/oss-triage-bot.yml": {
		{Name: "issue opened", Input: Input{Event: "issues", Type: "opened"}},
		{Name: "issue closed is not listed", Input: Input{Event: "issues", Type: "closed"}},
		{Name: "issue_comment created", Input: Input{Event: "issue_comment", Type: "created"}},
		{Name: "pull_request_target opened", Input: Input{Event: "pull_request_target", Ref: "main", Type: "opened"}},
		{Name: "schedule", Input: Input{Event: "schedule", Cron: "30 1 * * *"}},
	},
	"workflows/oss-docker-publish.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "pull_request touching docker/", Input: in("pull_request", "main", "docker/a")},
		{Name: "pull_request not touching docker", Input: in("pull_request", "main", "src/a")},
	},
	"pail/ci.yml": {
		{Name: "push main", Input: in("push", "main")},
		{Name: "push other branch", Input: in("push", "feature")},
		{Name: "pull_request", Input: in("pull_request", "main")},
	},
	"pail/model.yml": {
		{Name: "workflow_dispatch", Input: in("workflow_dispatch", "main")},
		{Name: "push does not trigger", Input: in("push", "main")},
	},
	"pail/release.yml": {
		{Name: "tag v0.1.0", Input: in("push", "refs/tags/v0.1.0")},
		{Name: "push main", Input: in("push", "main")},
	},
	"pail/site.yml": {
		{Name: "push main, site changed", Input: in("push", "main", "site/index.html")},
		{Name: "push main, src only", Input: in("push", "main", "src/main.rs")},
		{Name: "workflow_dispatch", Input: in("workflow_dispatch", "main")},
	},
	"pail/web.yml": {
		{Name: "pull_request, web changed", Input: in("pull_request", "main", "web/src/app.tsx")},
		{Name: "pull_request, docs only", Input: in("pull_request", "main", "docs/a.md")},
		{Name: "push main, Cargo.lock changed", Input: in("push", "main", "Cargo.lock")},
	},
}
