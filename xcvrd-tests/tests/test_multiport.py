"""Concurrent multi-port behavior.

Exercises presence, change events and DOM across several ports *at the same
time*. This specifically stresses per-module isolation in the emulator + bridge
+ xcvrd: simultaneous events on many ports must each be handled independently,
with no cross-talk or aliasing (the failure mode the emulator MemMap/EEPROM
fixes addressed).
"""
from concurrent.futures import ThreadPoolExecutor

import pytest

from lib import cmis
from lib.waits import wait_until, stays, T_FAST, T_MULTI, T_DOM


def _fan_out(fn, mods):
    """Invoke fn(module) for all modules concurrently (true simultaneity)."""
    with ThreadPoolExecutor(max_workers=len(mods)) as ex:
        list(ex.map(fn, mods))


def test_concurrent_unplug_then_replug(multiport):
    """Unplug all ports simultaneously -> all clear; replug all -> all restore."""
    for m in multiport:
        m.wait_info_populated(timeout=T_FAST)

    _fan_out(lambda m: m.unplug(), multiport)
    wait_until(lambda: all(not m.info_populated() for m in multiport), timeout=T_MULTI,
               msg="all ports cleared after concurrent unplug")

    _fan_out(lambda m: m.plug(), multiport)
    wait_until(lambda: all(m.info_populated() for m in multiport), timeout=T_MULTI,
               msg="all ports restored after concurrent replug")
    assert all(m.info_manufacturer() == "xcvr-emu" for m in multiport)


def test_partial_unplug_isolation(multiport):
    """Unplug half the ports; the other half must stay populated (no collateral)."""
    for m in multiport:
        m.wait_info_populated(timeout=T_FAST)
    half = len(multiport) // 2
    pulled, kept = multiport[:half], multiport[half:]

    _fan_out(lambda m: m.unplug(), pulled)
    wait_until(lambda: all(not m.info_populated() for m in pulled), timeout=T_MULTI,
               msg="pulled ports cleared")
    # The kept ports must remain populated throughout.
    assert stays(lambda: all(m.info_populated() for m in kept), duration=8.0), \
        "an un-pulled port was incorrectly cleared"

    _fan_out(lambda m: m.plug(), pulled)
    wait_until(lambda: all(m.info_populated() for m in pulled), timeout=T_MULTI,
               msg="pulled ports restored")


@pytest.mark.slow
def test_concurrent_dom_no_crosstalk(multiport):
    """Write a DISTINCT temperature to each port at once; each DOM must reflect
    its OWN value -> proves per-module isolation (no shared/aliased EEPROM)."""
    for m in multiport:
        m.wait_info_populated(timeout=T_FAST)

    # distinct target per port: 30.0, 35.0, 40.0, ...
    targets = {m.index: 30.0 + 5.0 * i for i, m in enumerate(multiport)}
    _fan_out(lambda m: m.emu.write_field(m.index, cmis.TEMP,
                                         cmis.encode_temperature(targets[m.index])),
             multiport)

    # emulator serves each its own value back (isolation at the emulator layer)
    for m in multiport:
        served = cmis.decode_temperature(m.emu.read_field(m.index, cmis.TEMP))
        assert abs(served - targets[m.index]) < 0.5, \
            f"{m.port}: emulator served {served} != {targets[m.index]}"

    def _all_match():
        for m in multiport:
            try:
                val = float(m.dom().get("temperature"))
            except (TypeError, ValueError):
                return False
            if abs(val - targets[m.index]) >= 1.0:
                return False
        return True

    wait_until(_all_match, timeout=T_DOM,
               msg="each port's DOM temperature matches its own written value")
