"""
The device-link auth decision, as a pure function.

Split out of the controller's link-auth check so it can be tested. Its callers
(`em_device.register_device` for the device link,
`em_controller._shell_auth_ok` for the shell plane) add a websocket header
read, a DB lookup and a log call; the part worth getting right is the four-way
decision below, and it was previously unreachable from the test suite because
em_controller pulls in the whole controller runtime.

It cost an orphaned device to find out. Deleting a device removed its row, and
the token is a column on that row, so `expected` became None while the device
carried on presenting the credential it still had on disk. The rule rejected
that, on every plane including the shell plane the controller would
otherwise have used to push a fresh credential, and the device retried forever
behind a pulsing orange ring.
"""

import hmac
from typing import NamedTuple, Optional


class Verdict(NamedTuple):
    ok: bool
    # Why, for the log. None when there is nothing worth saying.
    reason: Optional[str] = None
    # True when the connection is allowed but carried a credential nothing
    # recognises — worth a log line, since it is almost always a device that
    # was deleted and has come back.
    stale_token: bool = False


def decide(
    *,
    presented: Optional[str],
    expected: Optional[str],
    secure: bool,
    require_tls: bool,
) -> Verdict:
    """
    Whether to admit a device connection.

    `presented` is the X-EM-Token header, `expected` the stored token for this
    device id, `secure` whether the connection arrived over TLS.

    Four rules:

    1. A presented token that MISMATCHES a stored one always rejects. This is
       the only case where a credential is actually wrong.

    2. A stored token with NOTHING presented is allowed unless `require_tls`.
       The DB row is minted before the files reach the device, and rejecting
       in that window would cut off the shell plane that the credential push
       itself rides on.

    3. A token presented for a device with nothing on record is IGNORED, not
       rejected. Rule 2 already admits a connection presenting no token at
       all, so rejecting an unrecognised one treats it as worse than none
       while an attacker can simply omit the header: it buys nothing, and it
       is what made deleting a device a one-way door. Such a device comes back
       as pending and waits for approval, which is a human decision anyway.

    4. `require_tls` demands TLS AND a token matching a stored one, full stop.
       A deleted device is still refused there, and re-provisioning over USB
       stays the intended path.
    """
    if presented and expected and not hmac.compare_digest(presented, expected):
        return Verdict(False, "token mismatch")

    if require_tls and not (secure and presented and expected):
        return Verdict(False,
                       "plain connection" if not secure else "missing a valid token")

    if presented and not expected:
        return Verdict(True, "token presented but none on record", stale_token=True)

    return Verdict(True)
