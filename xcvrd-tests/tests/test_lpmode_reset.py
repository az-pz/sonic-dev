"""lpmode / reset control-plane (write-trace assertions).

The operator commands `sfputil reset` and `sfputil lpmode` drive the module
through the host sonic_platform bridge, which translates them into CMIS
ModuleGlobalControls (00h:26) writes to the emulator. We assert the exact
register write appears on the Monitor stream:
  - reset       -> 00h:26.3 SoftwareReset bit (0x08)
  - lpmode on   -> 00h:26.4 LowPwrRequestSW bit (0x10)
"""
from lib import cmis
from lib.waits import eventually


def _mgc_writes(monitor, index):
    """Writes that touch ModuleGlobalControls (00h:26) for a module, as the
    byte value written at offset 26."""
    out = []
    for e in monitor.writes(index=index, page=cmis.MGC_PAGE):
        if e.offset <= cmis.MGC_OFFSET < e.offset + e.length:
            out.append(e.data[cmis.MGC_OFFSET - e.offset])
    return out


def test_reset_writes_software_reset_bit(monitor, module, sfp_control):
    module.wait_info_populated(timeout=60)
    monitor.clear()
    rc = sfp_control.reset(module.port)
    assert rc.returncode == 0, f"sfputil reset failed: {rc.stderr or rc.stdout}"

    vals = eventually(lambda: _mgc_writes(monitor, module.index) or None,
                      timeout=30, interval=1.0,
                      msg=f"ModuleGlobalControls write for {module.port} on reset")
    assert any(v & cmis.SOFTWARE_RESET_BIT for v in vals), \
        f"no SoftwareReset bit (0x08) in MGC writes {[hex(v) for v in vals]}"


def test_lpmode_on_writes_lowpwr_bit(monitor, module, sfp_control):
    module.wait_info_populated(timeout=60)
    monitor.clear()
    rc = sfp_control.lpmode(module.port, on=True)
    assert rc.returncode == 0, f"sfputil lpmode on failed: {rc.stderr or rc.stdout}"

    vals = eventually(lambda: _mgc_writes(monitor, module.index) or None,
                      timeout=30, interval=1.0,
                      msg=f"ModuleGlobalControls write for {module.port} on lpmode on")
    assert any(v & cmis.LOW_PWR_REQUEST_BIT for v in vals), \
        f"no LowPwrRequestSW bit (0x10) in MGC writes {[hex(v) for v in vals]}"


def test_lpmode_reported_on_then_off(module, sfp_control):
    """The lpmode state round-trips through sfputil show."""
    assert sfp_control.lpmode(module.port, on=True).returncode == 0
    assert sfp_control.show_lpmode(module.port) == "On"
    assert sfp_control.lpmode(module.port, on=False).returncode == 0
    assert sfp_control.show_lpmode(module.port) == "Off"
