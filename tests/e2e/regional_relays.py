"""Reason's relay list: two machines homed on different regional relays still join and talk, and a team's own relay
still wins over the list. Each machine gets its own signed list from a local server (REASON_NETWORK_RELAYS_URL, which
debug builds honor), so alex knows only sfo1 and bea only fra1.

    cargo build && env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/regional_relays.py

Needs internet (Reason's live relays, n0's address lookup).
"""
import base64, http.server, json, os, re, subprocess, sys, tempfile, threading, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import KID, REASON, TRUST, b64, proof, sign

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.environ.get("REASON_NETWORK_TEST_BIN", os.path.join(ROOT, "target", "debug", "reason-network"))
D = tempfile.mkdtemp(prefix="reason-network-relays-")
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
RELAY = {r: f"https://relay-{r}.compute.reasonmachines.com" for r in ("sfo1", "iad1", "fra1", "sin1")}
# Who knows which relays; carl's team runs its own (sin1), which must win over his list (fra1).
WHO = {"alex": ("laptop", ["sfo1"]), "bea": ("desk", ["fra1"]), "carl": ("box", ["fra1"])}
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def relay_list(regions):
    """relays.json as Reason serves it: the list, and the same list signed."""
    relays = [{"url": RELAY[r], "region": r} for r in regions]
    now = int(time.time())
    claims = {"iss": "https://reasonmachines.com/network/relays", "aud": "reason-network", "relays": relays,
              "iat": now, "exp": now + 30 * 86400}
    head = {"alg": "RS256", "kid": KID, "typ": "reason-network-relays"}
    signing = f"{b64(json.dumps(head).encode())}.{b64(json.dumps(claims).encode())}"
    return json.dumps({"relays": relays, "signed": f"{signing}.{b64(sign(REASON, signing.encode()))}"}).encode()


LISTS = {f"/{w}.json": relay_list(regions) for w, (_, regions) in WHO.items()}


class Lists(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = LISTS.get(self.path)
        self.send_response(200 if body else 404)
        self.send_header("Content-Length", str(len(body or b"")))
        self.end_headers()
        self.wfile.write(body or b"")

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Lists)
threading.Thread(target=server.serve_forever, daemon=True).start()


def env(who):
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    e.update(HOME=os.path.join(D, "home"), PATH=FAKE_BIN + os.pathsep + os.environ["PATH"], CLAUDE_CONFIG_DIR=os.path.join(D, "claude"),
             REASON_NETWORK_HOME=os.path.join(D, who), REASON_NETWORK_MACHINE=WHO[who][0], REASON_NETWORK_NO_MDNS="1",
             REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
             REASON_NETWORK_TRUST_KEYS=TRUST, REASON_NETWORK_RELAYS_URL=f"http://127.0.0.1:{server.server_port}/{who}.json")
    return e


def cli(who, *args):
    r = subprocess.run([BIN, *args], env=env(who), capture_output=True, text=True, timeout=90)
    return (r.stdout + r.stderr).strip()


def until(who, args, ok, secs=60):
    deadline = time.time() + secs
    while True:
        out = cli(who, *args)
        if ok(out) or time.time() > deadline:
            return out
        time.sleep(1)


def link_relays(out):
    """A team link's relay fields: the team's own, and the inviting machine's home relay."""
    t = re.search(r"#rn1-(\S+)", out).group(1)
    raw, at, fields = base64.urlsafe_b64decode(t + "=" * (-len(t) % 4)), 32, []
    for _ in range(3):  # the team name first
        short = raw[at] == 0xFF
        at += short
        fields.append(raw[at + 1:at + 1 + raw[at]].decode())
        at += 1 + raw[at]
    return fields[1:]


def logged(who):
    p = os.path.join(D, who, "daemon.log")
    return open(p, errors="replace").read() if os.path.exists(p) else ""


os.makedirs(FAKE_BIN)
os.makedirs(os.path.join(D, "home"))
for name in ("open", "xdg-open", "osascript", "notify-send"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)

try:
    ep = {w: cli(w, "endpoint") for w in WHO}
    out = cli("alex", "init", "regions", "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
    check("alex starts a team", "You are alex on laptop." in out, out)
    check("…and keeps the list he was served", os.path.exists(os.path.join(D, "alex", "relays.json")), os.listdir(os.path.join(D, "alex")))
    out = cli("alex", "link")
    check("alex is homed on sfo1, the only relay his list has", link_relays(out) == ["", RELAY["sfo1"] + "/"], out)
    out = cli("bea", "join", re.search(r"https://\S+/join/#\S+", out).group(0), "--proof", proof("u_bea", "bea", ep["bea"]), "--no-setup")
    check("bea, who knows only fra1, joins through alex on sfo1", "Joined team regions as bea on desk." in out, out)
    out = until("bea", ["link"], lambda o: link_relays(o)[1] == RELAY["fra1"] + "/")
    check("bea is homed on fra1", link_relays(out) == ["", RELAY["fra1"] + "/"], out)
    out = until("alex", ["team"], lambda o: re.search(r"bea \(verified\)\s+desk\s+(direct|relay)", o))
    check("alex sees bea connected", re.search(r"bea \(verified\)\s+desk\s+(direct|relay)", out) is not None, out)
    out = until("bea", ["team"], lambda o: re.search(r"alex \(verified\)\s+laptop\s+(direct|relay)", o))
    check("bea sees alex connected", re.search(r"alex \(verified\)\s+laptop\s+(direct|relay)", out) is not None, out)
    cli("alex", "listen", "--as", "t", "--wait", "0")
    cli("bea", "listen", "--as", "t", "--wait", "0")
    out = until("alex", ["send", "bea/t", "--from", "alex/t", "hello from sfo1"], lambda o: "→ bea/t" in o and "no session" not in o)
    check("alex sends to bea", "→ bea/t" in out, out)
    out = cli("bea", "listen", "--as", "t", "--wait", "15")
    check("…who gets it", "hello from sfo1" in out, out)
    cli("bea", "send", "alex/t", "--from", "bea/t", "hello from fra1")
    out = cli("alex", "listen", "--as", "t", "--wait", "15")
    check("and back", "hello from fra1" in out, out)

    # A team's own relay wins: carl's list says fra1, his team says sin1.
    out = cli("carl", "init", "own", "--relay", RELAY["sin1"], "--proof", proof("u_carl", "carl", ep["carl"]), "--no-setup")
    check("carl starts a team on its own relay", "You are carl on box." in out, out)
    out = until("carl", ["link"], lambda o: link_relays(o)[1] != "")
    check("…which is where he is, not on his list's", link_relays(out) == [RELAY["sin1"], RELAY["sin1"] + "/"], out)
    check("no list was turned down", all("relays:" not in logged(w) for w in WHO), [logged(w)[-1500:] for w in WHO])
finally:
    for w in WHO:
        subprocess.run([BIN, "stop"], env=env(w), capture_output=True, timeout=30)
    server.shutdown()
log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
