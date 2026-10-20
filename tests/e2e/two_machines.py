"""Two 'machines' (two REASON_NETWORK_HOMEs) on one box, talking over iroh, with simulated agent sessions.

    cargo build && python3 tests/e2e/two_machines.py

Needs internet (iroh relays/discovery). Run inside a Claude Code session, the simulated Claude session pushes its
test messages into that real session (they say "ignore this").
"""
import json, os, re, subprocess, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-e2e-")
H = {"alex": os.path.join(D, "alex"), "alice": os.path.join(D, "alice")}
MACHINE = {"alex": "laptop", "alice": "desktop"}
FAKE_CODEX = os.path.join(D, "fake-codex.sh")
QUEUE_LOG = os.path.join(D, "codex-queue.log")
# Stands in for osascript / notify-send, so the test shows nothing on screen: each machine logs what it would show.
FAKE_NOTIFY = os.path.join(D, "fake-notify.sh")
IN_CLAUDE = "CLAUDE_CODE_MESSAGING_SOCKET" in os.environ
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who, strip_claude=True):
    e = dict(os.environ, REASON_NETWORK_HOME=H[who], REASON_NETWORK_MACHINE=MACHINE[who], REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_CODEX_BIN=FAKE_CODEX)
    e.update({"REASON_NETWORK_OSASCRIPT_BIN": FAKE_NOTIFY, "REASON_NETWORK_NOTIFY-SEND_BIN": FAKE_NOTIFY, "DBUS_SESSION_BUS_ADDRESS": "test",
              "NOTIFY_LOG": os.path.join(D, f"notify-{who}.log")})
    if strip_claude:
        for k in list(e):
            if k.startswith("CLAUDE"):
                del e[k]
    # Never this machine's real Claude sessions: Reason Network could deliver into them.
    e["CLAUDE_CONFIG_DIR"] = os.path.join(D, "claude-config")
    return e


def cli(who, *args):
    r = subprocess.run([BIN, *args], env=env(who), capture_output=True, text=True, timeout=90)
    return (r.stdout + r.stderr).strip()


def hook(who, agent, event, payload):
    r = subprocess.run([BIN, "hook", agent, event], env=env(who), capture_output=True, text=True,
                       input=json.dumps(payload), timeout=30)
    return r.stdout.strip()


class Mcp:
    def __init__(self, who, client, strip_claude, cwd):
        self.p = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env(who, strip_claude),
                                  cwd=cwd, text=True, bufsize=1)
        self.id = 0
        self.req("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": client, "version": "1"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")

    def req(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n")
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.id:
                return msg

    def tool(self, name, args, meta=None):
        params = {"name": name, "arguments": args}
        if meta:
            params["_meta"] = meta
        res = self.req("tools/call", params).get("result", {})
        return "\n".join(c.get("text", "") for c in res.get("content", []))

    def close(self):
        self.p.stdin.close()
        self.p.wait(5)


with open(FAKE_CODEX, "w") as f:
    f.write(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {QUEUE_LOG}\necho "Queued message"\n')
os.chmod(FAKE_CODEX, 0o755)
with open(FAKE_NOTIFY, "w") as f:
    f.write('#!/bin/sh\nprintf "%s\\n" "$*" >> "$NOTIFY_LOG"\n')
os.chmod(FAKE_NOTIFY, 0o755)


def shown(who):
    time.sleep(0.5)
    p = os.path.join(D, f"notify-{who}.log")
    return open(p).read() if os.path.exists(p) else ""
os.makedirs(os.path.join(D, "work"), exist_ok=True)

# --- team formation ---
out = cli("alex", "init", "vibes", "--as", "alex", "--no-setup")
check("init creates team", "Created team vibes. You are alex on laptop." in out, out)
out = cli("alex", "invite")
link = re.search(r"(https://\S+/join/#tincan1\S+)", out)
check("invite prints the team link", bool(link), out)
out = cli("alice", "join", link.group(1), "--as", "alice", "--no-setup")
check("join with the team link succeeds", "Joined team vibes as alice on desktop." in out, out)
check("joiner sees alex connected", re.search(r"alex\s+laptop\s+(direct|relay)", out) is not None, out)
out = cli("alex", "team")
check("founder sees alice connected", re.search(r"alice\s+desktop\s+(direct|relay)", out) is not None, out)
out = cli("alice", "join", link.group(1), "--as", "bob", "--no-setup")
check("joining a second team is refused while yours has others, and says how to switch",
      "already in team 'vibes' (with alex)" in out and "reason-network leave" in out, out)
# Installed on its own and started a team by itself first: the team link still just works.
sam = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "sam"), REASON_NETWORK_MACHINE="mini")
r = subprocess.run([BIN, "init", "solo", "--as", "sam", "--no-setup"], env=sam, capture_output=True, text=True, timeout=90)
r = subprocess.run([BIN, "join", link.group(1), "--as", "sam", "--no-setup"], env=sam, capture_output=True, text=True, timeout=90)
check("a team of one is swapped for the team you join", "Joined team vibes as sam on mini." in r.stdout, r.stdout + r.stderr)
subprocess.run([BIN, "leave"], env=sam, capture_output=True, timeout=60)
subprocess.run([BIN, "stop"], env=sam, capture_output=True)
os.makedirs(os.path.join(D, "bob"), exist_ok=True)
bob = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "bob"), REASON_NETWORK_MACHINE="nuc")
out = cli("alex", "invite", "--code")
code = re.search(r"Invite code for a teammate: ([a-z]+(?:-[a-z]+){3})", out)
check("invite --code prints a four-word code", bool(code), out)
r = subprocess.run([BIN, "join", code.group(1).replace("-", " ").upper(), "--as", "bob", "--no-setup"], env=bob,
                   capture_output=True, text=True, timeout=90)
