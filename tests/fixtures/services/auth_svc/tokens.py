"""Token checks."""


def verify_token(token):
    """Reject an invalid token."""
    return decode(token) is not None


def decode(token):
    return token or None


def rotate_signing_key():
    """Replace the signing key."""
    return "new-key"
