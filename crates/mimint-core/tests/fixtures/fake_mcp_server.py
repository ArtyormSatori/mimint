#!/usr/bin/env python3
"""Fake MCP server for mimint-core integration tests.

Stdlib only, NDJSON over stdio. One file, behaviour selected
by argv[1] plus environment knobs:

Modes (argv[1], default "echo"):
  echo          full server: initialize + tools/list + tools/call
  hang          answers initialize/list, then never answers calls
  garbage       answers initialize/list, then emits a non-JSON line
                on the first call
  huge          answers initialize, then emits a >256 KiB frame
                for tools/list
  exit-mid-call answers initialize/list, then exits on the first call
  exit-early    exits immediately, before any handshake
  batch         answers tools/list with a JSON array (batching)
  wrong-id      answers tools/list with a mismatched id
  stateless     answers initialize with a JSON-RPC error
  sampling      sends sampling/createMessage after initialize and
                logs the client's response to stderr as RESPONSE:<json>
  roots         same, with roots/list
  ping          same, with ping (expects result {})
  list-changed  sends notifications/tools/list_changed after
                initialize, then behaves like echo
  stderr-flood  writes 1 MiB to stderr at startup, then echo
  env-dump      echo + a dump_env tool returning os.environ
  image         echo + a shot tool returning image content
  big-output    echo + a flood tool returning 100 KiB of text

Environment:
  FAKE_MCP_VERSION   protocolVersion in the initialize result
                     (default 2025-06-18)
  FAKE_MCP_TOOLS     JSON list of tool dicts (default echo+fail)
  FAKE_MCP_PAGE_SIZE tools/list page size, 0 = no pagination
  FAKE_MCP_TRACE=1   write SAW:<method> to stderr for every
                     inbound request/notification

The server is designed to run inside the bwrap sandbox: it
needs nothing outside the read-only prefixes and its own
script file (bound read-only by the client).
"""

import json
import os
import sys
import time


