"""Provision optics_si_settings.json on the DUT for the SI-application test.

xcvrd loads ``optics_si_settings.json`` ONCE at startup, from the platform/HWSKU
directory. Inside pmon that directory is a **read-only** mount whose source on
the DUT host is ``/usr/share/sonic/device/<platform>/``. To make xcvrd apply
media/optics SI settings we drop the file into that host dir (needs ``sudo``) and
restart xcvrd so it reloads; teardown removes the file and restarts again so the
session returns to the no-SI baseline.

This is test-only setup that lives entirely outside xcvrd and the emulator image
(it just stages a config file the platform would normally ship), keeping the
SI-application gate a pure harness stimulus.
"""
import subprocess

PMON = "pmon"
SI_FILENAME = "optics_si_settings.json"


def _platform():
    out = subprocess.run(
        ["docker", "exec", PMON, "sonic-cfggen", "-d", "-v",
         "DEVICE_METADATA.localhost.platform"],
        capture_output=True, text=True, timeout=30)
    return out.stdout.strip()


def platform_si_path():
    """Host path of optics_si_settings.json (source of pmon's read-only mount)."""
    return f"/usr/share/sonic/device/{_platform()}/{SI_FILENAME}"


def sudo_available():
    """True iff passwordless sudo works (the provision needs it)."""
    r = subprocess.run(["sudo", "-n", "true"], capture_output=True, timeout=15)
    return r.returncode == 0


def provision(src_json):
    """sudo-copy the SI settings file into the platform dir; return the dest path."""
    dest = platform_si_path()
    subprocess.run(["sudo", "cp", src_json, dest], check=True, timeout=30)
    subprocess.run(["sudo", "chmod", "644", dest], check=True, timeout=30)
    return dest


def deprovision():
    """Remove the SI settings file (idempotent; best-effort)."""
    try:
        subprocess.run(["sudo", "rm", "-f", platform_si_path()], timeout=30)
    except Exception:  # noqa: BLE001
        pass
