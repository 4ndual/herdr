package main

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Client talks to the fleet daemon. Localhost only: the daemon binds every
// interface, but this plugin has no business reaching another machine's fleet,
// so the base URL is pinned rather than configurable to an arbitrary host.
type Client struct {
	base string
	http *http.Client
}

func NewClient(base string) *Client {
	return &Client{
		base: strings.TrimSuffix(base, "/"),
		// A per-request timeout, not a global one: a hung daemon must not wedge
		// the collector, and the WebSocket does not go through this client.
		http: &http.Client{Timeout: 8 * time.Second},
	}
}

func (c *Client) getJSON(ctx context.Context, path string, out any) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.base+path, nil)
	if err != nil {
		return err
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode != http.StatusOK {
		// Drain so the connection can be reused rather than dropped.
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4<<10))
		return fmt.Errorf("%s: http %d", path, resp.StatusCode)
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

func (c *Client) postJSON(ctx context.Context, path string, body any) (string, error) {
	var payload io.Reader
	if body != nil {
		buf, err := json.Marshal(body)
		if err != nil {
			return "", err
		}
		payload = strings.NewReader(string(buf))
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.base+path, payload)
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := c.http.Do(req)
	if err != nil {
		return "", err
	}
	defer func() { _ = resp.Body.Close() }()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<10))
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("http %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	return string(raw), nil
}

func (c *Client) Fleet(ctx context.Context) ([]Harness, error) {
	var out []Harness
	return out, c.getJSON(ctx, "/api/fleet", &out)
}

func (c *Client) Tree(ctx context.Context) ([]TreeNode, error) {
	var out struct {
		Nodes []TreeNode `json:"nodes"`
	}
	if err := c.getJSON(ctx, "/api/orchestrator/livetree", &out); err != nil {
		return nil, err
	}
	return out.Nodes, nil
}

func (c *Client) Templates(ctx context.Context) ([]Template, error) {
	var out []Template
	return out, c.getJSON(ctx, "/api/templates", &out)
}

func (c *Client) Models(ctx context.Context) (json.RawMessage, error) {
	var out json.RawMessage
	return out, c.getJSON(ctx, "/api/models", &out)
}

func (c *Client) Exposure(ctx context.Context) (json.RawMessage, error) {
	var out json.RawMessage
	return out, c.getJSON(ctx, "/api/orchestrator/exposure", &out)
}