check("joining with just the code works", "Joined team vibes as bob on nuc." in r.stdout, r.stdout + r.stderr)
r = subprocess.run([BIN, "join", code.group(1), "--as", "bob2", "--no-setup"], env=dict(bob, REASON_NETWORK_HOME=os.path.join(D, "bob2")),
                   capture_output=True, text=True, timeout=90)
check("a used code doesn't work twice", "Joined" not in r.stdout + r.stderr, r.stdout + r.stderr)
subprocess.run([BIN, "stop"], env=dict(bob, REASON_NETWORK_HOME=os.path.join(D, "bob2")), capture_output=True)
out = subprocess.run([BIN, "link"], env=bob, capture_output=True, text=True, timeout=30).stdout
check("any member can show the same team link", link.group(1).split("#")[1][:40] in out.replace("\n", ""), out)
subprocess.run([BIN, "stop"], env=bob, capture_output=True)

# --- sessions on both machines ---
claude = Mcp("alex", "claude-code", strip_claude=False, cwd=ROOT)
codex = Mcp("alice", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "work"))
meta = {"sessionId": "11111111-aaaa", "threadId": "11111111-aaaa"}
time.sleep(7)  # Claude's session registers after 3s; presence propagates every 2s
out = codex.tool("list_team", {}, meta)
claude_name = os.path.basename(ROOT).lower() + "-claude"
check("alice's agent sees alex's session across machines", claude_name in out and "alex · laptop" in out, out)
# (After Codex's first tool call registered it.) Nobody reports a status: Reason Network reads the plan step in progress from Codex's own transcript.
rollout = os.path.join(D, "rollout.jsonl")
plan = {"plan": [{"step": "set up fixtures", "status": "completed"}, {"step": "testing cross-machine Reason Network", "status": "in_progress"}]}
with open(rollout, "w") as f:
    f.write(json.dumps({"type": "response_item", "payload": {"type": "function_call", "name": "update_plan", "arguments": json.dumps(plan)}}) + "\n")
hook("alice", "codex", "doing", {"session_id": "11111111-aaaa", "cwd": os.path.join(D, "work"), "transcript_path": rollout})
time.sleep(3)
out = cli("alex", "who")
check("alex sees what alice's codex is doing, read from its plan", "work-codex" in out and "testing cross-machine Reason Network" in out, out)

