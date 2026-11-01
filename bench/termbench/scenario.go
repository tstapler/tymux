package main

import (
	"fmt"
	"strings"
)

// sizes scales every scenario; quick is a smoke test, full is for publishing.
type sizes struct {
	floodBytes   int // total across a scenario's sessions
	tuiFrames    int
	fanoutLines  int
	echoSamples  int
	baseSamples  int
	bgFloodBytes int
}

var (
	fullSizes  = sizes{floodBytes: 16 << 20, tuiFrames: 200, fanoutLines: 300, echoSamples: 200, baseSamples: 200, bgFloodBytes: 256 << 20}
	quickSizes = sizes{floodBytes: 2 << 20, tuiFrames: 60, fanoutLines: 100, echoSamples: 40, baseSamples: 40, bgFloodBytes: 64 << 20}
)

type scenario struct {
	name     string
	sessions []int
	run      func(d *Daemon, br *Bridge, engine string, n int, sz sizes, inflight int) (Run, error)
}

// floodScript prints SGR-colored text as fast as the pty allows.
func floodScript(bytes int) string {
	return fmt.Sprintf(`yes "$(printf '\033[32mlorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod\033[0m')" | head -c %d`, bytes)
}

// tuiScript redraws a full 80x24 screen of colored rows each frame on the
// alternate screen, ~60 fps, like a TUI under constant update.
func tuiScript(frames int) string {
	return fmt.Sprintf(`printf '\033[?1049h'; for f in $(seq 1 %d); do awk -v f=$f 'BEGIN{printf "\033[H"; for(r=1;r<=24;r++) printf "\033[%%d;1H\033[38;5;%%dm%%-80s\033[0m", r, (f+r)%%256, "frame " f " row " r}'; sleep 0.016; done; printf '\033[?1049l'`, frames)
}

// fanoutScript prints one line every 10 ms: a steady, moderate stream.
func fanoutScript(lines int) string {
	return fmt.Sprintf(`for i in $(seq 1 %d); do printf 'session output line %%d ................................\n' $i; sleep 0.01; done`, lines)
}

func streamScenario(name string, sessions []int, script func(sz sizes, n int) string) scenario {
	return scenario{name: name, sessions: sessions,
		run: func(d *Daemon, br *Bridge, engine string, n int, sz sizes, inflight int) (Run, error) {
			return runStream(d, br, engine, name, n, func(n int) string { return script(sz, n) }, inflight)
		}}
}

func scenarios() []scenario {
	return []scenario{
		{name: "baseline", sessions: []int{1},
			run: func(_ *Daemon, br *Bridge, engine string, _ int, sz sizes, _ int) (Run, error) {
				return runBaseline(br, engine, sz.baseSamples)
			}},
		{name: "echo", sessions: []int{1, 16},
			run: func(d *Daemon, br *Bridge, engine string, n int, sz sizes, inflight int) (Run, error) {
				return runEcho(d, br, engine, "echo", n, sz.echoSamples, nil, inflight)
			}},
		{name: "echo-load", sessions: []int{4},
			run: func(d *Daemon, br *Bridge, engine string, n int, sz sizes, inflight int) (Run, error) {
				bg := func() string { return floodScript(sz.bgFloodBytes) }
				return runEcho(d, br, engine, "echo-load", n, sz.echoSamples, bg, inflight)
			}},
		streamScenario("flood", []int{1, 4}, func(sz sizes, n int) string { return floodScript(max(sz.floodBytes/n, 256<<10)) }),
		streamScenario("tui", []int{1, 4, 16}, func(sz sizes, _ int) string { return tuiScript(sz.tuiFrames) }),
		streamScenario("fanout", []int{1, 4, 16, 64}, func(sz sizes, _ int) string { return fanoutScript(sz.fanoutLines) }),
	}
}

func scenarioNames() []string {
	var names []string
	for _, s := range scenarios() {
		names = append(names, s.name)
	}
	return names
}

func selectScenarios(csv string) ([]scenario, error) {
	byName := map[string]scenario{}
	for _, s := range scenarios() {
		byName[s.name] = s
	}
	var out []scenario
	for _, n := range strings.Split(csv, ",") {
		s, ok := byName[strings.TrimSpace(n)]
		if !ok {
			return nil, fmt.Errorf("unknown scenario %q (have %s)", n, strings.Join(scenarioNames(), ", "))
		}
		out = append(out, s)
	}
	return out, nil
}
