#!/usr/bin/env python3
"""bench: repeatable anoncat/tuwunel benchmark on the real disk.

  bench.py seed                      build the seed database once (users, rooms, history)
  bench.py run <profile> [tag]       copy the seed, start the server with profiles/<profile>.toml,
                                     measure cold then warm, save results/<profile>[-tag].json
  bench.py compare <a> <b> [...]     print results side by side

Cold numbers come first, right after start: with RocksDB's direct I/O the
kernel's page cache doesn't hold the database, so they are read off the
disk. Warm numbers are the same requests repeated.

Everything runs on 127.0.0.1 with federation off; nothing leaves the machine.
"""

import concurrent.futures as cf
import http.client
import json
import os
import random
import shutil
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.expanduser(os.environ.get("ANONCAT_BIN", "~/.cache/anoncat-target/release/tuwunel"))
ROOT = os.environ.get("ANONCAT_BENCH_DIR", "/Drives/server/bench")
SEED_DB = os.path.join(ROOT, "seed-db")
SEED_META = os.path.join(ROOT, "seed.json")
RUN_DB = os.path.join(ROOT, "run-db")
RESULTS = os.path.join(ROOT, "results")
HOST, PORT = "127.0.0.1", 8009
SERVER = "bench.test"
TOKEN = "bench-only-token"

USERS = 1000
ROOMS = 50
ROOM_MEMBERS = 100
MESSAGES = 50_000
WORKERS = 16

BASE_CONFIG = f"""[global]
server_name = "{SERVER}"
address = ["{HOST}"]
port = {PORT}
allow_federation = false
trusted_servers = []
allow_registration = true
registration_token = "{TOKEN}"
log = "info"
"""


# --- HTTP -------------------------------------------------------------------

_local = threading.local()


def call(method, path, body=None, token=None, timeout=600):
    conn = getattr(_local, "conn", None)
    if conn is None:
        conn = _local.conn = http.client.HTTPConnection(HOST, PORT, timeout=timeout)
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None if body is None else json.dumps(body)
    for attempt in range(3):
        try:
            conn.request(method, path, data, headers)
            r = conn.getresponse()
            raw = r.read()
            break
        except (http.client.HTTPException, OSError):
            conn.close()
            conn = _local.conn = http.client.HTTPConnection(HOST, PORT, timeout=timeout)
            if attempt == 2:
                raise
    out = json.loads(raw) if raw else {}
    return r.status, out


def ok(method, path, body=None, token=None):
    status, out = call(method, path, body, token)
    if status != 200:
        raise RuntimeError(f"{method} {path} -> {status} {out}")
    return out


def timed(method, path, token=None, body=None):
    t = time.perf_counter()
    ok(method, path, body, token)
    return (time.perf_counter() - t) * 1000


def q(s):
    return urllib.parse.quote(s, safe="")


# --- server -----------------------------------------------------------------

def start_server(db, profile_text=""):
    cfg = os.path.join(ROOT, "run.toml")
    with open(cfg, "w") as f:
        f.write(BASE_CONFIG + f'database_path = "{db}"\n' + profile_text)
    log = open(os.path.join(ROOT, "server.log"), "w")
    t = time.perf_counter()
    proc = subprocess.Popen([BIN, "-c", cfg], stdout=log, stderr=subprocess.STDOUT)
    while True:
        if proc.poll() is not None:
            sys.exit(f"server exited, see {ROOT}/server.log")
        try:
            if call("GET", "/_matrix/client/versions", timeout=2)[0] == 200:
                break
        except OSError:
            pass
        time.sleep(0.05)
    return proc, (time.perf_counter() - t) * 1000


def stop_server(proc):
    proc.terminate()
    proc.wait(timeout=120)
    _local.conn = None


def rss_mb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    return 0


def du_mb(path):
    total = 0
    for dirpath, _, files in os.walk(path):
        for name in files:
            total += os.path.getsize(os.path.join(dirpath, name))
    return total / 1024 / 1024


def evict_page_cache(path):
    """Drop the copy's pages from the kernel cache, so cold really is cold."""
    for dirpath, _, files in os.walk(path):
        for name in files:
            fd = os.open(os.path.join(dirpath, name), os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)


# --- seed -------------------------------------------------------------------

