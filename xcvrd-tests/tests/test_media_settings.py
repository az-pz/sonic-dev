"""NPU/ASIC-side media SerDes settings + NPU_SI_SETTINGS_SYNC_STATUS lifecycle (C20/C21).

Two closely-related, entirely unexercised behaviours:

  * C20 -- NPU/ASIC media SI settings. Distinct from the MODULE-side optics-SI
    (test_optics_si, page-10h writes): xcvrd reads media_settings.json and PUBLISHES
    the resolved ASIC-side SerDes attributes into APPL_DB PORT_TABLE for the port
    (media_settings_parser.notify_media_setting -> get_app_port_tbl().set), where the
    orchagent/NPU would consume them. A reduced daemon that ignores media_settings.json
    publishes nothing to APPL_DB.

  * C21 -- NPU_SI_SETTINGS_SYNC_STATUS lifecycle. xcvrd seeds STATE_DB
    PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS = NPU_SI_SETTINGS_DEFAULT at init
    (xcvrd.py:941-958) and, after it publishes the media SI to APPL_DB, stamps it
    NPU_SI_SETTINGS_NOTIFIED (media_settings_parser.py:636). The NOTIFIED value is the
    idempotency guard (is_npu_si_settings_update_required) so xcvrd publishes once per
    port until the status is reset. Never asserted before.

Both need a media_settings.json on the platform dir (xcvrd loads it once at startup),
provisioned as pure harness stimulus (lib/media_settings, mirroring lib/optics_si):
drop the file + restart xcvrd; teardown removes it + restarts so the session returns to
the stock no-media-settings baseline. The provisioned profile publishes a `preemphasis`
SI attribute for the XCVR-EMU vendor at the 40G/4-lane (10G/lane) key.

Targets an admin-up port (default Ethernet32) not used by the golden gates; override
with XCVRD_MEDIA_PORT.
"""
import os

import pytest

from lib import media_settings
from lib.emu import port_to_index, index_to_port
from lib.waits import wait_until, eventually, T_FAST, T_DOM, T_BASELINE

pytestmark = pytest.mark.slow

MEDIA_PORT = os.environ.get("XCVRD_MEDIA_PORT", "Ethernet32")
APPL_PORT_TABLE = "PORT_TABLE"          # APPL_DB PORT_TABLE (colon-separated)
STATE_PORT_TABLE = "PORT_TABLE"         # STATE_DB PORT_TABLE (pipe-separated)
SYNC_STATUS_KEY = "NPU_SI_SETTINGS_SYNC_STATUS"
DEFAULT_VALUE = "NPU_SI_SETTINGS_DEFAULT"
NOTIFIED_VALUE = "NPU_SI_SETTINGS_NOTIFIED"


def _appl(statedb_appl, port, field=media_settings.DB_ATTR):
    return statedb_appl.hget(f"{APPL_PORT_TABLE}:{port}", field)


def _sync_status(statedb, port):
    return statedb.hget(f"{STATE_PORT_TABLE}|{port}", SYNC_STATUS_KEY)


@pytest.fixture
def media_loaded(emu, statedb, configdb, xcvrd, test_index):
    """Provision media_settings.json + restart xcvrd so it loads it; restore the
    stock no-media-settings baseline (remove file + restart) on teardown."""
    port = MEDIA_PORT
    idx = port_to_index(port)
    if idx not in emu.list():
        pytest.skip(f"emulator has no module {idx} ({port})")
    if configdb.hget(f"PORT|{port}", "admin_status") != "up":
        pytest.skip(f"{port} is not admin-up; media SI is published for a brought-up port. "
                    "Set XCVRD_MEDIA_PORT to an admin-up port.")
    if not media_settings.sudo_available():
        pytest.skip("passwordless sudo required to provision media_settings.json")

    src = os.path.join(os.path.dirname(__file__), "data", media_settings.MS_FILENAME)
    try:
        media_settings.provision(src)
    except Exception as e:  # noqa: BLE001
        pytest.skip(f"could not provision media_settings.json: {e}")

    xcvrd.restart()
    baseline_port = index_to_port(test_index)
    if not xcvrd.wait_healthy(baseline_port, timeout=T_BASELINE):
        media_settings.deprovision()
        xcvrd.restart()
        pytest.fail("xcvrd not healthy after provisioning media_settings.json")

    yield port, idx

    media_settings.deprovision()
    xcvrd.restart()
    xcvrd.wait_healthy(baseline_port, timeout=T_BASELINE)


