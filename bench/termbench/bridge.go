package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/coder/websocket"
)

// ack is one chunk the page has written and seen painted. parse and paint are
// the page's own clock (write -> parsed, parsed -> next animation frame).
type ack struct {
	pane       int
	seq        uint32
	parse, pnt float64
	at         time.Time // when Go received the ack
}

type ready struct {
	Scale float64 `json:"scale"`
	DPR   float64 `json:"dpr"`
	UA    string  `json:"ua"`
}

// Bridge serves the harness page to headless Chrome and relays pty output to
// it over a WebSocket; acks come back the same way.
type Bridge struct {
	ln     net.Listener
	chrome *exec.Cmd
	conn   *websocket.Conn

	hello chan struct{}
	ready chan ready

	mu    sync.Mutex
	onAck func([]ack)
	// lost is the page's WebGL context-loss count, as of the latest ack.
	lost atomic.Int64
}

func newBridge(webDir string) (*Bridge, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	b := &Bridge{ln: ln, hello: make(chan struct{}, 1), ready: make(chan ready, 1)}
	mux := http.NewServeMux()
	mux.Handle("/", http.FileServer(http.Dir(webDir)))
	// ghostty-web's init() fetches its wasm from the origin root.
	mux.HandleFunc("/ghostty-vt.wasm", func(w http.ResponseWriter, r *http.Request) {
		http.ServeFile(w, r, filepath.Join(webDir, "node_modules/ghostty-web/ghostty-vt.wasm"))
	})
	mux.HandleFunc("/ws", b.serveWS)
	go http.Serve(ln, mux)
	return b, nil
}

func (b *Bridge) serveWS(w http.ResponseWriter, r *http.Request) {
	c, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	c.SetReadLimit(64 << 20)
	b.mu.Lock()
	b.conn = c
	b.mu.Unlock()
	for {
		_, data, err := c.Read(context.Background())
		if err != nil {
			return
		}
		at := time.Now()
		var m struct {
			T string `json:"t"`
			ready
			A    [][4]float64 `json:"a"`
			Lost int64        `json:"lost"`
		}
		if json.Unmarshal(data, &m) != nil {
			continue
		}
		switch m.T {
		case "hello":
			select {
			case b.hello <- struct{}{}:
			default:
			}
		case "ready":
			select {
			case b.ready <- m.ready:
			default:
			}
		case "ack":
			b.lost.Store(m.Lost)
			acks := make([]ack, len(m.A))
			for i, a := range m.A {
				acks[i] = ack{pane: int(a[0]), seq: uint32(a[1]), parse: a[2], pnt: a[3], at: at}
			}
			b.mu.Lock()
			f := b.onAck
			b.mu.Unlock()
			if f != nil {
				f(acks)
			}
		}
	}
}

func (b *Bridge) setAckHandler(f func([]ack)) {
	b.mu.Lock()
	b.onAck = f
	b.mu.Unlock()
}

// Launch starts headless Chrome on the harness page and waits for it to connect.
func (b *Bridge) Launch(chrome, window string, uncap bool, extra []string) error {
	profile, err := os.MkdirTemp("", "tb-chrome-")
	if err != nil {
		return err
	}
	args := []string{
		"--headless=new", "--user-data-dir=" + profile, "--window-size=" + window,
		"--no-first-run", "--no-default-browser-check", "--hide-scrollbars", "--mute-audio",
		// A benchmark page is never in the foreground; keep Chrome from throttling it.
		"--disable-background-timer-throttling", "--disable-renderer-backgrounding",
		"--disable-backgrounding-occluded-windows",
		// Software GL so the WebGL renderer runs without a GPU.
		"--use-angle=swiftshader", "--enable-unsafe-swiftshader",
	}
	if uncap {
		args = append(args, "--disable-frame-rate-limit", "--disable-gpu-vsync")
	}
	args = append(args, extra...)
	args = append(args, "http://"+b.ln.Addr().String()+"/")
	b.chrome = exec.Command(chrome, args...)
	b.chrome.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := b.chrome.Start(); err != nil {
		return fmt.Errorf("start chrome: %w", err)
	}
	select {
	case <-b.hello:
		return nil
	case <-time.After(30 * time.Second):
		return fmt.Errorf("chrome did not connect to the bridge within 30s")
	}
}

// Init builds the page's terminals and returns once they have rendered.
func (b *Bridge) Init(engine string, panes int) (ready, error) {
	b.lost.Store(0)
	select {
	case <-b.ready:
	default:
	}
	msg, _ := json.Marshal(map[string]any{"t": "init", "engine": engine, "panes": panes, "cols": 80, "rows": 24})
	if err := b.conn.Write(context.Background(), websocket.MessageText, msg); err != nil {
		return ready{}, err
	}
	select {
	case r := <-b.ready:
		return r, nil
	case <-time.After(60 * time.Second):
		return ready{}, fmt.Errorf("engine %s: page did not report ready (check chrome and web/node_modules)", engine)
	}
}

// Send writes data to a pane's terminal; seq comes back in the ack.
func (b *Bridge) Send(pane int, seq uint32, data []byte) error {
	frame := make([]byte, 8+len(data))
	binary.LittleEndian.PutUint16(frame[0:], uint16(pane))
	binary.LittleEndian.PutUint32(frame[4:], seq)
	copy(frame[8:], data)
	return b.conn.Write(context.Background(), websocket.MessageBinary, frame)
}

func (b *Bridge) Close() {
	if b.conn != nil {
		b.conn.CloseNow()
	}
	if b.chrome != nil && b.chrome.Process != nil {
		_ = syscall.Kill(-b.chrome.Process.Pid, syscall.SIGKILL)
		_ = b.chrome.Wait()
	}
	b.ln.Close()
}