# --- messages across machines ---
out = codex.tool("send_message", {"to": f"alex/{claude_name}", "message": "Reason Network e2e (cross-machine push): ignore this."}, meta)
want = "delivered into the session" if IN_CLAUDE else "only sees it when it checks"
check("cross-machine send reaches the Claude session", want in out, out)
out = cli("alex", "send", "alice/work-codex", "hello from alex's terminal")
check("a terminal send to a remote pull session is queued there", "only sees it when it checks its inbox" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("remote message arrives in codex inbox, from alex/terminal", 'from="alex/terminal"' in out and "hello from alex's terminal" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…and is gone once read", out.startswith("No messages."), out)
out = cli("alice", "send", "alex", "dinner at 8?")
check("a person is not an address: the error lists their agent sessions",
      "messages go to agent sessions; alex's are:" in out and f"alex/{claude_name}" in out, out)
out = codex.tool("send_message", {"to": f"alex/{claude_name}", "message": "can you approve the deploy? (e2e test, ignore)", "notify": True}, meta)
check("notify: the message also pops up on the person's screen", "popped up for alex (laptop)" in out, out)
log = shown("alex")
check("…naming who asks, which session, and the ask", f"alice/work-codex → {claude_name}" in log and "approve the deploy" in log, log)
out = cli("alice", "send", f"alex/{claude_name}", "--notify", "and from the terminal (e2e test, ignore)")
check("one pop-up per person a minute", "not popped up: laptop had one from alice" in out and "from the terminal" not in shown("alex"), out)
out = codex.tool("send_message", {"to": "alex", "message": "x", "notify": True}, meta)
check("a teammate's pop-up goes with a message to one of their sessions", "messages go to agent sessions" in out, out)
out = codex.tool("send_message", {"to": "alice", "message": "need your ok (e2e test, ignore)", "notify": True}, meta)
check("an agent can tap its own user on the shoulder", "popped up for alice (desktop)" in out and "need your ok" in shown("alice"), out)
out = codex.tool("send_message", {"to": "alex/*", "message": "x", "notify": True}, meta)
check("notify doesn't go to groups", "not a group" in out, out)
# No waiting for presence: the terminal that just messaged alice can be answered at once.
out = codex.tool("send_message", {"to": "alex/terminal", "message": "codex here: replying to your terminal"}, meta)
check("an agent can reply at once to a terminal that just messaged it", "→ alex/terminal: queued" in out or "→ alex/terminal: delivered" in out, out)
time.sleep(1)
out = cli("alex", "listen", "--as", "terminal", "--wait", "5")
check("…and `reason network listen --as terminal` shows the reply", "replying to your terminal" in out and 'from="alice/work-codex"' in out, out)
cli("alice", "listen", "--as", "fresh", "--wait", "0")
cli("alice", "send", "alex/terminal", "--from", "alice/fresh", "first words from a new session")
out = cli("alex", "send", "alice/fresh", "answered at once")
check("a brand-new session that just messaged can be answered at once", "→ alice/fresh" in out and "no session" not in out, out)
out = cli("alice", "listen", "--as", "fresh", "--wait", "5")
check("…and it gets the answer", "answered at once" in out, out)
cli("alex", "listen", "--as", "terminal", "--wait", "0")
out = cli("alex", "send", "*", "a script's broadcast (e2e test, ignore)")
check("a group send from a script, not a person at the terminal, is refused", "only a person at a terminal" in out, out)
out = cli("alex", "send", "*", "--from", "alex/terminal", "broadcast from alex (e2e test, ignore)")
check("…but goes out with --from alex/terminal", "alice/work-codex" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…and only that one arrived", "broadcast from alex" in out and "a script's broadcast" not in out, out)
for to in ("alex/*", "*@laptop"):
    out = codex.tool("send_message", {"to": to, "message": "an agent's broadcast (e2e test, ignore)"}, meta)
    check(f"an agent can't send to a group ({to})", "agents message one session, not a group" in out and "→" not in out, out)
out = cli("alex", "send", "nobody", "x")
check("unknown name fails helpfully", "no session called 'nobody'" in out, out)

# --- a session waiting in check_inbox gets the reply there, once ---
import threading
waited = {}
t = threading.Thread(target=lambda: waited.setdefault("out", codex.tool("check_inbox", {"wait_secs": 15}, meta)))
t.start()
time.sleep(2)
out = cli("alex", "send", "alice/work-codex", "reply while you wait")
t.join()
check("long-poll returns the message that arrives", "reply while you wait" in waited.get("out", ""), waited)

# --- small text files ---
FILES = os.path.join(D, "files")
os.makedirs(FILES, exist_ok=True)
def write(name, data):
    path = os.path.join(FILES, name)
    with open(path, "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode())
    return path
def inbox(who):
    p = os.path.join(H[who], "inbox")
    return sorted(os.listdir(p)) if os.path.isdir(p) else []
def message_with(out, text):
    """The part of an inbox listing for the message saying `text`: its frame and the file list after it."""
    return next((part for part in out.split("<reason-message") if text in part.split("</reason-message>")[0]), "")
def saved(out, who):
    """The file paths a message lists, if each is in `who`'s inbox folder, readable and not executable."""
    paths = re.findall(r"^- (/.+)$", out, re.M)
    ok = all(p.startswith(os.path.join(H[who], "inbox") + os.sep) and os.path.isfile(p) and not os.stat(p).st_mode & 0o111
             for p in paths)
    return paths if ok else []
notes = write("notes.md", "# Notes\nline two \u2014 caf\u00e9\n")
config = write("app.json", '{"port": 8080}\n')
out = cli("alex", "send", "alice/work-codex", "--file", notes, "--file", config, "the notes and the config (e2e test)")
check("a send with files is accepted", "→ alice/work-codex" in out and "refused" not in out, out)
out = codex.tool("check_inbox", {}, meta)
paths = saved(out, "alice")
check("files arrive in the message, listed by where they are saved",
      "Files they sent with" in out.split("</reason-message>")[-1] and [os.path.basename(p) for p in paths] == ["notes.md", "app.json"], out)
check("…in folders named by the recipient, not by the sender's message id",
      all(re.fullmatch(r"[0-9A-HJKMNP-TV-Z]{26}", os.path.basename(os.path.dirname(p))) for p in paths)
      and re.search(r'id="([0-9A-Z]{26})"', out).group(1) not in inbox("alice"), out)
check("…with the same text, readable and not executable",
      len(paths) == 2 and open(paths[0], encoding="utf-8").read() == "# Notes\nline two \u2014 caf\u00e9\n" and open(paths[1]).read() == '{"port": 8080}\n', paths)
with open(os.path.join(D, "work", "reply.md"), "w") as f:
    f.write("relative paths are from the agent's folder\n")
out = codex.tool("send_message", {"to": "alex/terminal", "message": "a file back (e2e test)", "files": ["reply.md"]}, meta)
check("an agent sends a file by a path relative to its folder", "→ alex/terminal" in out and "refused" not in out, out)
out = cli("alex", "listen", "--as", "terminal", "--wait", "5")
paths = saved(out, "alex")
check("…and it lands on the other machine", len(paths) == 1 and open(paths[0]).read().startswith("relative paths"), out)
def work(name, data):
    path = os.path.join(D, "work", name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode())
    return name
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("logo.png", b"\x89PNG\r\n\x1a\n\x00\x00")]}, meta)
check("a binary file is refused, saying why", "looks like a binary file" in out and "only text files" in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("term.log", "\x1b[31mred\x1b[0m\n")]}, meta)
check("…and so is text with control characters", "control characters" in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("big.log", "x" * (256 * 1024 + 1))]}, meta)
check("a file over 256 KB is refused", "a file can be at most 256 KB" in out and "→" not in out, out)
# An agent sends only from its own folder: nothing outside it, nothing hidden (a prompt-injected agent can't mail secrets).
work(".env", "SECRET=1\n")
for path, why in [(notes, "outside this session's folder"), (".env", "hidden file"), ("../alice/machine.key", "outside this session's folder")]:
    out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [path]}, meta)
    check(f"an agent can't send {os.path.basename(path)} ({why})", why in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [notes] * 11}, meta)
