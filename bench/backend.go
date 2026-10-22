package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"connectrpc.com/connect"
	v1 "github.com/tstapler/tymux/clients/go/gen/tymux/v1"
	"github.com/tstapler/tymux/clients/go/gen/tymux/v1/tymuxv1connect"
	"github.com/tstapler/tymux/clients/go/udsdialer"
)

// Backend is one multiplexer under test. Every backend creates a session
// running /bin/cat (blocks on the pty, no shell startup in the numbers).
type Backend interface {
	Name() string
	// Start launches an isolated server and returns once it answers a request,
	// so the time Start takes is the cold-start time.
	Start(workdir string) error
	Create(i int) (id string, err error)
	Kill(id string) error
	List() error
	// Capture returns the pane's screen as text.
	Capture(id string) (string, error)
	// ServerPID is the multiplexer server process, for RSS sampling.
	ServerPID() int
	// OwnsServer is false when another backend row measures the same server
	// (memory would just repeat).
	OwnsServer() bool
	Stop()
}

// Echoer is a backend that can type into a session and watch the echo come
// back over a persistent connection.
type Echoer interface {
	// EchoSession opens the connection once; Echo then times one keystroke.
	EchoSession(id string) (Echo, error)
}

type Echo interface {
	RoundTrip(ch byte) (time.Duration, error)
	Close()
}

// Reattacher times getting a dropped client streaming again.
type Reattacher interface {
	// ReattachTarget creates a session that produces output continuously.
	ReattachTarget() (id string, err error)
	// Reattach drops the client and reconnects, returning the time until the
	// stream is flowing and whether the stream was provably gap-free.
	Reattach(id string) (d time.Duration, contiguous bool, err error)
}

const (
	sessionCmd = "/bin/cat"
	// A session that keeps printing, for reattach.
	chattyScript = "while :; do printf x; sleep 0.01; done"
)

// ---- tymuxd over gRPC ----

type tymuxAPI struct {
	bin    string
	cmd    *exec.Cmd
	sock   string
	client tymuxv1connect.TymuxServiceClient
	panes  sync.Map // session id -> pane id
}

func (b *tymuxAPI) Name() string     { return "tymux (gRPC API)" }
func (b *tymuxAPI) OwnsServer() bool { return true }

func (b *tymuxAPI) Start(workdir string) error {
	// sun_path is limited to 108 bytes and tymuxd wants to create (mode 0700)
	// the socket's parent itself, so the dir must not exist yet.
	b.sock = filepath.Join(workdir, "tymuxd", "t.sock")
	port, err := freePort()
	if err != nil {
		return err
	}
	b.cmd = exec.Command(b.bin)
	b.cmd.Env = append(os.Environ(),
		"TYMUXD_SOCKET_PATH="+b.sock,
		"TYMUXD_ADDR=127.0.0.1:"+strconv.Itoa(port),
		"XDG_DATA_HOME="+filepath.Join(workdir, "data"),
		"XDG_STATE_HOME="+filepath.Join(workdir, "state"),
	)
	b.cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := b.cmd.Start(); err != nil {
		return fmt.Errorf("start tymuxd: %w", err)
	}
	b.client = tymuxv1connect.NewTymuxServiceClient(udsdialer.DialUnixHTTPClient(b.sock), "http://unix", connect.WithGRPC())
	return waitFor(func() bool {
		_, err := b.client.ListSessions(context.Background(), connect.NewRequest(&v1.ListSessionsRequest{}))
		return err == nil
	}, 10*time.Second)
}

func (b *tymuxAPI) create(name, command string, args []string) (*v1.Session, error) {
	r, err := b.client.CreateSession(context.Background(), connect.NewRequest(&v1.CreateSessionRequest{
		Name: name, Command: command, Args: args, Cwd: "/tmp",
	}))
	if err != nil {
		return nil, err
	}
	if ids := flattenPaneIDs(r.Msg.GetWindows()[0].GetLayout()); len(ids) > 0 {
		b.panes.Store(r.Msg.Id, ids[0])
	}
	return r.Msg, nil
}

func (b *tymuxAPI) Create(i int) (string, error) {
	s, err := b.create(fmt.Sprintf("s%d", i), sessionCmd, nil)
	if err != nil {
		return "", err
	}
	return s.Id, nil
}

func (b *tymuxAPI) Kill(id string) error {
	_, err := b.client.KillSession(context.Background(), connect.NewRequest(&v1.KillSessionRequest{SessionId: id}))
	return err
}

func (b *tymuxAPI) List() error {
	_, err := b.client.ListSessions(context.Background(), connect.NewRequest(&v1.ListSessionsRequest{}))
	return err
}

