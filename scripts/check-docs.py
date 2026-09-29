#!/usr/bin/env python3
"""Check built book links and run explicitly selected documentation examples."""
import argparse
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import tempfile
import time
from urllib.parse import unquote, urlsplit


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.ids = set()
        self.links = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag == "a" and "name" in attrs:
            self.ids.add(attrs["name"])
        for key in ("href", "src"):
            if key in attrs:
                self.links.append(attrs[key])


def check_links(book):
    pages = {p.resolve(): Page(p.read_text()) for p in book.rglob("*.html")}
    assert pages, f"no HTML pages in {book}; run mdbook build docs first"
    errors = []
    for path, page in pages.items():
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            base = book.resolve() if url.path.startswith("/") else path.parent
            target = (base / unquote(url.path).lstrip("/")).resolve() if url.path else path
            if target.is_dir():
                target /= "index.html"
            if not target.exists():
                errors.append(f"{path.name}: missing target {link}")
            elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
                errors.append(f"{path.name}: missing anchor {link}")
    assert not errors, "\n".join(errors)
    print(f"checked local links and anchors in {len(pages)} HTML pages")


def blocks(source, filename, language):
    text = (source / filename).read_text()
    return re.findall(r"^```" + language + r"\n(.*?)^```", text, re.M | re.S)


def check_examples(source, binary):
    # Only the two runnable walkthroughs and the explicit overlay invocation
    # below are selected. Never execute arbitrary shell blocks from the book.
    with tempfile.TemporaryDirectory(prefix="dc-docs-", dir="/tmp") as directory:
        root = Path(directory)
        env = os.environ.copy()
        for variable in ("HOME", "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME"):
            location = root / variable.lower()
            location.mkdir(mode=0o700)
            env[variable] = str(location)
        # Prevent the caller's interpolation variables from overriding this fixture.
        for variable in ("DEMO_PORT", "DECOMPOSE_SESSION", "COMPOSE_SHELL"):
            env.pop(variable, None)
        env["DECOMPOSE_DAEMON_READY_TIMEOUT_MS"] = "10000"

        def run(args, cwd, expected=0):
            result = subprocess.run([str(binary), *args], cwd=cwd, env=env,
                                    text=True, capture_output=True, timeout=45)
            assert result.returncode == expected, (
                f"{shlex.join(args)}: expected {expected}, got {result.returncode}\n"
                f"{result.stdout}{result.stderr}")
            return result.stdout

        def command(line, cwd):
            args = shlex.split(line.replace("$PATH", env["PATH"]))
            assert args.pop(0) == "decompose", f"unexpected example command: {line}"
            # Follow is deliberately parsed without entering an unbounded stream.
            if args == ["logs", "-f"]:
                args.append("--help")
            return run(args, cwd)

        def port():
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                return str(sock.getsockname()[1])

        quick = root / "quick"
        quick.mkdir()
        yaml = blocks(source, "getting-started.md", "yaml")
        assert len(yaml) == 1, "expected one quickstart YAML example"
        (quick / "decompose.yaml").write_text(yaml[0].replace("8000", port()))
        try:
            commands = [line for block in blocks(source, "getting-started.md", "bash")
                        for line in block.splitlines() if line.startswith("decompose ")]
            assert commands, "missing quickstart commands"
            for line in commands:
                output = command(line, quick)
                if line == "decompose ps":
                    assert output.startswith("name"), output
                    assert "web" in output and "worker" in output, output
                if line == "decompose up -d --wait":
                    assert "all requested services are ready" in output, output
                if line == "decompose logs -n 5 worker":
                    deadline = time.monotonic() + 10
                    while "worker heartbeat" not in output and time.monotonic() < deadline:
                        time.sleep(0.1)
                        output = command(line, quick)
                    assert "worker heartbeat" in output, output
        finally:
            run(["down"], quick)

        migration = root / "migration"
        migration.mkdir()
        shell = blocks(source, "migration.md", "sh")
        assert len(shell) == 2, "expected setup and command blocks in migration guide"
        # Explicitly selected setup block creates only the walkthrough's local files.
        subprocess.run(["sh", "-eu", "-c", shell[0].replace("8765", port())],
                       cwd=migration, env=env, check=True, timeout=10)
        project = migration / "native-demo"
        yamls = blocks(source, "migration.md", "yaml")
        assert len(yamls) == 2, "expected Compose and native YAML examples"
        (project / "decompose.yml").write_text(yamls[1])
        try:
            for line in shell[1].splitlines():
                if not line.strip():
                    continue
                output = command(line, project)
                if line.startswith("decompose run "):
                    assert "hello from the host" in output, output
                    # exec's running-service guard and child exit propagation.
                    assert "hello from the host" in run(
                        ["exec", "--env", f"PATH={env['PATH']}", "web", "python3", "check.py"], project)
                    run(["run", "check", "sh", "-c", "exit 7"], project, expected=7)
        finally:
            run(["down"], project)

        overlay = root / "overlay"
        overlay.mkdir()
        (overlay / "base.yml").write_text("processes:\n  worker:\n    command: sleep 60\n")
        (overlay / "dev-overrides.yml").write_text("processes:\n  worker:\n    replicas: 2\n")
        examples = blocks(source, "configuration.md", "(?:bash|sh)")
        line = next(line for block in examples for line in block.splitlines()
                    if line.startswith("decompose -f base.yml -f dev-overrides.yml "))
        files = ["-f", "base.yml", "-f", "dev-overrides.yml"]
        try:
            command(line, overlay)
            status = json.loads(run([*files, "ps", "--json"], overlay))
            # Read the real result, not just Clap's acceptance of -f.
            services = status["processes"]
            assert len(services) == 2, status
            assert {s["base"] for s in services} == {"worker"}, status
            assert {s["replica"] for s in services} == {1, 2}, status
        finally:
            run([*files, "down"], overlay)
    print("checked quickstart, migration, overlays, and one-off commands")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--book", type=Path, default=Path("docs/book"))
    parser.add_argument("--source", type=Path, default=Path("docs/src"))
    parser.add_argument("--binary", type=Path, default=Path("target/debug/decompose"))
    args = parser.parse_args()
    check_links(args.book)
    check_examples(args.source, args.binary.resolve())


if __name__ == "__main__":
    main()
