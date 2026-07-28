"""DOM sensor behavior (HLD 1.1.2, 1.4).

xcvrd periodically reads the module-level DOM monitors (temperature, voltage)
from the transceiver and republishes them to TRANSCEIVER_DOM_SENSOR. We change
the raw CMIS monitor bytes in the emulator and assert the new value propagates
after a refresh cycle. The refresh interval is ~60s (HLD open question 1), so
the value-change tests are marked `slow`.
"""
import pytest

from lib import cmis
from lib.waits import wait_until, T_FAST, T_DOM


@pytest.mark.slow
def test_dom_table_present(module):
    module.wait_info_populated(timeout=T_FAST)
    # DOM_SENSOR is populated by DomInfoUpdateTask on its first poll, which lands
    # shortly AFTER TRANSCEIVER_INFO on a fresh (flushed) baseline -- wait for it.
    wait_until(lambda: "temperature" in module.dom(), timeout=T_DOM,
               msg=f"{module.port} TRANSCEIVER_DOM_SENSOR populated")
    dom = module.dom()
    assert "temperature" in dom
    assert "voltage" in dom


def _dom_temperature(module):
    val = module.dom().get("temperature")
    try:
        return float(val)
    except (TypeError, ValueError):
        return None


@pytest.mark.slow
def test_temperature_reflects_emulator(module):
    """Writing a raw temperature into the emulator shows up in DOM after refresh."""
    module.wait_info_populated(timeout=T_FAST)
    target_c = 42.5
    module.emu.write_field(module.index, cmis.TEMP, cmis.encode_temperature(target_c))
    # Sanity: the emulator now serves the value back.
    served = cmis.decode_temperature(module.emu.read_field(module.index, cmis.TEMP))
    assert abs(served - target_c) < 0.5

    wait_until(lambda: _dom_temperature(module) is not None
               and abs(_dom_temperature(module) - target_c) < 1.0,
               timeout=T_DOM,
               msg=f"{module.port} DOM temperature -> {target_c}C after refresh")


@pytest.mark.slow
def test_voltage_reflects_emulator(module):
    """Writing a raw supply voltage into the emulator shows up in DOM after refresh."""
    module.wait_info_populated(timeout=T_FAST)
    target_v = 3.30
    module.emu.write_field(module.index, cmis.VCC, cmis.encode_voltage(target_v))
    served = cmis.decode_voltage(module.emu.read_field(module.index, cmis.VCC))
    assert abs(served - target_v) < 0.01

    def _voltage():
        try:
            return float(module.dom().get("voltage"))
        except (TypeError, ValueError):
            return None

    wait_until(lambda: _voltage() is not None and abs(_voltage() - target_v) < 0.05,
               timeout=T_DOM,
               msg=f"{module.port} DOM voltage -> {target_v}V after refresh")


DOM_PER_LANE_FIELDS = ([f"tx{i}power" for i in range(1, 9)]
                       + [f"rx{i}power" for i in range(1, 9)]
                       + [f"tx{i}bias" for i in range(1, 9)])


@pytest.mark.slow
def test_dom_sensor_publishes_per_lane_fields(module):
    """xcvrd publishes the full per-lane DOM sensor field set (tx/rx power + tx bias
    for 8 lanes), not just module temperature/voltage.

    On this module the per-lane VALUES read 'N/A' -- its config does not advertise
    the page-11h media-lane optical-power/bias monitors (01h:160) -- so xcvrd, in
    parity with the module, emits N/A for them (a page-11h monitor write does NOT
    propagate). Real per-lane values would need the emulator to advertise + serve
    those monitors. This gate therefore locks the per-lane field STRUCTURE: a
    reduced daemon that publishes only temperature/voltage and omits the 24
    per-lane keys fails here.
    """
    module.wait_info_populated(timeout=T_FAST)
    wait_until(lambda: "temperature" in module.dom(), timeout=T_DOM,
               msg=f"{module.port} TRANSCEIVER_DOM_SENSOR populated")
    dom = module.dom()
    assert "temperature" in dom and "voltage" in dom
    missing = [f for f in DOM_PER_LANE_FIELDS if f not in dom]
    assert not missing, f"{module.port} DOM_SENSOR missing per-lane fields: {missing}"


# Expected decode of the page-02h thresholds cmis.write_dom_thresholds() writes:
# raw register -> engineering value (temp signed 1/256 C; vcc 100uV -> V). These
# mirror DOM_THRESHOLD_WRITES and let the test assert the decode math explicitly,
# independent of any captured golden.
DOM_THRESHOLD_EXPECTED = {
    "temphighalarm": 75.0, "templowalarm": -5.0,
    "temphighwarning": 70.0, "templowwarning": -10.0,
    "vcchighalarm": 3.6, "vcclowalarm": 3.0,
    "vcchighwarning": 3.5, "vcclowwarning": 3.1,
}


@pytest.mark.slow
def test_dom_thresholds_decode_written_values(module):
    """Writing the page-02h Module Thresholds and re-inserting the module makes
    xcvrd publish TRANSCEIVER_DOM_THRESHOLD with each register decoded to its
    engineering value (temp 1/256 C, vcc 100 uV). A standalone decode assertion
    that complements the steady_state golden without depending on golden capture.

    Thresholds are cached by xcvrd at insertion (not re-read on the DOM poll), so
    we write then re-plug to force a fresh read."""
    from lib import cmis
    module.wait_info_populated(timeout=T_FAST)
    cmis.write_dom_thresholds(module.emu, module.index)
    module.unplug()
    module.wait_info_cleared(timeout=T_FAST)
    module.plug()
    module.wait_info_populated(timeout=T_FAST)

    def _thr():
        return module.db.hgetall(f"TRANSCEIVER_DOM_THRESHOLD|{module.port}")

    sentinel = cmis.DOM_THRESHOLD_SENTINEL[0]
    wait_until(lambda: _thr().get(sentinel) is not None, timeout=T_DOM,
               msg=f"{module.port} DOM_THRESHOLD re-read after page-02h write")
    thr = _thr()
    for field, expected in DOM_THRESHOLD_EXPECTED.items():
        got = thr.get(field)
        assert got is not None, f"{module.port} DOM_THRESHOLD missing {field}"
        assert abs(float(got) - expected) < 0.05, \
            f"{module.port} {field}={got!r} decoded, expected ~{expected}"