func (b *tymuxAPI) paneID(id string) (string, error) {
	if p, ok := b.panes.Load(id); ok {
		return p.(string), nil
	}
	r, err := b.client.ListSessions(context.Background(), connect.NewRequest(&v1.ListSessionsRequest{}))
	if err != nil {
		return "", err
	}
	for _, s := range r.Msg.Sessions {
		if s.Id == id {
			if ids := flattenPaneIDs(s.GetWindows()[0].GetLayout()); len(ids) > 0 {
				b.panes.Store(id, ids[0])
				return ids[0], nil
			}
		}
	}
	return "", fmt.Errorf("no pane for session %s", id)
}

func (b *tymuxAPI) Capture(id string) (string, error) {
	pane, err := b.paneID(id)
	if err != nil {
		return "", err
	}
	r, err := b.client.CapturePane(context.Background(), connect.NewRequest(&v1.CapturePaneRequest{PaneId: pane}))
	if err != nil {
		return "", err
	}
	var sb strings.Builder
	for _, row := range r.Msg.Grid {
		for _, c := range row.Cells {
			sb.WriteString(c.Text)
		}
		sb.WriteByte('\n')
	}
	return sb.String(), nil
}

func (b *tymuxAPI) ServerPID() int { return b.cmd.Process.Pid }

func (b *tymuxAPI) Stop() {
	if b.cmd != nil && b.cmd.Process != nil {
		_ = b.cmd.Process.Kill()
		_ = b.cmd.Wait()
	}
}

func (b *tymuxAPI) EchoSession(id string) (Echo, error) {
	pane, err := b.paneID(id)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	stream := b.client.Attach(ctx)
	if err := stream.Send(&v1.AttachRequest{Payload: &v1.AttachRequest_PaneId{PaneId: pane}}); err != nil {
		cancel()
		return nil, err
	}
	// The first event is the initial snapshot; wait for it so it can't be
	// mistaken for echo.
	if _, err := stream.Receive(); err != nil {
		cancel()
		return nil, err
	}
	return &apiEcho{stream: stream, cancel: cancel}, nil
}

type apiEcho struct {
	stream *connect.BidiStreamForClient[v1.AttachRequest, v1.AttachEvent]
	cancel context.CancelFunc
}

func (e *apiEcho) RoundTrip(ch byte) (time.Duration, error) {
	t := time.Now()
	if err := e.stream.Send(&v1.AttachRequest{Payload: &v1.AttachRequest_Input{Input: []byte{ch}}}); err != nil {
		return 0, err
	}
	for {
		ev, err := e.stream.Receive()
		if err != nil {
			return 0, err
		}
		if out, ok := ev.GetPayload().(*v1.AttachEvent_Output); ok && strings.IndexByte(string(out.Output), ch) >= 0 {
			return time.Since(t), nil
		}
	}
}

func (e *apiEcho) Close() { e.cancel() }

func (b *tymuxAPI) ReattachTarget() (string, error) {
	s, err := b.create("chatty", "/bin/sh", []string{"-c", chattyScript})
	if err != nil {
		return "", err
	}
	return s.Id, nil
}

// reattachState remembers the last output seq a client saw, like a real
// reconnecting client would.
var lastSeq uint64

func (b *tymuxAPI) Reattach(id string) (time.Duration, bool, error) {
	pane, err := b.paneID(id)
	if err != nil {
		return 0, false, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	stream := b.client.Attach(ctx)
	resume := lastSeq
	t := time.Now()
	if err := stream.Send(&v1.AttachRequest{Payload: &v1.AttachRequest_PaneId{PaneId: pane}, ResumeFromSeq: &resume}); err != nil {
		return 0, false, err
	}
	// Streaming has resumed at the first chunk, or at the snapshot that
	// follows a GapExceeded. Keep reading to the first chunk either way so the
	// next resume point is known.
	var d time.Duration
	gap := false
	for {
		ev, err := stream.Receive()
		if err != nil {
			return 0, false, err
		}
		switch p := ev.GetPayload().(type) {
		case *v1.AttachEvent_GapExceeded:
			gap = true
		case *v1.AttachEvent_Snapshot:
			if gap && d == 0 {
				d = time.Since(t)
			}
		case *v1.AttachEvent_OutputChunk:
			if d == 0 {
				d = time.Since(t)
			}
			seq := p.OutputChunk.GetSeq()
			contiguous := !gap && (resume == 0 || seq == resume+1)
			lastSeq = seq
			return d, contiguous, nil
		}
	}
}

func flattenPaneIDs(l *v1.Layout) []string {
	if l == nil {
		return nil
	}
	switch n := l.GetNode().(type) {
	case *v1.Layout_Pane:
		return []string{n.Pane.GetId()}
	case *v1.Layout_Split:
		var ids []string
		for _, c := range n.Split.GetChildren() {
			ids = append(ids, flattenPaneIDs(c.GetLayout())...)
		}
		return ids
	}
	return nil
}

// ---- the tymux CLI against the same kind of daemon ----

type tymuxCLI struct {
	tymuxAPI
	cli   string
	names sync.Map // session id -> name
}

func (b *tymuxCLI) Name() string     { return "tymux (CLI)" }
func (b *tymuxCLI) OwnsServer() bool { return false }

func (b *tymuxCLI) run(args ...string) ([]byte, error) {
	c := exec.Command(b.cli, args...)
	c.Env = append(os.Environ(), "TYMUXD_SOCKET_PATH="+b.sock)
	out, err := c.Output()
	if err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			return out, fmt.Errorf("%w: %s", err, ee.Stderr)
		}
	}
	return out, err
}

