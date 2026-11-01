package main

import "fmt"

// benchdiffMetric is one row of tstapler/benchdiff's canonical JSON; identity
// is (group, name, measure).
type benchdiffMetric struct {
	Group   string  `json:"group"`
	Name    string  `json:"name"`
	Measure string  `json:"measure,omitempty"`
	Unit    string  `json:"unit"`
	Better  string  `json:"better"`
	Value   float64 `json:"value"`
}

type benchdiffFile struct {
	Schema  int               `json:"schema"`
	Env     map[string]string `json:"env"`
	Metrics []benchdiffMetric `json:"metrics"`
}

func (r Results) benchdiffEnv() map[string]string {
	return map[string]string{
		"sha": r.Env.GitSHA, "cpu": r.Env.CPU, "chrome": r.Env.Chrome,
		"xterm": r.Env.XtermVer, "ghostty-web": r.Env.GhosttyVer,
	}
}

// benchdiffMetrics are the numbers a PR comment can afford to show per run:
// total p50 and p95, plus flood throughput. The full set stays in the
// --out JSON. Runs where Chrome evicted WebGL contexts yield nothing; they
// are not clean measurements.
func benchdiffMetrics(run Run, group string) []benchdiffMetric {
	if run.WebGLContextsLost > 0 {
		return nil
	}
	var ms []benchdiffMetric
	if t := run.Latency["total"]; t != nil {
		ms = append(ms,
			benchdiffMetric{group, "total", "p50", "ms", "lower", t.P50},
			benchdiffMetric{group, "total", "p95", "ms", "lower", t.P95})
	}
	if run.Scenario == "flood" && run.WallMs > 0 {
		mibs := float64(run.Bytes) / (1 << 20) / (run.WallMs / 1000)
		ms = append(ms, benchdiffMetric{group, "throughput", "", "MiB/s", "higher", mibs})
	}
	return ms
}

// benchdiff is every engine in one file, grouped "engine/scenario/sessions",
// for comparing this run with another commit's. Only point values are
// emitted: per-chunk samples queue behind one another, so a significance test
// over them would overstate confidence.
func (r Results) benchdiff() benchdiffFile {
	f := benchdiffFile{Schema: 1, Env: r.benchdiffEnv(), Metrics: []benchdiffMetric{}}
	for _, run := range r.Runs {
		f.Metrics = append(f.Metrics, benchdiffMetrics(run, fmt.Sprintf("%s/%s/%d", run.Engine, run.Scenario, run.Sessions))...)
	}
	return f
}

// benchdiffByEngine is one file per engine with identical groups
// ("scenario/sessions"), so benchdiff can compare two engines on the same run.
func (r Results) benchdiffByEngine() map[string]benchdiffFile {
	files := map[string]benchdiffFile{}
	for _, run := range r.Runs {
		f, ok := files[run.Engine]
		if !ok {
			f = benchdiffFile{Schema: 1, Env: r.benchdiffEnv(), Metrics: []benchdiffMetric{}}
		}
		f.Metrics = append(f.Metrics, benchdiffMetrics(run, fmt.Sprintf("%s/%d", run.Scenario, run.Sessions))...)
		files[run.Engine] = f
	}
	return files
}
