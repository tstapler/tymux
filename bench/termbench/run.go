package main

import (
	"bytes"
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	"connectrpc.com/connect"
	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
)

// maxQueuedBytes bounds one pane's unsent output; past it the reader stops
// receiving, which backpressures the daemon (and shows up as output gaps).
const maxQueuedBytes = 32 << 20

// warmupSamples are run before recording echo and baseline samples so JIT,
// glyph atlas and first-layout costs don't land in the percentiles.
const warmupSamples = 8

// rec is one chunk's life: received from tymuxd, written to the page, painted.
type rec struct {
	data              []byte
	nbytes            int
	recv, sent, ackAt time.Time
	parse, paint      float64 // page clock, ms
	acked             bool
}

type pane struct {
	idx    int
	id     string
	stream *connect.BidiStreamForClient[v1.AttachRequest, v1.AttachEvent]
	cancel context.CancelFunc

	mu       sync.Mutex
	cond     *sync.Cond
	recs     []rec
	sentN    int
	ackedN   int
	queued   int
	readDone bool
	err      error
	gaps     int

	window chan struct{} // in-flight chunk slots; sender fills, ack drains
	want   atomic.Int32  // echo: byte to wait for, 0 for none
	hit    chan int      // echo: index of the chunk containing it
}

// harness wires N panes (tymuxd side) to N terminals (page side).
type harness struct {
	d     *Daemon
	br    *Bridge
	panes []*pane
}

func newHarness(d *Daemon, br *Bridge, inflight int, ids []string) *harness {
	h := &harness{d: d, br: br}
	for i, id := range ids {
		p := &pane{idx: i, id: id, window: make(chan struct{}, inflight), hit: make(chan int, 1)}
		p.cond = sync.NewCond(&p.mu)
		h.panes = append(h.panes, p)
	}
	br.setAckHandler(h.onAck)
	return h
}

func (h *harness) onAck(acks []ack) {
	for _, a := range acks {
		if a.pane >= len(h.panes) {
			continue
		}
		p := h.panes[a.pane]
		p.mu.Lock()
		if int(a.seq) >= len(p.recs) {
			p.mu.Unlock()
			continue
		}
		r := &p.recs[a.seq]
		r.ackAt, r.parse, r.paint, r.acked = a.at, a.parse, a.pnt, true
		p.ackedN++
		p.mu.Unlock()
		select {
		case <-p.window:
		default:
		}
	}
}

// attach opens the pane's stream, skips the initial snapshot, and starts the
// receive and forward loops.
func (h *harness) attach(p *pane) error {
	ctx, cancel := context.WithCancel(context.Background())
	p.cancel = cancel
	p.stream = h.d.Client.Attach(ctx)
	if err := p.stream.Send(&v1.AttachRequest{Payload: &v1.AttachRequest_PaneId{PaneId: p.id}}); err != nil {
		cancel()
		return err
	}
	if _, err := p.stream.Receive(); err != nil {
		cancel()
		return fmt.Errorf("attach %s: %w", p.id, err)
	}
	go h.receive(p, ctx)
	go h.forward(p, ctx)
	return nil
}

func (h *harness) receive(p *pane, ctx context.Context) {
	defer func() {
		p.mu.Lock()
		p.readDone = true
		p.cond.Broadcast()
		p.mu.Unlock()
	}()
	for {
		p.mu.Lock()
		for p.queued > maxQueuedBytes && ctx.Err() == nil {
			p.cond.Wait()
		}
		p.mu.Unlock()
		if ctx.Err() != nil {
			return
		}

		ev, err := p.stream.Receive()
		recv := time.Now()
		if err != nil {
			if ctx.Err() == nil {
				p.mu.Lock()
				p.err = err
				p.mu.Unlock()
			}
			return
		}
		var data []byte
		switch e := ev.GetPayload().(type) {
		case *v1.AttachEvent_OutputChunk:
			data = e.OutputChunk.GetData()
		case *v1.AttachEvent_Output:
			data = e.Output
		case *v1.AttachEvent_OutputGap:
			p.mu.Lock()
			p.gaps++
			p.mu.Unlock()
			continue
		case *v1.AttachEvent_Exited:
			return
		default:
			continue
		}
		if len(data) == 0 {
			continue
		}
		p.mu.Lock()
		p.recs = append(p.recs, rec{data: data, nbytes: len(data), recv: recv})
		idx := len(p.recs) - 1
		p.queued += len(data)
		p.cond.Broadcast()
		p.mu.Unlock()
		if w := byte(p.want.Load()); w != 0 && bytes.IndexByte(data, w) >= 0 && p.want.CompareAndSwap(int32(w), 0) {
			p.hit <- idx
		}
	}
}

