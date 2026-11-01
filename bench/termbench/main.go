// Command termbench measures how fast pty output from tymuxd, streamed over
// gRPC by the Go client, shows up in browser terminals (xterm.js and
// ghostty-web) running in headless Chrome.
//
//	scripts/termbench.sh --out docs/benchmarks/terminal-results.json
//
// Per chunk the clock runs from "Go received it from tymuxd" (or, for echo,
// "Go sent the keystroke") to "the page's next animation frame after the
// terminal parsed it". Go owns the single clock for those totals; the page's
// own parse and paint durations are reported alongside.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"
)

type Run struct {
	Engine   string `json:"engine"`
	Scenario string `json:"scenario"`
	Sessions int    `json:"sessions"`
	Chunks   int    `json:"chunks"`
	Bytes    int64  `json:"bytes,omitempty"`
	// WallMs is first input to last paint (stream scenarios).
	WallMs float64 `json:"wall_ms,omitempty"`
	// Gaps counts output_gap events: tymuxd dropped output for a slow client.
	Gaps int `json:"output_gaps"`
	// Latency keys: total (see the package comment), grpc (echo: keystroke to
	// receipt), queue (receipt to written to the page), render (written to
	// painted, including the WebSocket hop), parse and paint (page clock).
	Latency map[string]*Stats `json:"latency_ms"`
	// Scale is the factor the page shrank its terminal grid by to fit the viewport.
	Scale float64 `json:"grid_scale"`
	// WebGLContextsLost > 0 means Chrome evicted WebGL contexts during the run,
	// so an xterm-webgl row is not a clean WebGL measurement.
	WebGLContextsLost int     `json:"webgl_contexts_lost,omitempty"`
	DPR               float64 `json:"device_pixel_ratio"`
}

type Env struct {
	GeneratedAt string `json:"generated_at"`
	GitSHA      string `json:"tymux_git_sha"`
	Chrome      string `json:"chrome"`
	Kernel      string `json:"kernel"`
	CPU         string `json:"cpu"`
	Cores       int    `json:"cores"`
	Window      string `json:"window"`
	InFlight    int    `json:"in_flight_chunks_per_pane"`
	UncapFrames bool   `json:"uncapped_frame_rate"`
	XtermVer    string `json:"xterm_version"`
	GhosttyVer  string `json:"ghostty_web_version"`
}

type Results struct {
	Env  Env   `json:"env"`
	Runs []Run `json:"runs"`
}

func main() {
	tymuxd := flag.String("tymuxd", "target/release/tymuxd", "path to the tymuxd binary")
	chrome := flag.String("chrome", "", "Chrome/Chromium binary (default: first of google-chrome-stable, chromium, chrome on PATH)")
	webDir := flag.String("web-dir", "bench/termbench/web", "directory with index.html and node_modules (npm ci there first)")
	engines := flag.String("engines", "xterm,xterm-webgl,ghostty", "comma-separated terminal engines")
	scenarios := flag.String("scenarios", strings.Join(scenarioNames(), ","), "comma-separated scenarios")
	quick := flag.Bool("quick", false, "smoke-test sizes")
	inflight := flag.Int("inflight", 32, "unacked chunks allowed per pane before the sender waits")
	window := flag.String("window", "1920,1080", "browser viewport as W,H")
	uncap := flag.Bool("uncap-frames", false, "lift Chrome's 60 Hz frame cap (removes the vsync floor from paint latency)")
	chromeArgs := flag.String("chrome-args", "", "extra space-separated Chrome flags")
	out := flag.String("out", "", "write results JSON here (default: print the table only)")
	diffOut := flag.String("benchdiff-out", "", "also write benchdiff canonical JSON here (all engines, for comparing commits)")
	diffDir := flag.String("benchdiff-engines-dir", "", "also write one benchdiff JSON per engine into this directory (for comparing engines)")
	flag.Parse()

	chromeBin := *chrome
	if chromeBin == "" {
		for _, c := range []string{"google-chrome-stable", "chromium", "chromium-browser", "chrome"} {
			if p, err := exec.LookPath(c); err == nil {
				chromeBin = p
				break
			}
		}
	}
	if chromeBin == "" {
		fatalf("no Chrome found; pass --chrome")
	}
	if _, err := os.Stat(*webDir + "/node_modules"); err != nil {
		fatalf("%s/node_modules missing: run `npm ci` in %s", *webDir, *webDir)
	}
	sel, err := selectScenarios(*scenarios)
	if err != nil {
		fatalf("%v", err)
	}
	sz := fullSizes
	if *quick {
		sz = quickSizes
	}

	d, err := startDaemon(*tymuxd)
	if err != nil {
		fatalf("%v", err)
	}
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)

	res := Results{Env: collectEnv(chromeBin, *window, *inflight, *uncap, *webDir)}
	var brMu sync.Mutex
	var br *Bridge
	setBridge := func(b *Bridge) {
		brMu.Lock()
		br = b
		brMu.Unlock()
	}
	cleanup := func() {
		brMu.Lock()
		b := br
		br = nil
		brMu.Unlock()
		if b != nil {
			b.Close()
		}
		d.Stop()
	}
	go func() { <-sig; cleanup(); os.Exit(130) }()
	fail := func(format string, a ...any) {
		cleanup()
		fatalf(format, a...)
	}
	failed := 0

	for _, engine := range strings.Split(*engines, ",") {
		cur, err := newBridge(*webDir)
		if err != nil {
			fail("%v", err)
		}
		setBridge(cur)
		if err := cur.Launch(chromeBin, *window, *uncap, strings.Fields(*chromeArgs)); err != nil {
			fail("%s: %v", engine, err)
		}
		for _, sc := range sel {
			for _, n := range sc.sessions {
				fmt.Fprintf(os.Stderr, "== %s / %s / %d session(s)\n", engine, sc.name, n)
				run, err := sc.run(d, cur, engine, n, sz, *inflight)
				if err != nil {
					// Keep what the other cells measured; a failed cell is reported, not hidden.
					fmt.Fprintf(os.Stderr, "   FAILED: %v\n", err)
					failed++
					continue
				}
				run.WebGLContextsLost = int(cur.lost.Load())
				res.Runs = append(res.Runs, run)
			}
		}
		setBridge(nil)
		cur.Close()
	}
	d.Stop()

	printTable(res)
	if *out != "" {
		data, _ := json.MarshalIndent(res, "", "  ")
		if err := os.WriteFile(*out, append(data, '\n'), 0o644); err != nil {
			fail("write %s: %v", *out, err)
		}
		fmt.Fprintf(os.Stderr, "wrote %s\n", *out)
	}
	if *diffOut != "" {
		data, _ := json.MarshalIndent(res.benchdiff(), "", "  ")
		if err := os.WriteFile(*diffOut, append(data, '\n'), 0o644); err != nil {
			fail("write %s: %v", *diffOut, err)
		}
		fmt.Fprintf(os.Stderr, "wrote %s\n", *diffOut)
	}
	if *diffDir != "" {
		if err := os.MkdirAll(*diffDir, 0o755); err != nil {
			fail("%v", err)
		}
		for engine, f := range res.benchdiffByEngine() {
			data, _ := json.MarshalIndent(f, "", "  ")
			path := *diffDir + "/" + engine + ".json"
			if err := os.WriteFile(path, append(data, '\n'), 0o644); err != nil {
				fail("write %s: %v", path, err)
			}
			fmt.Fprintf(os.Stderr, "wrote %s\n", path)
		}
	}
	if failed > 0 {
		fmt.Fprintf(os.Stderr, "%d cell(s) failed\n", failed)
		os.Exit(1)
	}
}