def test_media_settings_published_to_appl_db(media_loaded, statedb):
    """xcvrd resolves the ASIC-side media SI from media_settings.json and publishes it
    into APPL_DB PORT_TABLE for the port (C20)."""
    from lib.statedb import StateDB
    appl = StateDB("APPL_DB")
    port, _ = media_loaded

    val = eventually(lambda: _appl(appl, port), timeout=T_DOM,
                     msg=f"{port} APPL_DB PORT_TABLE.{media_settings.DB_ATTR} published from "
                         "media_settings.json")
    # The published value is the per-lane SI value (joined across the port's lanes).
    assert media_settings.LANE_VALUE.lower() in val.lower(), (
        f"{port}: APPL_DB {media_settings.DB_ATTR}={val!r} does not contain the provisioned "
        f"per-lane value {media_settings.LANE_VALUE} -- xcvrd did not publish the media SI")


def test_npu_si_sync_status_notified(media_loaded, statedb):
    """After publishing the media SI, xcvrd stamps NPU_SI_SETTINGS_SYNC_STATUS =
    NPU_SI_SETTINGS_NOTIFIED for the port (C21)."""
    port, _ = media_loaded
    wait_until(lambda: _sync_status(statedb, port) == NOTIFIED_VALUE, timeout=T_DOM,
               msg=f"{port} NPU_SI_SETTINGS_SYNC_STATUS -> NOTIFIED after media SI publish")


def test_npu_si_sync_status_default_to_notified_transition(media_loaded, statedb, emu):
    """The lifecycle is DEFAULT -> NOTIFIED: resetting the status to DEFAULT and
    re-triggering (re-plug) makes xcvrd re-publish and stamp NOTIFIED again -- proving
    xcvrd itself drives the transition, not a one-off at startup."""
    from lib.statedb import StateDB
    appl = StateDB("APPL_DB")
    port, idx = media_loaded

    # Start from the notified steady state.
    wait_until(lambda: _sync_status(statedb, port) == NOTIFIED_VALUE, timeout=T_DOM,
               msg=f"{port} NOTIFIED at baseline")

    # Reset the guard to DEFAULT and clear the published APPL_DB attr, then re-plug the
    # module so xcvrd re-runs notify_media_setting for the port.
    statedb.hset(f"{STATE_PORT_TABLE}|{port}", SYNC_STATUS_KEY, DEFAULT_VALUE)
    appl.hdel(f"{APPL_PORT_TABLE}:{port}", media_settings.DB_ATTR)
    wait_until(lambda: _sync_status(statedb, port) == DEFAULT_VALUE, timeout=T_FAST,
               msg=f"{port} reset to DEFAULT before re-trigger")

    emu.unplug(idx)
    wait_until(lambda: not statedb.hget(f"TRANSCEIVER_INFO|{port}", "manufacturer"),
               timeout=T_FAST, msg=f"{port} removed before re-insert")
    emu.plug(idx)

    # xcvrd re-publishes the media SI and re-stamps NOTIFIED.
    wait_until(lambda: _sync_status(statedb, port) == NOTIFIED_VALUE, timeout=T_DOM,
               msg=f"{port} NPU_SI_SETTINGS_SYNC_STATUS DEFAULT -> NOTIFIED on re-trigger")
    assert _appl(appl, port) is not None, (
        f"{port}: APPL_DB {media_settings.DB_ATTR} not re-published on the DEFAULT->NOTIFIED "
        "transition")