def register(i):
    body = {"username": f"u{i:05d}", "password": f"pw-{i}-bench", "inhibit_login": False}
    status, out = call("POST", "/_matrix/client/v3/register", body)
    if status == 401:
        body["auth"] = {"type": "m.login.registration_token", "token": TOKEN, "session": out["session"]}
        out = ok("POST", "/_matrix/client/v3/register", body)
    elif status != 200:
        raise RuntimeError(f"register u{i}: {status} {out}")
    return out["user_id"], out["access_token"]


def pmap(fn, items, label):
    t = time.perf_counter()
    with cf.ThreadPoolExecutor(WORKERS) as ex:
        out = list(ex.map(fn, items))
    print(f"  {label}: {len(items)} in {time.perf_counter() - t:.1f}s", flush=True)
    return out


def seed():
    if os.path.exists(SEED_DB):
        sys.exit(f"{SEED_DB} exists; delete it to reseed")
    os.makedirs(ROOT, exist_ok=True)
    rng = random.Random(42)
    proc, _ = start_server(SEED_DB)
    try:
        print("seeding:", flush=True)
        users = pmap(register, range(USERS), "users")
        tokens = [tok for _, tok in users]

        big = ok("POST", "/_matrix/client/v3/createRoom",
                 {"preset": "public_chat", "name": "big"}, tokens[0])["room_id"]
        pmap(lambda tok: ok("POST", f"/_matrix/client/v3/join/{q(big)}", {}, tok), tokens[1:], "big room joins")

        rooms = pmap(lambda i: ok("POST", "/_matrix/client/v3/createRoom",
                                  {"preset": "public_chat", "name": f"room{i}"}, tokens[i])["room_id"],
                     range(ROOMS), "rooms")
        # User 0 is in every room (the heavy sync account); the last user only
        # in the big room (the light one). Room i was created by user i.
        light = USERS - 1
        members = {r: sorted({0, i, *rng.sample(range(1, light), ROOM_MEMBERS - 2)}) for i, r in enumerate(rooms)}
        joins = [(r, u) for i, r in enumerate(rooms) for u in members[r] if u != i]
        pmap(lambda ru: ok("POST", f"/_matrix/client/v3/join/{q(ru[0])}", {}, tokens[ru[1]]), joins, "room joins")

        all_rooms = [big] + rooms
        room_members = {big: list(range(USERS)), **members}
        sends = []
        for n in range(MESSAGES):
            room = big if n % 5 == 0 else rng.choice(rooms)
            sends.append((room, rng.choice(room_members[room]), n))

        def send(job):
            room, u, n = job
            ok("PUT", f"/_matrix/client/v3/rooms/{q(room)}/send/m.room.message/s{n}",
               {"msgtype": "m.text", "body": f"seed message {n} " + "lorem ipsum " * rng.randint(1, 20)}, tokens[u])

        pmap(send, sends, "messages")
        meta = {"tokens": tokens, "big": big, "rooms": rooms, "all_rooms": all_rooms, "light_user": light}
        with open(SEED_META, "w") as f:
            json.dump(meta, f)
    finally:
        stop_server(proc)
    print(f"seed db: {du_mb(SEED_DB):.0f} MB at {SEED_DB}")


# --- run --------------------------------------------------------------------

def pct(values):
    values = sorted(values)
    return {
        "p50": round(statistics.median(values), 2),
        "p95": round(values[int(len(values) * 0.95) - 1], 2),
        "p99": round(values[int(len(values) * 0.99) - 1], 2),
        "max": round(values[-1], 2),
    }


def paginate(room, token, pages=20, limit=50):
    t = time.perf_counter()
    path = f"/_matrix/client/v3/rooms/{q(room)}/messages?dir=b&limit={limit}"
    frm = ""
    for _ in range(pages):
        out = ok("GET", path + (f"&from={q(frm)}" if frm else ""), token=token)
        frm = out.get("end")
        if not frm:
            break
    return (time.perf_counter() - t) * 1000


