#!/usr/bin/env python3
"""restart: how long clients wait while the server restarts onto a new binary.

Runs a copy of the seed database behind a socket-activated listener (as
systemd runs it), keeps senders and syncs going, replaces the binary and
sends SIGHUP. Reports failed requests, the longest wait, and how long the
restart took. BENCH_SEED=big for the larger dataset.
"""

import os
import shutil
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(__file__))
import bench  # noqa: E402
from bench import HOST, PORT, ROOT, q  # noqa: E402

SENDERS = 8
SYNCERS = 4
RESTARTS = 2
SPACING = 15
PROFILE = os.environ.get("PROFILE", "ramfull")


def main():
    meta = bench.json.load(open(bench.SEED_META))
    shutil.rmtree(bench.RUN_DB, ignore_errors=True)
    shutil.copytree(bench.SEED_DB, bench.RUN_DB)

    exe = os.path.join(ROOT, "restart-bin", "anoncat")
    os.makedirs(os.path.dirname(exe), exist_ok=True)
    shutil.copy2(bench.BIN, exe)

    cfg = os.path.join(ROOT, "run.toml")
    with open(cfg, "w") as f:
        f.write(bench.BASE_CONFIG + f'database_path = "{bench.RUN_DB}"\n')
        f.write(open(os.path.join(os.path.dirname(__file__), "profiles", f"{PROFILE}.toml")).read())
    log = open(os.path.join(ROOT, "server.log"), "w")
    proc = subprocess.Popen(
        ["systemd-socket-activate", "-l", f"{HOST}:{PORT}", exe, "-c", cfg],
        stdout=log, stderr=subprocess.STDOUT)
    wait_ready(proc)
    server_log = os.path.join(ROOT, "server.log")
    while "Database prewarm finished" not in open(server_log).read():
        time.sleep(0.5)
    print(f"server up and warm, pid {proc.pid}, profile {PROFILE}")

    stop = threading.Event()
    lock = threading.Lock()
    samples = []  # (start, ms, ok)

    def record(t0, ok_):
        with lock:
            samples.append((t0, (time.perf_counter() - t0) * 1000, ok_))

    def sender(i):
        tok = meta["tokens"][i]
        room = meta["big"]
        n = 0
        while not stop.is_set():
            n += 1
            t0 = time.perf_counter()
            try:
                st, _ = bench.call("PUT", f"/_matrix/client/v3/rooms/{q(room)}/send/m.room.message/r{i}-{n}-{time.time_ns()}",
                                   {"msgtype": "m.text", "body": "restart"}, tok)
                record(t0, st == 200)
            except Exception:
                record(t0, False)

    def syncer(i):
        tok = meta["tokens"][100 + i]
        since = None
        while not stop.is_set():
            t0 = time.perf_counter()
            path = "/_matrix/client/v3/sync?timeout=0" + (f"&since={q(since)}" if since else "")
            try:
                st, out = bench.call("GET", path, token=tok)
                record(t0, st == 200)
                if st == 200:
                    since = out.get("next_batch", since)
            except Exception:
                record(t0, False)

    threads = [threading.Thread(target=sender, args=(i,)) for i in range(SENDERS)]
    threads += [threading.Thread(target=syncer, args=(i,)) for i in range(SYNCERS)]
    for t in threads:
        t.start()

    hups = []
    for _ in range(RESTARTS):
        time.sleep(SPACING)
        # What an update does: a new file takes the old one's name.
        tmp = exe + ".new"
        shutil.copy2(bench.BIN, tmp)
        os.rename(tmp, exe)
        hups.append(time.perf_counter())
        # systemd-socket-activate execs the server, so this pid is the server's.
        os.kill(proc.pid, signal.SIGHUP)
    time.sleep(SPACING)
    stop.set()
    for t in threads:
        t.join()

    alive = proc.poll() is None
    proc.send_signal(signal.SIGTERM)
    proc.wait(timeout=120)

    failed = [s for s in samples if not s[2]]
    print(f"requests {len(samples)}, failed {len(failed)}, process still running: {alive}")
    steady = [s[1] for s in samples if all(abs(s[0] - h) > 5 for h in hups)]
    steady.sort()
    print(f"away from restarts: median {steady[len(steady) // 2]:.0f} ms, p99 {steady[len(steady) * 99 // 100]:.0f} ms")
    for n, h in enumerate(hups, 1):
        hit = [s for s in samples if s[0] <= h + 1 and s[0] + s[1] / 1000 >= h]
        done = [s[0] + s[1] / 1000 - h for s in samples if s[0] >= h and s[2]]
        print(f"restart {n}: {len(hit)} requests caught in it, longest {max(s[1] for s in hit):.0f} ms, "
              f"first new request answered {min(done):.1f} s after SIGHUP")
    log = [line for line in open(os.path.join(ROOT, "server.log")) if "Received SIGHUP" in line
           or line.rstrip().endswith("Restart") or "Listening on" in line or "Opened database" in line]
    print("".join(log[-4 * RESTARTS:]), end="")


def wait_ready(proc):
    while True:
        if proc.poll() is not None:
            sys.exit(f"server exited, see {ROOT}/server.log")
        try:
            if bench.call("GET", "/_matrix/client/versions", timeout=2)[0] == 200:
                return
        except OSError:
            pass
        time.sleep(0.1)


if __name__ == "__main__":
    main()
