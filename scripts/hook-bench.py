#!/usr/bin/env python3
"""Agent hook wall-time bench against a PRIVATE Engine. Headless.

    scripts/hook-bench.py BIN_DIR [--hooks 400] [--sessions 40] [--pad 800]
                          [--bg-remove 8] [--phases]

BIN_DIR holds release `ubrad-rs`, `ubra-holder` and `ubra` (for A/B,
copy each revision's binaries to their own directory). The bench starts
BIN_DIR/ubrad-rs with HOME set to a fresh fixture under /private/tmp,
spawns `--sessions` Claude Code sessions whose Agent is a stand-in that, like
Claude, runs its SessionEnd hook synchronously when terminated, and then times
`--hooks` `ubra hook PreToolUse|PostToolUse` processes from fork to exit
with Claude-shaped payloads. `--pad N` preloads N exited records so state.json
is about the size of a real one (~1.1 MB at 800). `--bg-remove K` removes K
further sessions while the hooks run, each behind a login shell that outlives
SIGTERM (as the wrapper's exec'd interactive shell does), which holds the
Registry through the Holder's TERM->KILL escalation. `--phases` also times
connect / Hello / hook.report from a raw socket client.

Nothing touches the installed app, its Engine or its state: every path is
under the fixture HOME, telemetry uploads are off, and teardown kills only
process groups whose command line names the fixture HOME.
"""
import argparse, glob, json, os, resource, shutil, signal, socket, subprocess, tempfile, threading, time

parser = argparse.ArgumentParser()
parser.add_argument("bin_dir")
parser.add_argument("--hooks", type=int, default=400)
parser.add_argument("--sessions", type=int, default=40)
parser.add_argument("--pad", type=int, default=0)
parser.add_argument("--bg-remove", type=int, default=0)
parser.add_argument("--phases", action="store_true")
args = parser.parse_args()
bin_dir = os.path.abspath(args.bin_dir)

# Short: the Engine's socket path must fit sockaddr_un.
home = tempfile.mkdtemp(prefix="uhb", dir="/private/tmp")
support = f"{home}/Library/Application Support/Ubra"
os.makedirs(support, mode=0o700)
env = {k: v for k, v in os.environ.items() if not k.startswith("UBRA_")}
env.update(HOME=home, UBRA_TELEMETRY_ENDPOINT="off", UBRA_MANIFESTS_DIR=f"{home}/manifests")

# The Agent: a Claude Code manifest whose binary is a stand-in.
agent = f"{home}/fake-claude"
with open(agent, "w") as f:
    f.write("""#!/usr/bin/python3
import os, signal, subprocess, sys, time
def end(*_):
    subprocess.run([os.environ["UBRA_CLI"], "hook", "SessionEnd"],
                   input=b'{"session_id":"fake","hook_event_name":"SessionEnd","reason":"other"}',
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sys.exit(0)
signal.signal(signal.SIGTERM, end)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
while True:
    time.sleep(1)
""")
os.chmod(agent, 0o755)
source = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../crates/ubra-engine/manifests")
shutil.copytree(source, f"{home}/manifests")
manifest = json.load(open(f"{home}/manifests/claude-code.json"))
manifest["agent"]["binary"] = agent
json.dump(manifest, open(f"{home}/manifests/claude-code.json", "w"))
if args.bg_remove:
    os.makedirs(f"{home}/.config/fish", exist_ok=True)
    open(f"{home}/.config/fish/config.fish", "w").write("function __bench_term --on-signal TERM\nend\n")
if args.pad:
    records = [{"id": f"s_pad{k:08x}", "kind": {"claudeCode": {}}, "cwd": "/private/tmp",
                "projectID": "p_bench", "title": "padded history " + "t" * 900, "titleSource": 3,
                "agentSessionID": f"10000000-0000-4000-8000-{k:012d}",
                "status": {"exited": {"_0": {"reason": "exited", "code": 0}}},
                "resumability": "notResumable", "createdAt": 1.79e12 + k, "updatedAt": 1.79e12 + k,
                "pinned": False} for k in range(args.pad)]
    json.dump({"version": 1, "projects": [{"id": "p_bench", "path": "/private/tmp", "name": "tmp"}],
               "sessions": records}, open(f"{support}/state.json", "w"))

engine = subprocess.Popen([f"{bin_dir}/ubrad-rs"], env=env, stdout=subprocess.DEVNULL,
                          stderr=open(f"{home}/engine.err", "w"), start_new_session=True)


def teardown():
    engine.send_signal(signal.SIGTERM)
    time.sleep(0.3)
    me = os.getpgrp()
    listing = subprocess.run(["ps", "-axo", "pgid=,command="], capture_output=True, text=True).stdout
    for line in listing.splitlines():
        pgid, _, command = line.strip().partition(" ")
        if home in command and int(pgid) != me:
            try:
                os.killpg(int(pgid), signal.SIGKILL)
            except OSError:
                pass
    shutil.rmtree(home, ignore_errors=True)


