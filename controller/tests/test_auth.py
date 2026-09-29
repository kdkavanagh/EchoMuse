"""
Session/role resolution against a real temporary database.
"""

import asyncio

import pytest

pytest.importorskip("bcrypt")

import em_auth as auth  # noqa: E402
import em_db as db  # noqa: E402


@pytest.fixture()
def fresh_db(tmp_path):
    db.init(str(tmp_path / "test.db"))
    yield db
    if db._conn is not None:
        db._conn.close()
        db._conn = None


def test_an_unknown_stored_role_fails_closed_to_readonly(fresh_db):
    # users.role is unconstrained TEXT: a hand-edited or foreign row must
    # downgrade the account, not lock it out with a 500 on every request.
    db.create_user("bob", auth.hash_password("pw-123456"), "operator")

    token, role = asyncio.run(auth.login("bob", "pw-123456"))
    assert role is auth.Role.READONLY

    user = asyncio.run(auth._session_user(token))
    assert user is not None and user["role"] is auth.Role.READONLY