func (c *Client) Detail(ctx context.Context, name string) (*Detail, error) {
	if name == "" {
		return nil, errors.New("no harness selected")
	}
	var out Detail
	if err := c.getJSON(ctx, "/api/harness/"+url.PathEscape(name), &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Mutate performs one fleet mutation. `spawn`, `stop` and `refresh` are the
// minimum the panel must be able to do; the route accepts any action the daemon
// switch knows, so the set grows without a code change here.
func (c *Client) Mutate(ctx context.Context, name, action string, body any) (string, error) {
	if name == "" || action == "" {
		return "", errors.New("mutate needs a harness and an action")
	}
	return c.postJSON(ctx, "/api/harness/"+url.PathEscape(name)+"/"+url.PathEscape(action), body)
}

// ── WebSocket ───────────────────────────────────────────────────────────────
//
// A minimal RFC 6455 client rather than a dependency: this speaks to one
// endpoint on loopback, reads text frames and never sends anything but a close.
// Pulling in a websocket library for that is more supply chain than the job
// needs, and the framing below is the whole protocol we use.

type wsConn struct {
	conn net.Conn
	br   *bufio.Reader
}

func dialWS(ctx context.Context, base string) (*wsConn, error) {
	u, err := url.Parse(base)
	if err != nil {
		return nil, err
	}
	host := u.Host
	if !strings.Contains(host, ":") {
		host += ":80"
	}
	var d net.Dialer
	conn, err := d.DialContext(ctx, "tcp", host)
	if err != nil {
		return nil, err
	}
	key := make([]byte, 16)
	if _, err := rand.Read(key); err != nil {
		_ = conn.Close()
		return nil, err
	}
	handshake := "GET /ws HTTP/1.1\r\n" +
		"Host: " + host + "\r\n" +
		"Upgrade: websocket\r\n" +
		"Connection: Upgrade\r\n" +
		"Sec-WebSocket-Version: 13\r\n" +
		"Sec-WebSocket-Key: " + base64.StdEncoding.EncodeToString(key) + "\r\n\r\n"
	if err := conn.SetWriteDeadline(time.Now().Add(5 * time.Second)); err != nil {
		_ = conn.Close()
		return nil, err
	}
	if _, err := io.WriteString(conn, handshake); err != nil {
		_ = conn.Close()
		return nil, err
	}
	br := bufio.NewReader(conn)
	if err := conn.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
		_ = conn.Close()
		return nil, err
	}
	status, err := br.ReadString('\n')
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	if !strings.Contains(status, "101") {
		_ = conn.Close()
		return nil, fmt.Errorf("websocket upgrade refused: %s", strings.TrimSpace(status))
	}
	for {
		line, err := br.ReadString('\n')
		if err != nil {
			_ = conn.Close()
			return nil, err
		}
		if strings.TrimSpace(line) == "" {
			break
		}
	}
	return &wsConn{conn: conn, br: br}, nil
}

func (w *wsConn) Close() error { return w.conn.Close() }

// ReadMessage returns the next text/binary payload, transparently answering
// pings and skipping control frames. `deadline` bounds one read so a silent
// daemon cannot park this goroutine forever.
func (w *wsConn) ReadMessage(deadline time.Duration) ([]byte, error) {
	for {
		if err := w.conn.SetReadDeadline(time.Now().Add(deadline)); err != nil {
			return nil, err
		}
		var head [2]byte
		if _, err := io.ReadFull(w.br, head[:]); err != nil {
			return nil, err
		}
		final := head[0]&0x80 != 0
		opcode := head[0] & 0x0f
		masked := head[1]&0x80 != 0
		size := int64(head[1] & 0x7f)
		switch size {
		case 126:
			var ext [2]byte
			if _, err := io.ReadFull(w.br, ext[:]); err != nil {
				return nil, err
			}
			size = int64(binary.BigEndian.Uint16(ext[:]))
		case 127:
			var ext [8]byte
			if _, err := io.ReadFull(w.br, ext[:]); err != nil {
				return nil, err
			}
			size = int64(binary.BigEndian.Uint64(ext[:]))
		}
		// A server frame must not be masked, and an oversized frame is a bug or
		// an attack; either way refuse rather than allocate.
		if masked || size < 0 || size > maxFrameBytes {
			return nil, fmt.Errorf("refusing websocket frame (masked=%v size=%d)", masked, size)
		}
		payload := make([]byte, size)
		if _, err := io.ReadFull(w.br, payload); err != nil {
			return nil, err
		}
		switch opcode {
		case 0x8: // close
			return nil, io.EOF
		case 0x9: // ping -> pong, masked as a client must
			if err := w.writeFrame(0xA, payload); err != nil {
				return nil, err
			}
		case 0xA: // pong
		default:
			if !final {
				// The daemon sends whole JSON documents; a fragmented frame is
				// not something this reader claims to support.
				return nil, errors.New("fragmented websocket frame")
			}
			return payload, nil
		}
	}
}

func (w *wsConn) writeFrame(opcode byte, payload []byte) error {
	if err := w.conn.SetWriteDeadline(time.Now().Add(5 * time.Second)); err != nil {
		return err
	}
	var mask [4]byte
	if _, err := rand.Read(mask[:]); err != nil {
		return err
	}
	header := []byte{0x80 | opcode}
	switch {
	case len(payload) < 126:
		header = append(header, byte(0x80|len(payload)))
	case len(payload) <= 0xffff:
		header = append(header, 0x80|126, 0, 0)
		binary.BigEndian.PutUint16(header[2:], uint16(len(payload)))
	default:
		header = append(header, 0x80|127, 0, 0, 0, 0, 0, 0, 0, 0)
		binary.BigEndian.PutUint64(header[2:], uint64(len(payload)))
	}
	header = append(header, mask[:]...)
	masked := make([]byte, len(payload))
	for i, b := range payload {
		masked[i] = b ^ mask[i%4]
	}
	if _, err := w.conn.Write(append(header, masked...)); err != nil {
		return err
	}
	return nil
}

// maxFrameBytes caps one daemon frame. The fleet payload is ~12 KB today; 8 MB
// is generous and still bounded.
const maxFrameBytes = 8 << 20