try:
    sock = f"{support}/daemon.sock"
    for _ in range(400):
        if os.path.exists(sock):
            break
        time.sleep(0.05)
    control = socket.socket(socket.AF_UNIX)
    control.connect(sock)
    replies = control.makefile("rb")
    counter = [0]

    def rpc(method, params):
        counter[0] += 1
        control.sendall((json.dumps({"id": counter[0], "method": method, "params": params}) + "\n").encode())
        while True:
            message = json.loads(replies.readline())
            if message.get("id") == counter[0]:
                assert "ok" in message, message
                return message["ok"]

    rpc("hello", {"build": "hook-bench"})
    ids = [rpc("session.spawn", {"kind": "claude-code", "cwd": "/private/tmp", "title": f"bench {i}"})["id"]
           for i in range(args.sessions + args.bg_remove)]
    time.sleep(1.0)
    hook_ids, doomed = ids[:args.sessions], ids[args.sessions:]

    def payload(i, event, prefix="toolu_"):
        j = (i // 2) % len(hook_ids)
        return j, {"session_id": f"00000000-0000-4000-8000-{j:012d}", "hook_event_name": event,
                   "tool_name": "Bash", "tool_use_id": f"{prefix}{i // 2:08d}", "cwd": "/private/tmp",
                   "transcript_path": f"{home}/.claude/projects/x/{j}.jsonl",
                   "tool_input": {"command": "ls -la " + "x" * 200}}

    if args.phases:
        phases = {"connect": [], "hello": [], "hook.report": []}
        for i in range(args.hooks):
            event = "PreToolUse" if i % 2 == 0 else "PostToolUse"
            j, body = payload(i, event, "toolu_p")
            a = time.perf_counter_ns()
            c = socket.socket(socket.AF_UNIX); c.connect(sock); f = c.makefile("rb")
            b = time.perf_counter_ns()
            c.sendall(b'{"id":1,"method":"hello","params":{"build":"hook-bench"}}\n'); f.readline()
            d = time.perf_counter_ns()
            c.sendall((json.dumps({"id": 2, "method": "hook.report", "params": {
                "kind": "claude-hook", "ubraSessionID": hook_ids[j], "event": event,
                "payload": body}}) + "\n").encode()); f.readline()
            e = time.perf_counter_ns()
            c.close()
            for name, value in (("connect", b - a), ("hello", d - b), ("hook.report", e - d)):
                phases[name].append(value / 1e6)
        for name, values in phases.items():
            values.sort()
            print(json.dumps({"phase": name, "p50_ms": round(values[len(values) // 2], 3),
                              "p95_ms": round(values[int(len(values) * 0.95)], 3), "max_ms": round(values[-1], 2)}))

    removals = []

    def remove_in_background():
        c = socket.socket(socket.AF_UNIX); c.connect(sock); f = c.makefile("rb")
        for k, sid in enumerate(doomed):
            time.sleep(0.3)
            a = time.perf_counter_ns()
            c.sendall((json.dumps({"id": k, "method": "session.remove", "params": {"sessionID": sid}}) + "\n").encode())
            while json.loads(f.readline()).get("id") != k:
                pass
            removals.append((time.perf_counter_ns() - a) / 1e6)

    remover = threading.Thread(target=remove_in_background, daemon=True)
    if doomed:
        remover.start()
    walls = []
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    for i in range(args.hooks):
        event = "PreToolUse" if i % 2 == 0 else "PostToolUse"
        j, body = payload(i, event)
        hook_env = dict(env, UBRA_SESSION_ID=hook_ids[j], UBRA_SOCKET=sock,
                        UBRA_SESSION_RECOVERY_DIR=f"{home}/recovery/{hook_ids[j]}")
        a = time.perf_counter_ns()
        done = subprocess.run([f"{bin_dir}/ubra", "hook", event], input=json.dumps(body).encode(),
                              env=hook_env, stdout=subprocess.PIPE)
        walls.append((time.perf_counter_ns() - a) / 1e6)
        assert done.returncode == 0 and done.stdout.strip() == b"{}", done
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if doomed:
        remover.join()
    walls.sort()
    q = lambda p: round(walls[min(len(walls) - 1, int(len(walls) * p))], 2)
    result = {"hooks": args.hooks, "sessions": args.sessions,
              "state_bytes": os.path.getsize(f"{support}/state.json"),
              "wall_p50_ms": q(0.5), "wall_p90_ms": q(0.9), "wall_p95_ms": q(0.95), "wall_p99_ms": q(0.99),
              "wall_max_ms": round(walls[-1], 2),
              "cli_cpu_s": round(after.ru_utime - before.ru_utime + after.ru_stime - before.ru_stime, 3)}
    if doomed:
        result["session_remove_ms"] = [round(x) for x in removals]
    print(json.dumps(result))
finally:
    teardown()
