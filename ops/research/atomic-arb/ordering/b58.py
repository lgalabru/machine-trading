ALPHA = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
IDX = {c: i for i, c in enumerate(ALPHA)}

def b58decode(s):
    n = 0
    for c in s:
        n = n * 58 + IDX[c]
    b = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    pad = len(s) - len(s.lstrip("1"))
    return b"\x00" * pad + b

def b58encode(b):
    n = int.from_bytes(b, "big")
    s = ""
    while n:
        n, r = divmod(n, 58)
        s = ALPHA[r] + s
    pad = len(b) - len(b.lstrip(b"\x00"))
    return "1" * pad + s
