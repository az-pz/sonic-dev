"""Transceiver error events (HLD 1.3.1, 1.4.2).

An injected error event surfaces via the bridge's get_change_event exactly as a
real platform would report a hardware error. xcvrd must:
  - set TRANSCEIVER_STATUS_SW.error to the decoded description(s),
  - remove DOM info for a BLOCKING error (EEPROM unreadable), while keeping the
    static TRANSCEIVER_INFO,
  - keep DOM for a non-blocking error,
  - clear the error and repopulate when the port recovers (a plug-in event).
"""
import pytest

from lib import errors
from lib.waits import wait_until, stays, T_FAST, T_DOM


def _error(module):
    return module.status_sw().get("error") or ""


def _dom_present(module):
    return "temperature" in module.dom()


@pytest.mark.slow
def test_blocking_error_sets_status_and_removes_dom(module, inject):
    """I2C-stuck (blocking) -> STATUS_SW.error set, DOM removed, INFO kept."""
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: _dom_present(module), timeout=T_DOM,
               msg=f"{module.port} DOM present before error")

    inject.set(module.index, errors.I2C_STUCK_EVENT)

    wait_until(lambda: "Bus stuck" in _error(module), timeout=T_FAST,
               msg=f"{module.port} STATUS_SW.error reflects I2C stuck")
    err = _error(module)
    assert "Blocking EEPROM from being read" in err
    assert "Bus stuck (I2C data or clock shorted)" in err

    # Blocking error removes DOM, but static INFO is retained.
    wait_until(lambda: not _dom_present(module), timeout=T_FAST,
               msg=f"{module.port} DOM removed on blocking error")
    assert module.info_manufacturer() == "xcvr-emu"

    # Recover: clearing the injection is a plug-in event -> error cleared, DOM back.
    inject.clear(module.index)
    wait_until(lambda: "Bus stuck" not in _error(module), timeout=T_FAST,
               msg=f"{module.port} error cleared on recovery")
    wait_until(lambda: _dom_present(module), timeout=T_DOM,
               msg=f"{module.port} DOM repopulated after recovery")


@pytest.mark.slow
def test_bad_eeprom_blocking_error(module, inject):
    """Bad-EEPROM (blocking) -> error set + DOM removed."""
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: _dom_present(module), timeout=T_DOM)

    inject.set(module.index, errors.BAD_EEPROM_EVENT)
    wait_until(lambda: "Bad or unsupported EEPROM" in _error(module), timeout=T_FAST,
               msg=f"{module.port} STATUS_SW.error reflects bad EEPROM")
    assert "Blocking EEPROM from being read" in _error(module)
    wait_until(lambda: not _dom_present(module), timeout=T_FAST,
               msg=f"{module.port} DOM removed on blocking error")

    inject.clear(module.index)
    wait_until(lambda: "EEPROM" not in _error(module) or _error(module) in ("", "N/A"),
               timeout=T_FAST, msg=f"{module.port} error cleared on recovery")


@pytest.mark.slow
def test_nonblocking_error_keeps_dom(module, inject):
    """High-temperature (non-blocking) -> error set but DOM retained."""
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: _dom_present(module), timeout=T_DOM)

    inject.set(module.index, errors.HIGH_TEMP_EVENT)
    wait_until(lambda: "High temperature" in _error(module), timeout=T_FAST,
               msg=f"{module.port} STATUS_SW.error reflects high temperature")
    # Non-blocking: DOM must NOT be removed.
    assert stays(lambda: _dom_present(module), duration=8.0), \
        f"{module.port} DOM was removed on a non-blocking error"
    assert "Blocking EEPROM from being read" not in _error(module)

    inject.clear(module.index)
    wait_until(lambda: "High temperature" not in _error(module), timeout=T_FAST,
               msg=f"{module.port} error cleared on recovery")
