"""Interaction-trace assertions (the "instrumentation" pillar).

Using the emulator's Monitor stream we observe the actual EEPROM reads/writes
xcvrd performs -- the black-box view of xcvrd <-> hardware interaction. These
assert that xcvrd genuinely polls the emulated module and reacts to a plug event
with a read burst, rather than serving cached/stale state.
"""
import time

import pytest

from lib.waits import eventually


def test_xcvrd_polls_module(monitor, module):
    """In steady state xcvrd keeps reading the module's EEPROM over the emulator."""
    module.wait_info_populated(timeout=60)
    monitor.clear()
    # xcvrd's DOM/CMIS loops should touch the module within a reasonable window.
    evs = eventually(lambda: monitor.reads(index=module.index) or None,
                     timeout=90, interval=2.0,
                     msg=f"xcvrd issued EEPROM reads for module {module.index}")
    assert len(evs) >= 1


def test_plug_triggers_read_burst(monitor, module):
    """A re-plug makes xcvrd re-read the module (identity/DOM), visible as a
    burst of reads on the Monitor stream for that index."""
    module.wait_info_populated(timeout=60)
    module.unplug()
    module.wait_info_cleared(timeout=60)

    monitor.clear()
    module.plug()
    # After insertion xcvrd re-reads the EEPROM to repopulate STATE_DB.
    reads = eventually(lambda: (monitor.reads(index=module.index)
                                if len(monitor.reads(index=module.index)) >= 3 else None),
                       timeout=90, interval=1.0,
                       msg=f"read burst for module {module.index} after re-plug")
    assert len(reads) >= 3
    module.wait_info_populated(timeout=60)


def test_reads_target_identity_page(monitor, module):
    """The reads xcvrd issues include lower-memory / page 00h (identity + module
    monitors) -- i.e. it is really decoding the module, not guessing."""
    module.wait_info_populated(timeout=60)
    monitor.clear()
    def _page0():
        p0 = [e for e in monitor.reads(index=module.index) if e.page == 0]
        return p0 or None
    p0 = eventually(_page0, timeout=90, interval=2.0,
                    msg=f"page 00h reads for module {module.index}")
    assert len(p0) >= 1