func (b *tymuxCLI) Create(i int) (string, error) {
	name := fmt.Sprintf("s%d", i)
	out, err := b.run("new", "-d", "--name", name, "--command", sessionCmd)
	if err != nil {
		return "", err
	}
	id := strings.TrimSpace(string(out))
	b.names.Store(id, name)
	return id, nil
}

func (b *tymuxCLI) Kill(id string) error { _, err := b.run("kill", id); return err }
func (b *tymuxCLI) List() error          { _, err := b.run("ls"); return err }

func (b *tymuxCLI) Capture(id string) (string, error) {
	name, ok := b.names.Load(id)
	if !ok {
		return "", fmt.Errorf("unknown session %s", id)
	}
	out, err := b.run("capture", name.(string))
	return string(out), err
}

// ---- tmux, one CLI process per command ----

type tmuxCLI struct {
	bin  string
	sock string
}

func (b *tmuxCLI) Name() string     { return "tmux (CLI)" }
func (b *tmuxCLI) OwnsServer() bool { return true }

func (b *tmuxCLI) tmux(args ...string) *exec.Cmd {
	c := exec.Command(b.bin, append([]string{"-S", b.sock, "-f", "/dev/null"}, args...)...)
	c.Env = append(os.Environ(), "TERM=xterm-256color")
	return c
}

func (b *tmuxCLI) Start(workdir string) error {
	b.sock = filepath.Join(workdir, "tmux.sock")
	// The first command starts the server; keeping this session alive stops
	// the server from exiting while there are no others.
	if out, err := b.tmux("new-session", "-d", "-s", "warm", sessionCmd).CombinedOutput(); err != nil {
		return fmt.Errorf("tmux first command: %w: %s", err, out)
	}
	return nil
}

func (b *tmuxCLI) Create(i int) (string, error) {
	name := fmt.Sprintf("s%d", i)
	if out, err := b.tmux("new-session", "-d", "-s", name, "-x", "80", "-y", "24", sessionCmd).CombinedOutput(); err != nil {
		return "", fmt.Errorf("%w: %s", err, out)
	}
	return name, nil
}

func (b *tmuxCLI) Kill(id string) error {
	out, err := b.tmux("kill-session", "-t", "="+id).CombinedOutput()
	if err != nil {
		return fmt.Errorf("%w: %s", err, out)
	}
	return nil
}

func (b *tmuxCLI) List() error { return b.tmux("list-sessions").Run() }

func (b *tmuxCLI) Capture(id string) (string, error) {
	out, err := b.tmux("capture-pane", "-p", "-t", "="+id+":").Output()
	return string(out), err
}

func (b *tmuxCLI) ServerPID() int {
	out, err := b.tmux("display-message", "-p", "#{pid}").Output()
	if err != nil {
		return 0
	}
	pid, _ := strconv.Atoi(strings.TrimSpace(string(out)))
	return pid
}

func (b *tmuxCLI) Stop() { _ = b.tmux("kill-server").Run() }

// ---- tmux over one persistent control-mode connection ----

// tmuxControl is the closest tmux analogue to a long-lived gRPC client: one
// connection, commands written as lines, replies framed by %begin/%end.
type tmuxControl struct {
	tmuxCLI
	ctl   *exec.Cmd
	stdin io.WriteCloser
	done  chan error
	outs  chan string // %output payload lines
}

func (b *tmuxControl) Name() string { return "tmux (control mode)" }

func (b *tmuxControl) Start(workdir string) error {
	if err := b.tmuxCLI.Start(workdir); err != nil {
		return err
	}
	return b.attach("warm")
}