check("more than 10 files are refused", "at most 10" in out and "→" not in out, out)
out = cli("alex", "send", "alice/work-codex", "--file", FILES, "x")
check("a folder is not a file", "is not a file" in out, out)
# A teammate's files are taken up to 20 MB an hour; past that their machine keeps them and retries later.
quota = [write("q1.log", "q" * (256 * 1024)), write("q2.log", "r" * (256 * 1024))]
# (alex sent a few small files above, so 39 of 512 KB fit the hour, and the 40th doesn't.)
sent = [cli("alex", "send", "alice/work-codex", "--file", quota[0], "--file", quota[1], f"quota {i} (e2e test)") for i in range(39)]
check("39 messages of 512 KB of files are taken", all("→ alice/work-codex" in o and "can't take" not in o for o in sent), sent[-1])
out = cli("alex", "send", "alice/work-codex", "--file", quota[0], "--file", quota[1], "one more (e2e test)")
check("…and the next waits: the sender's machine retries later", "can't take these files now" in out and "20 MB of files an hour from alex" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…the 39 arrive", out.count("quota ") == 39 and "one more" not in out, out[-500:])

# --- Codex hooks: session via hooks, delivery via codex queue ---
start = hook("alice", "codex", "session-start", {"session_id": "22222222-bbbb", "cwd": ROOT, "hook_event_name": "SessionStart"})
codex_name = os.path.basename(ROOT).lower() + "-codex"
check("codex SessionStart hook introduces the session", "SessionStart" in start and f"alice/{codex_name}" in start, start)
time.sleep(3)
out = cli("alex", "send", f"alice/{codex_name}", "please rebase on main")
check("message to a hooked codex session goes to codex queue", "queued into the Codex session" in out, out)
time.sleep(1.5)
log = open(QUEUE_LOG).read() if os.path.exists(QUEUE_LOG) else ""
check("codex queue got the thread and the message", "queue --thread 22222222-bbbb --message" in log and "please rebase on main" in log, log)
out = hook("alice", "codex", "post-tool-use", {"session_id": "22222222-bbbb", "cwd": ROOT, "hook_event_name": "PostToolUse"})
check("a message delivered by codex queue isn't injected again by the hook", out == "", out)

# --- hold policy ---
cfg = os.path.join(H["alice"], "config.toml")
text = open(cfg).read()
text = text.replace("[inbound.people]", '[inbound.people]\nalex = "hold"', 1) if "[inbound.people]" in text else text + '\n[inbound.people]\nalex = "hold"\n'
open(cfg, "w").write(text)
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)
before = inbox("alice")
out = cli("alex", "send", "alice/work-codex", "--file", notes, "held message")
check("message to alice is held by her policy", "held until alice approves" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("held message is not delivered", "held message" not in out, out)
check("…and its files aren't saved yet", inbox("alice") == before, inbox("alice"))
out = cli("alice", "held")
check("Reason Network held lists it", "held message" in out and "notes.md (saved once approved)" in out, out)
cli("alice", "approve", "--all")
out = codex.tool("check_inbox", {}, meta)
check("approved message is delivered", "held message" in out, out)
check("…with its files saved now", [os.path.basename(p) for p in saved(message_with(out, "held message"), "alice")] == ["notes.md"], out)
# Refused: nothing of it is kept.
text = open(cfg).read()
open(cfg, "w").write(text.replace('alex = "hold"', 'alex = "refuse"'))
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)
before = inbox("alice")
out = cli("alex", "send", "alice/work-codex", "--file", notes, "refused message")
check("a refused message's files are never saved", "doesn't accept messages from alex" in out and inbox("alice") == before, out)
open(cfg, "w").write(text)
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)

# --- offline machine: outbox ---
codex.close()
cli("alice", "stop")
time.sleep(1)
out = cli("alex", "send", "alice/work-codex", "sent while you were offline")
check("message to an offline machine is queued", "not connected" in out or "no answer" in out, out)
cli("alice", "team")  # restarts alice's daemon
time.sleep(8)
cli("alice", "approve", "--all")  # alice still holds messages from alex (the hold policy above)
codex = Mcp("alice", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "work"))
out = codex.tool("check_inbox", {}, meta)
check("outbox delivers when the machine is back", "sent while you were offline" in out, out)
codex.close()

# --- removal while away ---
cli("alice", "stop")
out = cli("alex", "remove", "alice")
check("remove alice (while her machine is off)", "Removed alice on desktop" in out, out)
check("removing someone replaces the team link", "team link changed" in out, out)
out = cli("alex", "link")
check("the new link differs from the old one", link.group(1) not in out, out)
cli("alice", "team")  # alice's machine comes back and dials alex
time.sleep(10)
out = cli("alice", "team")
check("alice's machine learns it was removed when it reconnects", "not in a team" in out, out)

claude.close()
cli("alex", "stop")
cli("alice", "stop")
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
