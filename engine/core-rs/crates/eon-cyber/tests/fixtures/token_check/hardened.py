import hmac


def verify_auth_token(presented: bytes, expected: bytes) -> bool:
    if type(presented) is not bytes or type(expected) is not bytes:
        return False
    if len(presented) != 32 or len(expected) != 32:
        return False
    return hmac.compare_digest(presented, expected)
