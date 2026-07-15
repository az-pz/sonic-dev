"""Daemon liveness — an explicit guard against stale-STATE_DB false passes.

The session `_clean_baseline` fixture already flushes TRANSCEIVER_*, restarts
xcvrd and requires repopulation (failing the whole suite otherwise). These give
that guarantee a first-class PASS/FAIL line, and assert the daemon is genuinely
alive rather than STATE_DB merely holding residue.
"""


def test_xcvrd_running(xcvrd):
    assert xcvrd.is_running(), f"xcvrd is not running: status={xcvrd.status()!r}"


def test_baseline_is_live(module, xcvrd):
    """After the flush+restart baseline the test port is populated by a live daemon."""
    assert xcvrd.is_running()
    module.wait_info_populated(timeout=60)
    assert module.info_manufacturer() == "xcvr-emu"
