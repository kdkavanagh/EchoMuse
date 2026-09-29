"""
The shell-plane rendezvous (em_shell.ShellBroker).

The interactive terminal used to unregister the device's pending session
unconditionally when it closed. An OTA that had registered its programmatic
session after the terminal opened (the terminal does not take the shell lock)
then had its request removed underneath it: the device's dial-back found
nothing, was closed, and the transfer failed on a 15 s timeout.
"""

import asyncio

import em_shell


def run(coro):
    return asyncio.run(coro)


def test_a_finished_session_does_not_unregister_a_newer_request():
    async def scenario():
        broker = em_shell.ShellBroker()
        terminal = broker.request("dev")
        ota = broker.request("dev")
        terminal.end()
        broker.release("dev", terminal)
        assert broker.claim("dev") is ota, "the OTA's request must still be answerable"
        broker.release("dev", ota)
        assert broker.claim("dev") is None
    run(scenario())


def test_a_dial_after_the_requester_gave_up_is_refused():
    """A requester that timed out must not be handed a socket nobody reads."""
    async def scenario():
        broker = em_shell.ShellBroker()
        req = broker.request("dev")
        try:
            await req.wait(timeout=0.01)
        except asyncio.TimeoutError:
            pass
        assert broker.claim("dev") is None
    run(scenario())