// attach starts a control-mode client on `session` and waits for its first
// reply, so replies to later commands line up.
func (b *tmuxControl) attach(session string) error {
	b.ctl = b.tmux("-C", "attach", "-t", "="+session)
	in, err := b.ctl.StdinPipe()
	if err != nil {
		return err
	}
	out, err := b.ctl.StdoutPipe()
	if err != nil {
		return err
	}
	b.stdin = in
	b.done = make(chan error, 16)
	b.outs = make(chan string, 4096)
	if err := b.ctl.Start(); err != nil {
		return err
	}
	done, outs := b.done, b.outs
	go func() {
		sc := bufio.NewScanner(out)
		sc.Buffer(make([]byte, 1<<20), 1<<20)
		for sc.Scan() {
			line := sc.Text()
			switch {
			case strings.HasPrefix(line, "%end "):
				done <- nil
			case strings.HasPrefix(line, "%error "):
				done <- fmt.Errorf("tmux control: %s", line)
			case strings.HasPrefix(line, "%output "):
				select {
				case outs <- line:
				default:
				}
			}
		}
	}()
	// The attach itself produces a %begin/%end pair.
	select {
	case <-done:
		return nil
	case <-time.After(5 * time.Second):
		return fmt.Errorf("tmux control mode never answered")
	}
}

func (b *tmuxControl) run(cmd string) error {
	if _, err := io.WriteString(b.stdin, cmd+"\n"); err != nil {
		return err
	}
	select {
	case err := <-b.done:
		return err
	case <-time.After(10 * time.Second):
		return fmt.Errorf("tmux control: timeout on %q", cmd)
	}
}

func (b *tmuxControl) Create(i int) (string, error) {
	name := fmt.Sprintf("s%d", i)
	return name, b.run(fmt.Sprintf("new-session -d -s %s -x 80 -y 24 %s", name, sessionCmd))
}

func (b *tmuxControl) Kill(id string) error { return b.run("kill-session -t =" + id) }
func (b *tmuxControl) List() error          { return b.run("list-sessions") }
func (b *tmuxControl) Capture(id string) (string, error) {
	return "", b.run("capture-pane -p -t =" + id + ":")
}

func (b *tmuxControl) Stop() {
	if b.stdin != nil {
		_ = b.stdin.Close()
	}
	b.tmuxCLI.Stop()
	if b.ctl != nil {
		_ = b.ctl.Wait()
	}
}

func (b *tmuxControl) EchoSession(id string) (Echo, error) {
	// Control clients only receive %output for panes in the session they are
	// attached to.
	if err := b.run("switch-client -t =" + id); err != nil {
		return nil, err
	}
	return &ctlEcho{b: b, id: id}, nil
}

type ctlEcho struct {
	b  *tmuxControl
	id string
}

func (e *ctlEcho) RoundTrip(ch byte) (time.Duration, error) {
	for len(e.b.outs) > 0 {
		<-e.b.outs
	}
	t := time.Now()
	if _, err := fmt.Fprintf(e.b.stdin, "send-keys -t =%s: -l %c\n", e.id, ch); err != nil {
		return 0, err
	}
	gotEnd, gotEcho := false, false
	timeout := time.After(5 * time.Second)
	var d time.Duration
	for !(gotEnd && gotEcho) {
		select {
		case err := <-e.b.done:
			if err != nil {
				return 0, err
			}
			gotEnd = true
		case line := <-e.b.outs:
			if strings.IndexByte(line, ch) >= 0 && !gotEcho {
				gotEcho, d = true, time.Since(t)
			}
		case <-timeout:
			return 0, fmt.Errorf("tmux control: no echo of %q", ch)
		}
	}
	return d, nil
}

// Close re-attaches to the warm session: killing the session the control
// client is on would end the client.
func (e *ctlEcho) Close() { _ = e.b.run("switch-client -t =warm") }

func (b *tmuxControl) ReattachTarget() (string, error) {
	name := "chatty"
	return name, b.run(fmt.Sprintf("new-session -d -s %s -x 80 -y 24 sh -c %q", name, chattyScript))
}

// Reattach drops the control client and attaches a new one. tmux has no
// resume protocol, so contiguity can't be asserted and is reported false.
func (b *tmuxControl) Reattach(id string) (time.Duration, bool, error) {
	_ = b.stdin.Close()
	_ = b.ctl.Process.Kill()
	_ = b.ctl.Wait()
	t := time.Now()
	if err := b.attach(id); err != nil {
		return 0, false, err
	}
	return time.Since(t), false, nil
}

// ---- helpers ----

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func waitFor(cond func() bool, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if cond() {
			return nil
		}
		time.Sleep(time.Millisecond)
	}
	return fmt.Errorf("timed out after %s", timeout)
}

// rssKiB reads VmRSS from /proc (Linux only; 0 elsewhere).
func rssKiB(pid int) int {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return 0
	}
	for _, line := range strings.Split(string(data), "\n") {
		if rest, ok := strings.CutPrefix(line, "VmRSS:"); ok {
			f := strings.Fields(rest)
			if len(f) > 0 {
				n, _ := strconv.Atoi(f[0])
				return n
			}
		}
	}
	return 0
}
