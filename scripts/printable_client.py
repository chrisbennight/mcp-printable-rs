#!/usr/bin/env python3
"""A direct HTTP MCP client with verified, streamed artifact downloads."""

import argparse
import base64
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import tempfile
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

MAX_RESPONSE = 8 * 1024 * 1024
MAX_DOWNLOAD = 1024 * 1024 * 1024
CLIENT_INFO = {"name": "printable-direct", "version": "2"}


def validate_url(value):
    url = urlsplit(value)
    loopback = url.hostname == "localhost"
    try:
        loopback = loopback or ipaddress.ip_address(url.hostname).is_loopback
    except ValueError:
        pass
    if (not url.hostname or url.username is not None or url.password is not None
            or url.fragment or url.query
            or not (url.scheme == "https" or (url.scheme == "http" and loopback))):
        raise ValueError("Use HTTPS or loopback HTTP without credentials, query, or fragment")
    return value


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Redirect refused; configure the final service URL")


class Client:
    def __init__(self, url, bearer_file):
        self.url = validate_url(url)
        with open(bearer_file, "r", encoding="ascii") as stream:
            self.bearer = stream.read(67).rstrip("\r\n")
        if len(self.bearer) != 64 or any(c not in "0123456789abcdef" for c in self.bearer):
            raise ValueError("Invalid bearer file")
        self.session = None
        self.sequence = 0
        self.protocol = "2025-11-25"
        self.server_info = None
        self.http = build_opener(NoRedirect())

    def __enter__(self):
        result = self.rpc("initialize", {
            "protocolVersion": self.protocol, "capabilities": {},
            "clientInfo": CLIENT_INFO,
        })
        self.protocol = result["protocolVersion"]
        self.server_info = result["serverInfo"]
        self.rpc("notifications/initialized", notification=True)
        return self

    def __exit__(self, kind, value, trace):
        if self.session:
            try:
                with self.http.open(Request(self.url, headers=self.headers(), method="DELETE"), timeout=10):
                    pass
            except (HTTPError, URLError, TimeoutError, ValueError):
                # Session expiry also releases transport state after a lost connection.
                import sys
                print("Session cleanup was not confirmed.", file=sys.stderr)

    def headers(self):
        result = {"Authorization": "Bearer " + self.bearer,
                  "Accept": "application/json, text/event-stream",
                  "Content-Type": "application/json", "MCP-Protocol-Version": self.protocol}
        if self.session:
            result["Mcp-Session-Id"] = self.session
        return result

    def rpc(self, method, params=None, notification=False):
        self.sequence += 1
        request = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            request["params"] = params
        if not notification:
            request["id"] = self.sequence
        with self.http.open(Request(self.url, data=json.dumps(request).encode(),
                                    headers=self.headers()), timeout=180) as response:
            if response.headers.get("Mcp-Session-Id"):
                self.session = response.headers["Mcp-Session-Id"]
            if notification:
                return None
            if response.headers.get_content_type() == "text/event-stream":
                used = 0
                data = []
                for _ in range(MAX_RESPONSE):
                    line = response.readline(MAX_RESPONSE - used + 1)
                    used += len(line)
                    if used > MAX_RESPONSE:
                        raise ValueError("MCP response exceeds client limit")
                    if not line:
                        break
                    if line in (b"\n", b"\r\n") and data:
                        payload = b"\n".join(data)
                        data = []
                        if not payload.strip():
                            continue
                        message = json.loads(payload)
                        if message.get("id") == self.sequence:
                            return self.result(message)
                    elif line.startswith(b"data:"):
                        data.append(line[5:].strip())
                raise ValueError("MCP response ended without the requested result")
            body = response.read(MAX_RESPONSE + 1)
            if len(body) > MAX_RESPONSE:
                raise ValueError("MCP response exceeds client limit")
            message = json.loads(body)
            if message.get("id") != self.sequence:
                raise ValueError("MCP response identifier mismatch")
            return self.result(message)

    @staticmethod
    def result(message):
        if "error" in message:
            raise ValueError("MCP request failed; inspect service status before retrying mutations")
        return message["result"]

    def call(self, name, arguments):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise ValueError("Tool failed; inspect service status before retrying mutations")
        if "structuredContent" in result:
            return result["structuredContent"]
        texts = [item["text"] for item in result.get("content", []) if item.get("type") == "text"]
        if len(texts) != 1:
            raise ValueError("Expected one JSON text result when structuredContent is absent")
        return json.loads(texts[0])

    def contracts(self, tool=None, action=None):
        if action is not None and tool is None:
            raise ValueError("An action contract requires a tool name")
        segments = [value for value in (tool, action) if value is not None]
        if any(not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9_]*", value)
               for value in segments):
            raise ValueError("Invalid contract tool or action name")
        uri = "printable://contracts" + "".join("/" + value for value in segments)
        result = self.rpc("resources/read", {"uri": uri})
        contents = result.get("contents", [])
        if len(contents) != 1 or contents[0].get("uri") != uri or "text" not in contents[0]:
            raise ValueError("Unexpected contract resource")
        return json.loads(contents[0]["text"])

    def tools(self):
        """Full standards-compatible discovery, including every returned page."""
        items = []
        cursor = None
        seen = set()
        while True:
            page = self.rpc("tools/list", {"cursor": cursor} if cursor else {})
            items.extend(page["tools"])
            cursor = page.get("nextCursor")
            if cursor is None:
                return {"tools": items}
            if not isinstance(cursor, str) or not cursor or cursor in seen:
                raise ValueError("Invalid or repeated discovery cursor")
            seen.add(cursor)
            if len(seen) >= 100:
                raise ValueError("Tool discovery exceeds the client page limit")

    def download(self, path, destination):
        destination = Path(destination)
        if destination.exists() or destination.is_symlink():
            raise FileExistsError("Download destination already exists")
        if not destination.parent.is_dir():
            raise FileNotFoundError("Download destination directory does not exist")
        published = self.call("artifact", {"action": "publish", "params": {"path": path}})["file"]
        descriptor = self.rpc("files/authorizeDownload", {
            "uri": published["uri"], "_meta": {
                "io.modelcontextprotocol/clientCapabilities": {
                    "files": {"download": True, "transports": ["http"]}}}})
        download = descriptor["download"]
        if descriptor["file"] != published or download["method"] != "GET" or download["transport"] != "http":
            raise ValueError("Unexpected download descriptor")
        if published["digest"]["algorithm"] != "sha-256":
            raise ValueError("Unsupported artifact digest")
        size = published["size"]
        if not isinstance(size, int) or not 0 <= size <= MAX_DOWNLOAD:
            raise ValueError("Artifact exceeds the 1 GiB client limit")
        url = validate_url(download["url"])
        authority = download["headers"]["Authorization"]
        digest = hashlib.sha256()
        received = 0
        with tempfile.NamedTemporaryFile(dir=destination.parent) as output:
            with self.http.open(Request(url, headers={"Authorization": authority}), timeout=180) as response:
                while chunk := response.read(min(65536, size - received + 1)):
                    received += len(chunk)
                    if received > size:
                        raise ValueError("Download exceeds declared size")
                    digest.update(chunk)
                    output.write(chunk)
            actual = base64.urlsafe_b64encode(digest.digest()).decode().rstrip("=")
            if received != size or actual != published["digest"]["value"]:
                raise ValueError("Artifact size or digest mismatch")
            output.flush()
            os.fsync(output.fileno())
            os.link(output.name, destination)
        return {"path": str(destination), "bytes": received, "sha256": digest.hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8000/mcp")
    parser.add_argument("--bearer-file", type=Path, default=Path(".dev/mcp-bearer"))
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("status")
    commands.add_parser("tools", help="read the complete MCP tool catalog")
    contracts = commands.add_parser("contracts", help="read the contract index or one selected contract")
    contracts.add_argument("tool", nargs="?")
    contracts.add_argument("action", nargs="?")
    call = commands.add_parser("call")
    call.add_argument("tool")
    call.add_argument("arguments", type=Path, help="JSON file containing tool arguments")
    download = commands.add_parser("download")
    download.add_argument("artifact")
    download.add_argument("destination", type=Path)
    args = parser.parse_args()
    try:
        with Client(args.url, args.bearer_file) as client:
            if args.command == "status":
                result = client.call("status", {"detail": True})
            elif args.command == "tools":
                result = client.tools()
            elif args.command == "contracts":
                result = client.contracts(args.tool, args.action)
            elif args.command == "call":
                result = client.call(args.tool, json.loads(args.arguments.read_text()))
            else:
                result = client.download(args.artifact, args.destination)
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, KeyError):
        raise SystemExit("Request failed. Check configuration and status; mutations are never automatically retried.") from None


if __name__ == "__main__":
    main()
