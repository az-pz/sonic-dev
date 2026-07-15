"""sfputil control-plane stimulus (lpmode / reset).

These are the real operator commands. On the emulated platform they go through
the host sonic_platform bridge -> emulator EEPROM writes (ModuleGlobalControls,
CMIS 00h:26), which the Monitor stream captures. sfputil needs root, so we shell
out via sudo (passwordless on the DUT).
"""
import subprocess


def _run(args, timeout=60):
    return subprocess.run(["sudo", "sfputil", *args],
                          capture_output=True, text=True, timeout=timeout)


def reset(port):
    """Momentary CMIS SoftwareReset of a port (writes 00h:26.3)."""
    return _run(["reset", port])


def lpmode(port, on):
    """Enable/disable low-power mode (writes/clears 00h:26.4)."""
    return _run(["lpmode", "on" if on else "off", port])


def show_lpmode(port):
    out = _run(["show", "lpmode"])
    for line in out.stdout.splitlines():
        if line.split()[:1] == [port]:
            return line.split()[-1]  # "On" / "Off"
    return None
