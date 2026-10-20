"""Reason sign-in proofs and relay lists for the e2e tests: a stand-in for Reason's signing key, trusted through
REASON_NETWORK_TRUST_KEYS (debug builds). RS256 (RSASSA-PKCS1-v1_5 with SHA-256) in plain Python, so the tests need
nothing installed: keys are made fresh for each run."""
import base64, hashlib, http.server, json, secrets, threading, time

# DER of the DigestInfo for SHA-256, which PKCS#1 v1.5 puts before the hash (RFC 8017, 9.2).
SHA256_INFO = bytes.fromhex("3031300d060960864801650304020105000420")


def _prime(bits):
    while True:
        n = secrets.randbits(bits) | (1 << (bits - 1)) | (1 << (bits - 2)) | 1
        if all(n % p for p in (3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37)) and _probably_prime(n):
            return n


def _probably_prime(n, rounds=40):
    d, r = n - 1, 0
    while d % 2 == 0:
        d, r = d // 2, r + 1
    for _ in range(rounds):
        x = pow(secrets.randbelow(n - 3) + 2, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(r - 1):
            x = pow(x, 2, n)
            if x == n - 1:
                break
        else:
            return False
    return True


def keygen(bits=2048):
    """An RSA key: (n, e, d)."""
    e = 65537
    while True:
        p, q = _prime(bits // 2), _prime(bits // 2)
        phi = (p - 1) * (q - 1)
        if p != q and phi % e and (p * q).bit_length() == bits:
            return p * q, e, pow(e, -1, phi)


def sign(key, msg):
    n, _, d = key
    k = (n.bit_length() + 7) // 8
    t = SHA256_INFO + hashlib.sha256(msg).digest()
    em = b"\x00\x01" + b"\xff" * (k - len(t) - 3) + b"\x00" + t
    return pow(int.from_bytes(em, "big"), d, n).to_bytes(k, "big")


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _int(i):
    return b64(i.to_bytes((i.bit_length() + 7) // 8, "big"))


REASON = keygen()  # stands in for Reason's signing key
KID = "e2e-1"
TRUST = json.dumps({"keys": [{"kty": "RSA", "kid": KID, "alg": "RS256", "use": "sig", "n": _int(REASON[0]), "e": _int(REASON[1])}]})


def proof(sub, name, endpoint, key=None, **claims):
    """A proof that `endpoint` belongs to account `sub`, called `name`; signed with another `key` it's a forgery."""
    now = int(time.time())
    body = {"iss": "https://reasonmachines.com/network", "aud": "reason-network", "sub": sub, "name": name,
            "email": f"{name}@example.com", "endpoint": endpoint, "iat": now, "exp": now + 90 * 86400, **claims}
    head = {"alg": "RS256", "kid": KID, "typ": "reason-network-proof"}
    signing = f"{b64(json.dumps(head).encode())}.{b64(json.dumps(body).encode())}"
    return f"{signing}.{b64(sign(key or REASON, signing.encode()))}"


RELAY = {r: f"https://relay-{r}.compute.reasonmachines.com" for r in ("sfo1", "iad1", "fra1", "sin1")}


def relay_list(regions):
    """relays.json as Reason serves it: the list, and the same list signed."""
    relays = [{"url": RELAY[r], "region": r} for r in regions]
    now = int(time.time())
    claims = {"iss": "https://reasonmachines.com/network/relays", "aud": "reason-network", "relays": relays,
              "iat": now, "exp": now + 30 * 86400}
    head = {"alg": "RS256", "kid": KID, "typ": "reason-network-relays"}
    signing = f"{b64(json.dumps(head).encode())}.{b64(json.dumps(claims).encode())}"
    return json.dumps({"relays": relays, "signed": f"{signing}.{b64(sign(REASON, signing.encode()))}"}).encode()


def serve_relay_lists(lists):
    """Serves `lists` ({name: regions}) at /<name>.json on a local port, for REASON_NETWORK_RELAYS_URL (debug builds),
    so tests never fetch Reason's real list; the server's base URL."""
    docs = {f"/{name}.json": relay_list(regions) for name, regions in lists.items()}

    class Lists(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            body = docs.get(self.path, b"")
            self.send_response(200 if body else 404)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Lists)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_port}"
