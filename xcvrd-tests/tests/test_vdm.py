"""TRANSCEIVER_VDM_* threshold-table coverage (Versatile Diagnostics Monitoring).

xcvrd publishes the four VDM threshold tables -- HALARM / LALARM / HWARN / LWARN
-- each carrying the CMIS VDM observables (laser temperature, eSNR, PAM4 level
transition, pre-FEC BER, errored frames) per host/media lane. The reduced Rust
daemon publishes no VDM tables at all, so asserting the four tables + their
observable field set is a parity gate.

NOTE: the values read 'N/A' on this testbed because the module's config does not
advertise VDM support (vdm_supported=False) and the emulator serves no VDM pages
(20h-2Fh); real VDM readings / TRANSCEIVER_VDM_REAL_VALUE (which Python leaves
empty here) would need a VDM feature in the emulator. This gate therefore covers
table PUBLICATION + observable field structure, matching Python's current output.
"""
import pytest

from lib.waits import wait_until, T_DOM

pytestmark = pytest.mark.slow

VDM_THRESHOLD_TABLES = [
    "TRANSCEIVER_VDM_HALARM_THRESHOLD",
    "TRANSCEIVER_VDM_LALARM_THRESHOLD",
    "TRANSCEIVER_VDM_HWARN_THRESHOLD",
    "TRANSCEIVER_VDM_LWARN_THRESHOLD",
]

# One representative observable per CMIS VDM category (lane 1). A daemon that
# publishes an empty or partial VDM table fails these.
VDM_SAMPLE_FIELDS = [
    "laser_temperature_media1",
    "esnr_host_input1",
    "esnr_media_input1",
    "pam4_level_transition_host_input1",
    "prefec_ber_curr_media_input1",
    "errored_frames_curr_host_input1",
]


def _tables(module):
    return {t: module.db.hgetall(f"{t}|{module.port}") for t in VDM_THRESHOLD_TABLES}


def test_vdm_threshold_tables_published(module):
    """xcvrd publishes all four VDM threshold tables, non-empty.

    A daemon that never publishes VDM tables (the reduced Rust) fails here.
    """
    module.plug()
    wait_until(lambda: all(_tables(module).values()), timeout=T_DOM,
               msg=f"{module.port} all four VDM threshold tables published")
    for t, row in _tables(module).items():
        assert row, f"{t}|{module.port} is empty"


def test_vdm_threshold_observable_fields(module):
    """Each VDM threshold table carries the CMIS VDM observable fields, and all
    four describe the same observables (identical field sets)."""
    module.plug()
    wait_until(lambda: all(_tables(module).values()), timeout=T_DOM,
               msg=f"{module.port} VDM threshold tables populated")
    tables = _tables(module)
    for t, row in tables.items():
        for f in VDM_SAMPLE_FIELDS:
            assert f in row, \
                f"{f} missing from {t}: present sample = {[x for x in VDM_SAMPLE_FIELDS if x in row]}"
    fieldsets = {t: frozenset(r) - {"last_update_time"} for t, r in tables.items()}
    ref = next(iter(fieldsets.values()))
    for t, fs in fieldsets.items():
        assert fs == ref, f"{t} field set differs from the other VDM threshold tables"
