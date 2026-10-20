"""Reason sign-in proofs for the e2e tests: a stand-in for Reason's signing key, trusted through
REASON_NETWORK_TRUST_KEYS. Ed25519 from RFC 8032's reference code, so the tests need nothing installed."""
import base64, hashlib, json, os, time

p = 2**255 - 19
d = -121665 * pow(121666, p - 2, p) % p
q = 2**252 + 27742317777372353535851937790883648493


def _add(P, Q):
    A, B = (P[1] - P[0]) * (Q[1] - Q[0]) % p, (P[1] + P[0]) * (Q[1] + Q[0]) % p
    C, D = 2 * P[3] * Q[3] * d % p, 2 * P[2] * Q[2] % p
    E, F, G, H = B - A, D - C, D + C, B + A
    return (E * F, G * H, F * G, E * H)


def _mul(s, P):
    Q = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            Q = _add(Q, P)
        P, s = _add(P, P), s >> 1
    return Q


def _compress(P):
    zinv = pow(P[2], p - 2, p)
    x, y = P[0] * zinv % p, P[1] * zinv % p
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _base():
    y = 4 * pow(5, p - 2, p) % p
    x2 = (y * y - 1) * pow(d * y * y + 1, p - 2, p)
    x = pow(x2, (p + 3) // 8, p)
    if (x * x - x2) % p:
        x = x * pow(2, (p - 1) // 4, p) % p
    if x & 1:
        x = p - x
    return (x, y, 1, x * y % p)


G = _base()


def _expand(seed):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return a, h[32:]


def public(seed):
    return _compress(_mul(_expand(seed)[0], G))


def sign(seed, msg):
    a, prefix = _expand(seed)
    A = _compress(_mul(a, G))
    r = int.from_bytes(hashlib.sha512(prefix + msg).digest(), "little") % q
    R = _compress(_mul(r, G))
    h = int.from_bytes(hashlib.sha512(R + A + msg).digest(), "little") % q
    return R + int.to_bytes((r + h * a) % q, 32, "little")


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


REASON = os.urandom(32)  # stands in for Reason's signing key
KID = "e2e-1"
TRUST = json.dumps({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": KID, "x": b64(public(REASON))}]})


def proof(sub, name, endpoint, key=None, **claims):
    """A proof that `endpoint` belongs to account `sub`, called `name`; signed with `key` it's a forgery."""
    now = int(time.time())
    body = {"iss": "https://reasonmachines.com", "aud": "reason-network", "sub": sub, "name": name,
            "email": f"{name}@example.com", "endpoint": endpoint, "iat": now, "exp": now + 90 * 86400, **claims}
    head = {"alg": "EdDSA", "kid": KID, "typ": "reason-network-proof"}
    signing = f"{b64(json.dumps(head).encode())}.{b64(json.dumps(body).encode())}"
    return f"{signing}.{b64(sign(key or REASON, signing.encode()))}"
