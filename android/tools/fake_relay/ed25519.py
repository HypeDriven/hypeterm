"""Minimal Ed25519 verification (RFC 8032, §5.1.7).

The fake relay has to check real signatures, otherwise the client's proof-of-possession
path would never be exercised end to end. Python's standard library has no Ed25519 and
the test environment has no third-party crypto packages, so this is the reference
algorithm written out. It is used only by the test server; the client itself signs with
OpenSSL.
"""

import hashlib

P = 2 ** 255 - 19
L = 2 ** 252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
I = pow(2, (P - 1) // 4, P)


def _sha512(data: bytes) -> bytes:
    return hashlib.sha512(data).digest()


def _x_recover(y: int) -> int:
    xx = (y * y - 1) * pow(D * y * y + 1, P - 2, P)
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P != 0:
        x = (x * I) % P
    if x % 2 != 0:
        x = P - x
    return x


BASE_Y = 4 * pow(5, P - 2, P) % P
BASE = (_x_recover(BASE_Y), BASE_Y, 1, _x_recover(BASE_Y) * BASE_Y % P)
IDENTITY = (0, 1, 1, 0)


def _add(p, q):
    x1, y1, z1, t1 = p
    x2, y2, z2, t2 = q
    a = (y1 - x1) * (y2 - x2) % P
    b = (y1 + x1) * (y2 + x2) % P
    c = t1 * 2 * D * t2 % P
    dd = z1 * 2 * z2 % P
    e, f, g, h = b - a, dd - c, dd + c, b + a
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _double(p):
    return _add(p, p)


def _scalar_multiply(p, e: int):
    result = IDENTITY
    while e > 0:
        if e & 1:
            result = _add(result, p)
        p = _double(p)
        e >>= 1
    return result


def _compress(p):
    x, y, z, _ = p
    inverse = pow(z, P - 2, P)
    x = x * inverse % P
    y = y * inverse % P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _decompress(data: bytes):
    if len(data) != 32:
        return None
    value = int.from_bytes(data, "little")
    sign = value >> 255
    y = value & ((1 << 255) - 1)
    if y >= P:
        return None
    x = _x_recover(y)
    if x & 1 != sign:
        x = P - x
    point = (x, y, 1, x * y % P)
    if not _on_curve(point):
        return None
    return point


def _on_curve(p) -> bool:
    x, y, z, t = p
    return (
        (-x * x + y * y - z * z - D * t * t) % P == 0
        and (x * y) % P == (z * t) % P
    )


def verify(public_key: bytes, message: bytes, signature: bytes) -> bool:
    """True when `signature` is a valid Ed25519 signature of `message`."""
    if len(signature) != 64 or len(public_key) != 32:
        return False
    a = _decompress(public_key)
    if a is None:
        return False
    r = _decompress(signature[:32])
    if r is None:
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= L:
        return False
    h = int.from_bytes(_sha512(signature[:32] + public_key + message), "little") % L
    left = _scalar_multiply(BASE, s)
    right = _add(r, _scalar_multiply(a, h))
    return _compress(left) == _compress(right)


def public_key_from_seed(seed: bytes) -> bytes:
    """Only used to build test fixtures, never by the relay itself."""
    h = bytearray(_sha512(seed))
    h[0] &= 248
    h[31] &= 127
    h[31] |= 64
    a = int.from_bytes(bytes(h[:32]), "little")
    return _compress(_scalar_multiply(BASE, a))
