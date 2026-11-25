import hmac


def verify_auth_token(presented: bytes, expected: bytes) -> bool:
    if type(presented) is not bytes or type(expected) is not bytes:
        return False
    if len(presented) != 32 or len(expected) != 32:
        return False
    for i in range(32):
        if presented[i] != expected[i]:
            return False
    return True
