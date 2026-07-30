"""Provision media_settings.json on the DUT for the NPU/ASIC-side SI test (C20/C21).

Distinct from optics_si_settings.json (which xcvrd applies to the MODULE via page-10h
writes): media_settings.json drives the NPU/ASIC-side SerDes settings. xcvrd's
notify_media_setting() reads it and PUBLISHES the resolved SI attributes into APPL_DB
PORT_TABLE for the port, then stamps STATE_DB PORT_TABLE.NPU_SI_SETTINGS_SYNC_STATUS
= NPU_SI_SETTINGS_NOTIFIED (media_settings_parser.py:554-638).

xcvrd loads media_settings.json ONCE at startup from the platform/HWSKU dir. Inside
pmon that dir is a read-only mount whose host source is
/usr/share/sonic/device/<platform>/, so (like lib/optics_si) we drop the file there
(needs sudo) and restart xcvrd; teardown removes it and restarts so the session
returns to the no-media-settings baseline. Pure harness stimulus: it just stages a
config file the platform would normally ship -- no xcvrd or emulator change.
"""
import subprocess

PMON = "pmon"
MS_FILENAME = "media_settings.json"

# The APPL_DB PORT_TABLE field notify_media_setting publishes for the provisioned
# profile below (the ASIC-side pre-emphasis SerDes attribute).
DB_ATTR = "preemphasis"
# Per-lane value we provision; the published APPL_DB value is these joined per lane.
LANE_VALUE = "0x16440A"


def _platform():
    out = subprocess.run(
        ["docker", "exec", PMON, "sonic-cfggen", "-d", "-v",
         "DEVICE_METADATA.localhost.platform"],
        capture_output=True, text=True, timeout=30)
    return out.stdout.strip()


def platform_ms_path():
    """Host path of media_settings.json (source of pmon's read-only mount)."""
    return f"/usr/share/sonic/device/{_platform()}/{MS_FILENAME}"


def sudo_available():
    r = subprocess.run(["sudo", "-n", "true"], capture_output=True, timeout=15)
    return r.returncode == 0


def provision(src_json):
    """sudo-copy the media settings file into the platform dir; return the dest path."""
    dest = platform_ms_path()
    subprocess.run(["sudo", "cp", src_json, dest], check=True, timeout=30)
    subprocess.run(["sudo", "chmod", "644", dest], check=True, timeout=30)
    return dest


def deprovision():
    """Remove the media settings file (idempotent; best-effort)."""
    try:
        subprocess.run(["sudo", "rm", "-f", platform_ms_path()], timeout=30)
    except Exception:  # noqa: BLE001
        pass
