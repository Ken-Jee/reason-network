"""This build and an older one in one team: sign-in proofs and rn1 links leave older machines working.

    cargo build && REASON_NETWORK_OLD_BIN=<an older reason-network, e.g. 0.10.8> \
        env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/mixed_versions.py

Needs internet (iroh relays/discovery).
"""
import json, os, re, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import TRUST, proof

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NEW = os.path.join(ROOT, "target", "debug", "reason-network")
OLD = os.environ["REASON_NETWORK_OLD_BIN"]
D = tempfile.mkdtemp(prefix="reason-network-mixed-")
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
# alex and nina run this build, olga the older one.
WHO = {"alex": (NEW, "laptop"), "olga": (OLD, "oldbox"), "nina": (NEW, "desk"), "mal": (NEW, "evil")}
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who):
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    e.update(HOME=os.path.join(D, "home"), PATH=FAKE_BIN + os.pathsep + os.environ["PATH"], CLAUDE_CONFIG_DIR=os.path.join(D, "claude"),
             REASON_NETWORK_HOME=os.path.join(D, who), REASON_NETWORK_MACHINE=WHO[who][1], REASON_NETWORK_NO_MDNS="1",
             REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
             REASON_NETWORK_TRUST_KEYS=TRUST)
    return e


def cli(who, *args):
    r = subprocess.run([WHO[who][0], *args], env=env(who), capture_output=True, text=True, timeout=90)
    return (r.stdout + r.stderr).strip()


def link_of(out):
    m = re.search(r"https://\S+/join/#\S+", out)
    return m.group(0) if m else ""


os.makedirs(FAKE_BIN)
os.makedirs(os.path.join(D, "home"))
for name in ("open", "xdg-open", "osascript", "notify-send"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)

try:
    print(f"this build: {cli('alex', '--version')} · older: {cli('olga', '--version')}")
    ep = {w: cli(w, "endpoint") for w in ("alex", "nina", "mal")}
    out = cli("alex", "init", "mixed", "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
    check("this build starts a team, signed in", "You are alex on laptop." in out, out)
    link = link_of(cli("alex", "link"))
    out = cli("olga", "join", link, "--as", "olga", "--no-setup")
    check("the older build can't read an rn1 link", "Joined" not in out, out)
    out = cli("olga", "join", link.replace("#rn1-", "#tincan1"), "--as", "olga", "--no-setup")
    check("…but joins with the same link as tincan1", "Joined team mixed as olga on oldbox." in out, out)
    time.sleep(5)
    out = cli("alex", "team")
    check("the older machine is connected, and alex still verified after its state came back",
          re.search(r"olga\s+oldbox\s+(direct|relay)", out) is not None and "alex (verified)" in out, out)
    out = cli("olga", "team")
    check("the older machine sees alex, connected", re.search(r"alex\s+laptop\s+(direct|relay)", out) is not None, out)

    # A machine of this build joins through the older one, which knows nothing of proofs.
    old_link = link_of(cli("olga", "link"))
    out = cli("nina", "join", old_link, "--proof", proof("u_nina", "nina", ep["nina"]), "--no-setup")
    check("this build joins, signed in, through the older machine", "Joined team mixed as nina on desk." in out, out)
    check("…and its proof goes along anyway", "nina (verified)" in out, out)
    time.sleep(6)
    out = cli("alex", "team")
    check("alex's machine sees nina verified", re.search(r"nina \(verified\)\s+desk\s+(direct|relay)", out) is not None, out)
    out = cli("olga", "team")
    check("the older machine has all three, every signed entry checking out",
          all(re.search(rf"{p}\s+{m}\s+(direct|relay|this machine)", out) for p, m in [("alex", "laptop"), ("olga", "oldbox"), ("nina", "desk")]), out)
    out = subprocess.run([NEW, "join", old_link, "--proof", proof("u_mal", "alex", ep["mal"]), "--no-setup", "--json"],
                         env=env("mal"), capture_output=True, text=True, timeout=90).stdout.strip()
    check("a taken name through the older machine is name_taken", out == '{"error":"name_taken"}', out)

    # Messages both ways, and the state kept across restarts.
    cli("olga", "listen", "--as", "t", "--wait", "0")
    cli("alex", "listen", "--as", "t", "--wait", "0")
    time.sleep(3)
    out = cli("alex", "send", "olga/t", "--from", "alex/t", "hello old friend")
    check("this build sends to the older one", "→ olga/t" in out, out)
    out = cli("olga", "listen", "--as", "t", "--wait", "10")
    check("…which gets it", "hello old friend" in out, out)
    cli("olga", "send", "alex/t", "--from", "olga/t", "hello back")
    out = cli("alex", "listen", "--as", "t", "--wait", "10")
    check("and back", "hello back" in out, out)
    for w in ("alex", "olga"):
        cli(w, "stop")
    time.sleep(1)
    cli("olga", "team")
    cli("alex", "team")
    time.sleep(8)
    out = cli("alex", "team")
    check("after both restart, people stay verified", "alex (verified)" in out and "nina (verified)" in out
          and re.search(r"olga\s+oldbox\s+(direct|relay)", out) is not None, out)
finally:
    for w in WHO:
        subprocess.run([WHO[w][0], "stop"], env=env(w), capture_output=True, timeout=30)
log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
