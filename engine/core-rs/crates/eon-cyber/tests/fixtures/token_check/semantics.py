import sys
import types


assert sys.implementation.name == "cpython" and sys.flags.isolated
assert isinstance(hmac.compare_digest, types.BuiltinFunctionType)
assert hmac.compare_digest.__module__ in {"_hashlib", "_operator"}


class BytesSubclass(bytes):
    pass


def check(a, b):
    permitted = type(a) is bytes and type(b) is bytes and len(a) == len(b) == 32
    assert verify_auth_token(a, b) is (permitted and a == b)


for token in [bytes(32), bytes(range(32)), bytes([255]) * 32]:
    check(token, bytes(bytearray(token)))
    for i in range(32):
        different = bytearray(token)
        different[i] ^= 1
        check(token, bytes(different))
        check(bytes(different), token)
    for other in [b"", bytes(31), bytes(33), "x" * 32, bytearray(token),
                  memoryview(token), BytesSubclass(token), None]:
        check(token, other)
        check(other, token)
        check(other, other)