// forward sends queued chunks to the page, holding at most cap(window) unacked.
func (h *harness) forward(p *pane, ctx context.Context) {
	for {
		p.mu.Lock()
		for p.sentN >= len(p.recs) && !p.readDone {
			p.cond.Wait()
		}
		if p.sentN >= len(p.recs) {
			p.mu.Unlock()
			return
		}
		i := p.sentN
		p.mu.Unlock()

		select {
		case p.window <- struct{}{}:
		case <-ctx.Done():
			return
		}
		p.mu.Lock()
		data := p.recs[i].data
		p.recs[i].data = nil
		p.queued -= len(data)
		p.recs[i].sent = time.Now()
		p.sentN++
		p.cond.Broadcast()
		p.mu.Unlock()
		if err := h.br.Send(p.idx, uint32(i), data); err != nil {
			p.mu.Lock()
			p.err = err
			p.readDone = true
			p.mu.Unlock()
			return
		}
	}
}

// finished: the stream ended and every received chunk has been painted.
func (p *pane) finished() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.err != nil || (p.readDone && p.sentN == len(p.recs) && p.ackedN == len(p.recs))
}

func (p *pane) waitAcked(idx int, timeout time.Duration) error {
	return waitFor(func() bool {
		p.mu.Lock()
		defer p.mu.Unlock()
		return p.recs[idx].acked
	}, timeout)
}

func (h *harness) input(p *pane, b []byte) error {
	return p.stream.Send(&v1.AttachRequest{Payload: &v1.AttachRequest_Input{Input: b}})
}

func (h *harness) close() {
	h.br.setAckHandler(nil)
	for _, p := range h.panes {
		if p.cancel != nil {
			p.cancel()
		}
		// Wake receive/forward parked on the cond so they see the cancel and exit.
		p.mu.Lock()
		p.readDone = true
		p.cond.Broadcast()
		p.mu.Unlock()
	}
}

// collect folds every pane's painted chunks into run; wall time is measured
// from start to the last ack.
func (h *harness) collect(r *Run, start time.Time) {
	var total, queue, render, parse, paint []float64
	var last time.Time
	for _, p := range h.panes {
		p.mu.Lock()
		for _, c := range p.recs {
			if !c.acked {
				continue
			}
			total = append(total, ms(c.ackAt.Sub(c.recv)))
			queue = append(queue, ms(c.sent.Sub(c.recv)))
			render = append(render, ms(c.ackAt.Sub(c.sent)))
			parse = append(parse, c.parse)
			paint = append(paint, c.paint)
			if c.ackAt.After(last) {
				last = c.ackAt
			}
		}
		r.Chunks += p.ackedN
		for _, c := range p.recs {
			r.Bytes += int64(c.nbytes)
		}
		r.Gaps += p.gaps
		p.mu.Unlock()
	}
	if !last.IsZero() {
		r.WallMs = ms(last.Sub(start))
	}
	r.Latency = map[string]*Stats{
		"total": summarize(total), "queue": summarize(queue), "render": summarize(render),
		"parse": summarize(parse), "paint": summarize(paint),
	}
}

func ms(d time.Duration) float64 { return float64(d) / float64(time.Millisecond) }

// runStream attaches n panes running script and measures them until the
// workloads end and everything is painted.
func runStream(d *Daemon, br *Bridge, engine, name string, n int, script func(n int) string, inflight int) (Run, error) {
	run := Run{Engine: engine, Scenario: name, Sessions: n}
	var sessions, ids []string
	defer func() { killAll(d, sessions) }()
	for i := 0; i < n; i++ {
		sid, pid, err := d.NewPane(fmt.Sprintf("%s-%d", name, i), script(n))
		if err != nil {
			return run, err
		}
		sessions, ids = append(sessions, sid), append(ids, pid)
	}
	pg, err := br.Init(engine, n)
	if err != nil {
		return run, err
	}
	run.Scale, run.DPR = pg.Scale, pg.DPR
	h := newHarness(d, br, inflight, ids)
	defer h.close()
	for _, p := range h.panes {
		if err := h.attach(p); err != nil {
			return run, err
		}
	}
	start := time.Now()
	for _, p := range h.panes {
		if err := h.input(p, []byte("\n")); err != nil {
			return run, err
		}
	}
	err = waitFor(func() bool {
		for _, p := range h.panes {
			if !p.finished() {
				return false
			}
		}
		return true
	}, 120*time.Second)
	for _, p := range h.panes {
		p.mu.Lock()
		if p.err != nil && err == nil {
			err = p.err
		}
		p.mu.Unlock()
	}
	h.collect(&run, start)
	return run, err
}

func killAll(d *Daemon, sessions []string) {
	for _, s := range sessions {
		_, _ = d.Client.KillSession(context.Background(), connect.NewRequest(&v1.KillSessionRequest{SessionId: s}))
	}
}

