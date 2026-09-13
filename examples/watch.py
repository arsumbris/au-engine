#!/usr/bin/env python3
"""Watch a repo's live subscription stream from outside Rust.

A reference external consumer of the daemon wire, the position an editor
extension or other outside client is in. It reimplements the framing by hand,
so it exercises the contract as a real outsider would, not through the engine's
own client.

Usage:
    au daemon start <repo>            # in another terminal
    python3 examples/watch.py <repo> [channel] [file]

    channel defaults to "changes". For "diagnostics" pass the file to watch:
    python3 examples/watch.py <repo> diagnostics notes/a.md

Edit a file in the repo and the change events print live.
"""

import json
import socket
import struct
import sys
from pathlib import Path

# The wire: each frame is a 4-byte big-endian length prefix, then that many
# bytes of JSON. One endpoint per entry point, at a short hashed path outside the
# repo, ~/.arsumbris/au-engine/run/<hash>.sock (see socket_path).
LENGTH_PREFIX = struct.Struct(">I")


def socket_path(entry: str) -> Path:
    # Match au_engine::socket_path: the socket lives outside the repo, at
    # ~/.arsumbris/au-engine/run/<hash>.sock, so a deep repo path never overruns the
    # Unix sun_path limit. <hash> is the FNV-1a-64 of the absolute entry path's
    # bytes, in 16 hex digits. Resolve symlinks first (realpath) so this hashes
    # the same string the daemon does (on macOS /var -> /private/var).
    abs_entry = str(Path(entry).resolve())
    h = 0xCBF29CE484222325
    for b in abs_entry.encode("utf-8"):
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return Path.home() / ".arsumbris" / "au-engine" / "run" / f"{h:016x}.sock"


def send_frame(sock: socket.socket, message: dict) -> None:
    payload = json.dumps(message).encode("utf-8")
    sock.sendall(LENGTH_PREFIX.pack(len(payload)) + payload)


def recv_exact(sock: socket.socket, n: int) -> bytes | None:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None  # clean EOF, the daemon closed the connection
        buf.extend(chunk)
    return bytes(buf)


def recv_frame(sock: socket.socket) -> dict | None:
    header = recv_exact(sock, LENGTH_PREFIX.size)
    if header is None:
        return None
    (length,) = LENGTH_PREFIX.unpack(header)
    body = recv_exact(sock, length)
    if body is None:
        return None
    return json.loads(body)


def print_frame(frame: dict) -> None:
    kind = frame.get("type", "?")
    if kind == "ack":
        ok = "accepted" if frame.get("accepted") else "rejected"
        print(f"[ack]      #{frame.get('subscription_id')} {frame.get('channel')} {ok}")
    elif kind == "initial_value":
        print(f"[initial]  @v{frame.get('at_version')} {json.dumps(frame.get('result'))}")
    elif kind == "change_event":
        hint = frame.get("scope_hint", {})
        scope = hint.get("scope")
        if scope in ("files", "types"):
            # The delta axes the channel computed: files/added/removed/...
            parts = [f"{k}={v}" for k, v in hint.items() if k != "scope" and v]
            detail = " ".join(parts) if parts else f"scope={scope}"
        else:
            detail = f"scope={scope}"
        print(f"[change]   @v{frame.get('at_version')} {frame.get('kind')} ({detail})")
    elif kind == "error":
        print(f"[error]    {frame.get('error')}")
    else:
        print(f"[{kind}]   {json.dumps(frame)}")


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    # Line-buffer stdout so events appear live even when piped to a file.
    sys.stdout.reconfigure(line_buffering=True)
    repo = sys.argv[1]
    channel = sys.argv[2] if len(sys.argv) > 2 else "changes"

    request: dict = {"subscribe": channel}
    # diagnostics is whole-knowledge-base with no file, or one file when given.
    if channel == "diagnostics" and len(sys.argv) > 3:
        request["path"] = sys.argv[3]

    path = socket_path(repo)
    if not path.exists():
        print(f"no daemon socket at {path}; start one with: au daemon start {repo}", file=sys.stderr)
        return 1

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(path))
    send_frame(sock, request)
    print(f"subscribed to {channel}; watching {path}", file=sys.stderr)

    try:
        while True:
            frame = recv_frame(sock)
            if frame is None:
                print("daemon closed the connection", file=sys.stderr)
                return 0
            print_frame(frame)
    except KeyboardInterrupt:
        return 0
    finally:
        sock.close()


if __name__ == "__main__":
    sys.exit(main())