def trace(message):
    if os.environ.get("FAKE_MCP_TRACE") == "1":
        sys.stderr.write(message + "\n")
        sys.stderr.flush()


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def default_tools():
    return [
        {
            "name": "echo",
            "description": "Echoes text.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            },
        },
        {
            "name": "fail",
            "description": "Always errors.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ]


def answer_list(mid, params, tools, page_size):
    """Answer one tools/list request, honouring pagination."""
    cursor = (params or {}).get("cursor")
    start = int(cursor.split("-")[1]) if cursor else 0
    if page_size > 0:
        page = tools[start : start + page_size]
        result = {"tools": page}
        if start + page_size < len(tools):
            result["nextCursor"] = "page-%d" % (start + page_size)
    else:
        result = {"tools": tools}
    send({"jsonrpc": "2.0", "id": mid, "result": result})


def await_response(tools, page_size):
    """Read lines until the client's response to our server
    request arrives, answering any tools/list request that
    races ahead of it (the client sends tools/list right
    after notifications/initialized, possibly before our
    request is answered). The response line is logged to
    stderr as RESPONSE:<json> for the test to inspect."""
    while True:
        line = sys.stdin.readline()
        if not line:
            return
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        method = msg.get("method")
        if method == "tools/list":
            trace("SAW:tools/list")
            answer_list(msg.get("id"), msg.get("params"), tools, page_size)
            continue
        if method is not None:
            trace("SAW:" + method)
            continue
        sys.stderr.write("RESPONSE:" + line + "\n")
        sys.stderr.flush()
        return


def handle_call(mode, name, args):
    if name == "echo":
        return {
            "content": [
                {
                    "type": "text",
                    "text": "echo: " + json.dumps(args, sort_keys=True),
                }
            ]
        }
    if name == "fail":
        return {
            "content": [{"type": "text", "text": "deliberate failure"}],
            "isError": True,
        }
    if name == "dump_env":
        return {
            "content": [
                {
                    "type": "text",
                    "text": json.dumps(dict(os.environ), sort_keys=True),
                }
            ]
        }
    if name == "shot":
        return {
            "content": [
                {
                    "type": "image",
                    "data": "aGVsbG8=",
                    "mimeType": "image/png",
                }
            ]
        }
    if name == "flood":
        # 100 KiB: over the 64 KiB output cap, under the
        # 256 KiB frame cap, so the output cap is what
        # fires (a 1 MiB single frame would die at the
        # frame cap first, by design).
        return {"content": [{"type": "text", "text": "y" * (100 * 1024)}]}
    return {
        "content": [{"type": "text", "text": "unknown tool"}],
        "isError": True,
    }


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "echo"
    if mode == "exit-early":
        sys.exit(0)
    if mode == "stderr-flood":
        chunk = b"x" * 65536
        for _ in range(16):
            sys.stderr.buffer.write(chunk)
        sys.stderr.buffer.flush()

    raw_tools = os.environ.get("FAKE_MCP_TOOLS")
    tools = json.loads(raw_tools) if raw_tools else default_tools()
    if mode == "env-dump":
        tools.append(
            {
                "name": "dump_env",
                "description": "Dumps the environment.",
                "inputSchema": {"type": "object", "properties": {}},
            }
        )
    if mode == "image":
        tools.append(
            {
                "name": "shot",
                "description": "Returns an image.",
                "inputSchema": {"type": "object", "properties": {}},
            }
        )
    if mode == "big-output":
        tools.append(
            {
                "name": "flood",
                "description": "Returns 100 KiB of text.",
                "inputSchema": {"type": "object", "properties": {}},
            }
        )
    page_size = int(os.environ.get("FAKE_MCP_PAGE_SIZE", "0"))
    version = os.environ.get("FAKE_MCP_VERSION", "2025-06-18")

    while True:
        line = sys.stdin.readline()
        if not line:
            return
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        method = msg.get("method")
        mid = msg.get("id")
        if method is not None:
            trace("SAW:" + method)
        if method == "initialize":
            if mode == "stateless":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "error": {"code": -32603, "message": "nope"},
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "protocolVersion": version,
                            "capabilities": {},
                            "serverInfo": {"name": "fake", "version": "1.0"},
                        },
                    }
                )
        elif method == "notifications/initialized":
            if mode in ("sampling", "roots", "ping"):
                wanted = {
                    "sampling": "sampling/createMessage",
                    "roots": "roots/list",
                    "ping": "ping",
                }[mode]
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": "srv-1",
                        "method": wanted,
                        "params": {},
                    }
                )
                await_response(tools, page_size)
            elif mode == "list-changed":
                send(
                    {
                        "jsonrpc": "2.0",
                        "method": "notifications/tools/list_changed",
                    }
                )
        elif method == "tools/list":
            if mode == "batch":
                # A JSON array frame: batching, which the
                # client must reject as a protocol error.
                sys.stdout.write(
                    json.dumps(
                        [
                            {
                                "jsonrpc": "2.0",
                                "id": mid,
                                "result": {"tools": tools},
                            }
                        ]
                    )
                    + "\n"
                )
                sys.stdout.flush()
            elif mode == "wrong-id":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": 9999,
                        "result": {"tools": tools},
                    }
                )
            elif mode == "huge":
                sys.stdout.write("x" * (300 * 1024) + "\n")
                sys.stdout.flush()
            else:
                params = msg.get("params") or {}
                cursor = params.get("cursor")
                start = int(cursor.split("-")[1]) if cursor else 0
                if page_size > 0:
                    page = tools[start : start + page_size]
                    result = {"tools": page}
                    if start + page_size < len(tools):
                        result["nextCursor"] = "page-%d" % (start + page_size)
                else:
                    result = {"tools": tools}
                send({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "tools/call":
            if mode == "hang":
                time.sleep(3600)
            elif mode == "exit-mid-call":
                sys.exit(0)
            elif mode == "garbage":
                sys.stdout.write("this is not json\n")
                sys.stdout.flush()
            else:
                params = msg.get("params") or {}
                result = handle_call(
                    mode, params.get("name"), params.get("arguments") or {}
                )
                send({"jsonrpc": "2.0", "id": mid, "result": result})
        # Anything else (including notifications/cancelled)
        # is ignored: the client must never depend on a
        # server acknowledging a notification.


if __name__ == "__main__":
    main()