def run(profile, tag=""):
    meta = json.load(open(SEED_META))
    with open(os.path.join(HERE, "profiles", f"{profile}.toml")) as f:
        profile_text = f.read()
    tokens, big, rooms = meta["tokens"], meta["big"], meta["rooms"]
    heavy, light = tokens[0], tokens[meta["light_user"]]

    shutil.rmtree(RUN_DB, ignore_errors=True)
    shutil.copytree(SEED_DB, RUN_DB)
    evict_page_cache(RUN_DB)

    proc, startup_ms = start_server(RUN_DB, profile_text)
    res = {"profile": profile, "startup_ms": round(startup_ms)}
    try:
        # The prewarm runs in the background after start; the cold numbers are
        # what a client sees once it is done.
        if "rocksdb_prewarm = false" not in profile_text:
            t = time.perf_counter()
            while "Database prewarm finished" not in open(os.path.join(ROOT, "server.log")).read():
                if time.perf_counter() - t > 300:
                    sys.exit("prewarm did not finish")
                time.sleep(0.1)
            res["prewarm_ms"] = round((time.perf_counter() - t) * 1000)
        # Cold: first touch of each kind of data after start.
        res["cold_members_big_ms"] = round(timed("GET", f"/_matrix/client/v3/rooms/{q(big)}/members", heavy))
        res["cold_scrollback_big_ms"] = round(paginate(big, heavy))
        res["cold_scrollback_small_ms"] = round(paginate(rooms[7], heavy))
        res["cold_initial_sync_heavy_ms"] = round(timed("GET", "/_matrix/client/v3/sync?timeout=0", heavy))
        res["cold_initial_sync_light_ms"] = round(timed("GET", "/_matrix/client/v3/sync?timeout=0", light))

        # Warm: the same again.
        res["warm_members_big_ms"] = round(statistics.median(
            timed("GET", f"/_matrix/client/v3/rooms/{q(big)}/members", heavy) for _ in range(5)))
        res["warm_scrollback_big_ms"] = round(statistics.median(paginate(big, heavy) for _ in range(3)))
        res["warm_initial_sync_heavy_ms"] = round(statistics.median(
            timed("GET", "/_matrix/client/v3/sync?timeout=0", heavy) for _ in range(3)))

        # Sends under load: 16 senders, 4,000 messages over every room.
        rng = random.Random(7)
        jobs = [(rng.choice([big] + rooms), n) for n in range(4000)]

        def send(job):
            room, n = job
            return timed("PUT", f"/_matrix/client/v3/rooms/{q(room)}/send/m.room.message/r{profile}{n}{time.time_ns()}",
                         heavy, {"msgtype": "m.text", "body": f"run message {n}"})

        t = time.perf_counter()
        with cf.ThreadPoolExecutor(WORKERS) as ex:
            lat = list(ex.map(send, jobs))
        res["send_per_s"] = round(len(jobs) / (time.perf_counter() - t))
        res["send_ms"] = pct(lat)

        # Incremental sync right after a send, as a client would see it.
        since = ok("GET", "/_matrix/client/v3/sync?timeout=0", token=heavy)["next_batch"]
        inc = []
        for n in range(30):
            ok("PUT", f"/_matrix/client/v3/rooms/{q(big)}/send/m.room.message/i{n}{time.time_ns()}",
               {"msgtype": "m.text", "body": "ping"}, heavy)
            t = time.perf_counter()
            since = ok("GET", f"/_matrix/client/v3/sync?timeout=0&since={q(since)}", token=heavy)["next_batch"]
            inc.append((time.perf_counter() - t) * 1000)
        res["incremental_sync_ms"] = pct(inc)
        res["rss_mb"] = round(rss_mb(proc.pid))
    finally:
        stop_server(proc)
    res["db_mb"] = round(du_mb(RUN_DB))
    with open(os.path.join(ROOT, "server.log")) as f:
        log = f.read()
    res["write_stops"] = log.count("Database Stalled")  # writes fully stopped
    res["write_slowdowns"] = log.count("Database Stalling")  # writes delayed

    name = f"{profile}-{tag}" if tag else profile
    os.makedirs(RESULTS, exist_ok=True)
    with open(os.path.join(RESULTS, f"{name}.json"), "w") as f:
        json.dump(res, f, indent=2)
    compare([name])


def flatten(res):
    out = {}
    for k, v in res.items():
        if isinstance(v, dict):
            out.update({f"{k}.{kk}": vv for kk, vv in v.items()})
        elif k != "profile":
            out[k] = v
    return out


def compare(profiles):
    rows = [flatten(json.load(open(os.path.join(RESULTS, f"{p}.json")))) for p in profiles]
    keys = list(rows[0])
    print(f"{'':32}" + "".join(f"{p:>14}" for p in profiles))
    for k in keys:
        print(f"{k:32}" + "".join(f"{str(r.get(k, '-')):>14}" for r in rows))


if __name__ == "__main__":
    cmd, *rest = sys.argv[1:] or ["help"]
    if cmd == "seed":
        seed()
    elif cmd == "run" and len(rest) in (1, 2):
        run(*rest)
    elif cmd == "compare" and rest:
        compare(rest)
    else:
        print(__doc__)