// runEcho times keystroke -> painted echo in pane 0 while the other n-1 panes
// are idle (bg == nil) or running the background script.
func runEcho(d *Daemon, br *Bridge, engine, name string, n, samples int, bg func() string, inflight int) (Run, error) {
	run := Run{Engine: engine, Scenario: name, Sessions: n}
	var sessions, ids []string
	defer func() { killAll(d, sessions) }()
	add := func(sid, pid string, err error) error {
		if err == nil {
			sessions, ids = append(sessions, sid), append(ids, pid)
		}
		return err
	}
	if err := add(d.NewEchoPane(name + "-echo")); err != nil {
		return run, err
	}
	for i := 1; i < n; i++ {
		var err error
		if bg != nil {
			err = add(d.NewPane(fmt.Sprintf("%s-bg%d", name, i), bg()))
		} else {
			err = add(d.NewEchoPane(fmt.Sprintf("%s-idle%d", name, i)))
		}
		if err != nil {
			return run, err
		}
	}
	pg, err := br.Init(engine, n)
	if err != nil {
		return run, err
	}
	run.Scale, run.DPR = pg.Scale, pg.DPR
	h := newHarness(d, br, inflight, ids)
	defer h.close()
	for _, p := range h.panes {
		if err := h.attach(p); err != nil {
			return run, err
		}
	}
	if bg != nil {
		for _, p := range h.panes[1:] {
			if err := h.input(p, []byte("\n")); err != nil {
				return run, err
			}
		}
		time.Sleep(300 * time.Millisecond) // let the load ramp up
	}

	p := h.panes[0]
	var total, grpc, queue, render, parse, paint []float64
	for i := 0; i < samples+warmupSamples; i++ {
		ch := byte('a' + i%26)
		p.want.Store(int32(ch))
		t0 := time.Now()
		if err := h.input(p, []byte{ch}); err != nil {
			return run, err
		}
		var idx int
		select {
		case idx = <-p.hit:
		case <-time.After(5 * time.Second):
			return run, fmt.Errorf("echo %q never came back", ch)
		}
		if err := p.waitAcked(idx, 30*time.Second); err != nil {
			return run, fmt.Errorf("echo chunk never painted: %w", err)
		}
		if i < warmupSamples {
			time.Sleep(20 * time.Millisecond)
			continue
		}
		p.mu.Lock()
		c := p.recs[idx]
		p.mu.Unlock()
		total = append(total, ms(c.ackAt.Sub(t0)))
		grpc = append(grpc, ms(c.recv.Sub(t0)))
		queue = append(queue, ms(c.sent.Sub(c.recv)))
		render = append(render, ms(c.ackAt.Sub(c.sent)))
		parse = append(parse, c.parse)
		paint = append(paint, c.paint)
		time.Sleep(20 * time.Millisecond) // let the previous paint settle
	}
	p.mu.Lock()
	run.Chunks = len(total)
	run.Gaps = p.gaps
	p.mu.Unlock()
	run.Latency = map[string]*Stats{
		"total": summarize(total), "grpc": summarize(grpc), "queue": summarize(queue),
		"render": summarize(render), "parse": summarize(parse), "paint": summarize(paint),
	}
	return run, nil
}

// runBaseline writes one byte at a time straight from Go to the page, with no
// tymuxd involved: the engine's own floor (parse + wait for the next frame).
func runBaseline(br *Bridge, engine string, samples int) (Run, error) {
	run := Run{Engine: engine, Scenario: "baseline", Sessions: 1}
	pg, err := br.Init(engine, 1)
	if err != nil {
		return run, err
	}
	run.Scale, run.DPR = pg.Scale, pg.DPR
	p := &pane{idx: 0, window: make(chan struct{}, 1), hit: make(chan int, 1)}
	p.cond = sync.NewCond(&p.mu)
	h := &harness{br: br, panes: []*pane{p}}
	br.setAckHandler(h.onAck)
	defer br.setAckHandler(nil)
	var total, parse, paint []float64
	for i := 0; i < samples+warmupSamples; i++ {
		p.mu.Lock()
		p.recs = append(p.recs, rec{recv: time.Now()})
		idx := len(p.recs) - 1
		p.recs[idx].sent = p.recs[idx].recv
		p.mu.Unlock()
		if err := br.Send(0, uint32(idx), []byte{byte('a' + i%26)}); err != nil {
			return run, err
		}
		if err := p.waitAcked(idx, 10*time.Second); err != nil {
			return run, err
		}
		if i < warmupSamples {
			time.Sleep(20 * time.Millisecond)
			continue
		}
		p.mu.Lock()
		c := p.recs[idx]
		p.mu.Unlock()
		total = append(total, ms(c.ackAt.Sub(c.sent)))
		parse = append(parse, c.parse)
		paint = append(paint, c.paint)
		time.Sleep(20 * time.Millisecond)
	}
	run.Chunks = len(total)
	// Nothing queues or crosses gRPC here, so written-to-painted is the whole path.
	run.Latency = map[string]*Stats{"total": summarize(total), "render": summarize(total), "parse": summarize(parse), "paint": summarize(paint)}
	return run, nil
}