func printTable(res Results) {
	fmt.Printf("\n%-12s %-10s %4s %7s %8s | %8s %8s %8s | %8s %8s %8s %8s\n",
		"engine", "scenario", "sess", "chunks", "MiB/s", "p50 ms", "p95 ms", "p99 ms", "queue50", "render50", "parse50", "paint50")
	for _, r := range res.Runs {
		t := r.Latency["total"]
		if t == nil {
			fmt.Printf("%-12s %-10s %4d %7d   (no painted chunks)\n", r.Engine, r.Scenario, r.Sessions, r.Chunks)
			continue
		}
		// Only flood is bounded by the terminal; the others are paced by their scripts.
		mibs := ""
		if r.Scenario == "flood" && r.WallMs > 0 {
			mibs = fmt.Sprintf("%.1f", float64(r.Bytes)/(1<<20)/(r.WallMs/1000))
		}
		p50 := func(k string) float64 {
			if s := r.Latency[k]; s != nil {
				return s.P50
			}
			return 0
		}
		fmt.Printf("%-12s %-10s %4d %7d %8s | %8.2f %8.2f %8.2f | %8.2f %8.2f %8.2f %8.2f\n",
			r.Engine, r.Scenario, r.Sessions, r.Chunks, mibs, t.P50, t.P95, t.P99, p50("queue"), p50("render"), p50("parse"), p50("paint"))
		if r.Gaps > 0 {
			fmt.Printf("  ^ %d output gap(s): tymuxd dropped output for this client\n", r.Gaps)
		}
		if r.WebGLContextsLost > 0 {
			fmt.Printf("  ^ %d WebGL context(s) lost: not a clean WebGL measurement\n", r.WebGLContextsLost)
		}
	}
}

func collectEnv(chrome, window string, inflight int, uncap bool, webDir string) Env {
	e := Env{
		GeneratedAt: time.Now().UTC().Format(time.RFC3339),
		Cores:       runtime.NumCPU(),
		GitSHA:      run("git", "rev-parse", "--short", "HEAD"),
		Chrome:      run(chrome, "--version"),
		Kernel:      run("uname", "-sr"),
		Window:      window,
		InFlight:    inflight,
		UncapFrames: uncap,
		XtermVer:    npmVersion(webDir, "@xterm/xterm"),
		GhosttyVer:  npmVersion(webDir, "ghostty-web"),
	}
	if data, err := os.ReadFile("/proc/cpuinfo"); err == nil {
		for _, line := range strings.Split(string(data), "\n") {
			if k, v, ok := strings.Cut(line, ":"); ok && strings.TrimSpace(k) == "model name" {
				e.CPU = strings.TrimSpace(v)
				break
			}
		}
	}
	return e
}

func npmVersion(webDir, pkg string) string {
	data, err := os.ReadFile(webDir + "/node_modules/" + pkg + "/package.json")
	if err != nil {
		return ""
	}
	var p struct {
		Version string `json:"version"`
	}
	_ = json.Unmarshal(data, &p)
	return p.Version
}

func run(name string, args ...string) string {
	out, err := exec.Command(name, args...).Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

func fatalf(format string, a ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", a...)
	os.Exit(1)
}
