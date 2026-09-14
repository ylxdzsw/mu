"""Exercise the installed/portable Windows CLI without external providers."""

import base64
import ctypes
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time


def native(path):
    return Path(subprocess.check_output(["cygpath", "-aw", str(path)], text=True, encoding="utf-8").strip())


def main():
    binary = native(sys.argv[1])
    requests = []
    scenario = "normal"

    class Provider(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            if len(requests) == 1 or scenario == "crash":
                command = (
                    "# long command exercises Windows command-line transport\n"
                    + "# padding\n" * 4000
                    + "read -r text; printf '%s' \"$text\" > message.txt; "
                    + "cat message.txt; view_image pixel.png"
                )
                if scenario == "crash":
                    command = "python -c 'import os,time; print(os.getpid(), flush=True); time.sleep(90)' > native-pid & wait"
                delta = {"tool_calls": [{
                    "index": 0,
                    "id": "windows-call",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": json.dumps({
                            "title": "Windows acceptance",
                            "risk": "reversible",
                            "command": command,
                            "cwd": subprocess.check_output(
                                ["cygpath", "-au", str(project)], text=True, encoding="utf-8"
                            ).strip(),
                            "stdin": "hello 雪\n",
                        }),
                    },
                }]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "windows ok"}, "stop"
            body = "data: " + json.dumps({
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30},
            }) + "\n\ndata: [DONE]\n\n"
            data = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *_):
            pass

    with tempfile.TemporaryDirectory(prefix="mu-windows-雪 ") as temporary:
        root = Path(temporary)
        project = root / "project space 雪"
        project.mkdir()
        global_dir = root / "config"
        global_dir.mkdir()
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            (global_dir / "config.jsonc").write_text(json.dumps({
                "providers": {"local": {
                    "endpoint": f"http://127.0.0.1:{server.server_port}/v1/chat/completions",
                    "models": {"fixture": {"context_window": 100000}},
                }},
                "compaction": {"enabled": False},
            }), encoding="utf-8")
            env = dict(os.environ, MU_CONFIG_DIR=str(global_dir), XDG_CACHE_HOME=str(root / "cache"))
            env.pop("MU_SUBAGENT_DEPTH", None)

            def run(*args, stdin="", code=0):
                result = subprocess.run(
                    [str(binary), *args], cwd=project, env=env, input=stdin,
                    encoding="utf-8", capture_output=True, timeout=90,
                )
                assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
                return result.stdout

            run("--help")
            run("init")
            session = run("new").strip()
            assert session.startswith("ses_")
            assert run("cat", stdin="literal prompt 雪") == "literal prompt 雪"
            (project / "pixel.png").write_bytes(base64.b64decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6"
                "AAAAAElFTkSuQmCC"
            ))

            trapped = run("-s", session, "--trap", "all", "-o", "final", stdin="run fixture", code=3)
            assert "Windows acceptance" in trapped
            assert not (project / "message.txt").exists()
            assert run("retry", "-s", session, "--trap", "off", "-o", "final") == "windows ok"
            assert (project / "message.txt").read_text(encoding="utf-8") == "hello 雪"
            assert len(requests) == 2
            assert "data:image/png;base64," in json.dumps(requests[1])
            transcript = run("transcript", "-s", session, "-o", "detail")
            assert "windows ok" in transcript and "hello 雪" in transcript
            assert "\x1b" not in transcript
            run("retry", "-s", session, "-o", "final")
            journal = project / ".mu" / "sessions" / (session + ".jsonl")
            events = [json.loads(line) for line in journal.read_text(encoding="utf-8").splitlines()]
            assert events[0]["version"] == 4
            assert (project / ".mu" / "current-session").is_file()

            applets = root / "cache" / "mu" / "applets"
            if not applets.is_dir():
                applets = binary.parent.parent / "libexec" / "mu"
            for applet in ("apply_patch", "edit", "view_image"):
                assert (applets / (applet + ".exe")).is_file(), applets
            for applet, args, text in [
                ("apply_patch", [], "*** Begin Patch\n*** Add File: edited.txt\n+old\n*** End Patch\n"),
                ("edit", ["edited.txt"], "<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n"),
            ]:
                subprocess.run([str(applets / (applet + ".exe")), *args], cwd=project,
                               env=env, input=text, encoding="utf-8", check=True, timeout=30)
            assert (project / "edited.txt").read_text().strip() == "new"
            scenario = "crash"
            child = subprocess.Popen([str(binary), "--trap", "off", "-o", "final"],
                                     cwd=project, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            child.stdin.write(b"crash fixture")
            child.stdin.close()
            handle = None
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.OpenProcess.restype = ctypes.c_void_p
            kernel.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
            kernel.CloseHandle.argtypes = [ctypes.c_void_p]
            try:
                deadline = time.monotonic() + 30
                marker = project / "native-pid"
                while not marker.exists() or not marker.stat().st_size:
                    assert child.poll() is None, child.stderr.read().decode(errors="replace")
                    assert time.monotonic() < deadline, "descendant did not start"
                    time.sleep(0.05)
                pid = int(marker.read_text().strip())
                handle = kernel.OpenProcess(0x1000, False, pid)
                assert handle, ctypes.get_last_error()
                status = ctypes.c_ulong()
                assert kernel.GetExitCodeProcess(handle, ctypes.byref(status)) and status.value == 259
                child.kill()
                child.wait(timeout=10)
                deadline = time.monotonic() + 10
                while True:
                    assert kernel.GetExitCodeProcess(handle, ctypes.byref(status))
                    if status.value != 259:
                        break
                    assert time.monotonic() < deadline, "descendant survived Mu process death"
                    time.sleep(0.05)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=10)
                child.stdout.close()
                child.stderr.close()
                if handle:
                    kernel.CloseHandle(handle)
            scenario = "normal"
            assert run("retry", "--trap", "off", "-o", "final") == "windows ok"
            assert len(requests) == 4
            print("Windows acceptance: CLI, trapping/retry, long command, literal stdin, Unicode paths, image attachment, journal, applets, parent-death cleanup and crash retry passed")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
