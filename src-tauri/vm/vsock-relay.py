# OpenApp guest relay for one vsock route (src-tauri/src/vm.rs): guest 127.0.0.1:<port> → vsock 2:<port> → the
# kernel. Replaces `socat TCP-LISTEN VSOCK-CONNECT`, which fails on uploads larger than 256 KiB to a server that
# answers only after the whole body (an API request with a long chat):
# - the guest kernel caps its send window at its own 256 KiB vsock buffer, but libkrun (msb_krun_devices 0.1.41)
#   returns credit only every 4 MiB, or with a packet it sends: here the kernel acknowledges every 64 KiB;
# - libkrun reads one data descriptor per guest packet, and the guest kernel sends writes above ~32 KiB as
#   multi-descriptor packets, which are lost: here every write is at most 16 KiB.
#
# Frames, both directions: [type u8][len u32 big-endian][payload]. 0 = data, 1 = end of this direction, 2 = ack
# (kernel → guest only, no payload). The guest closes the vsock connection first, after both ends have sent "end".
import socket
import struct
import sys
import threading

PORT = int(sys.argv[1])
DATA, END, ACK = 0, 1, 2
CHUNK = 16384
HEAD = struct.Struct(">BI")


def recv_exact(s, n):
    buf = bytearray()
    while len(buf) < n:
        b = s.recv(n - len(buf))
        if not b:
            return None
        buf += b
    return bytes(buf)


def handle(c):
    v = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    try:
        v.connect((2, PORT))
    except OSError:
        c.close()
        v.close()
        return
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    lock = threading.Lock()
    ended = {"up": False, "down": False}

    def finish(side):
        with lock:
            ended[side] = True
            both = ended["up"] and ended["down"]
        if both:
            try:
                v.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def up():
        try:
            while True:
                b = c.recv(CHUNK)
                if not b:
                    break
                v.sendall(HEAD.pack(DATA, len(b)) + b)
        except OSError:
            pass
        try:
            v.sendall(HEAD.pack(END, 0))
        except OSError:
            pass
        finish("up")

    t = threading.Thread(target=up, daemon=True)
    t.start()
    try:
        while True:
            h = recv_exact(v, HEAD.size)
            if h is None:
                break
            kind, n = HEAD.unpack(h)
            if kind == DATA:
                p = recv_exact(v, n)
                if p is None:
                    break
                c.sendall(p)
            elif kind == END:
                try:
                    c.shutdown(socket.SHUT_WR)
                except OSError:
                    pass
                finish("down")
    except OSError:
        pass
    v.close()
    try:
        c.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    t.join(5)
    c.close()


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", PORT))
    srv.listen(256)
    while True:
        c, _ = srv.accept()
        threading.Thread(target=handle, args=(c,), daemon=True).start()


main()
