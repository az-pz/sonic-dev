"""Polling helpers for black-box assertions.

Black-box tests observe an eventually-consistent system (xcvrd reacts to the
emulator asynchronously, DOM refreshes on a timer), so almost every assertion is
"within N seconds, X becomes true". These helpers make that explicit and give
useful failure messages instead of a bare timeout.
"""
import time

# --- calibrated timeouts (seconds) ------------------------------------------
# Measured against the reference xcvrd on the KVM testbed with
# tools/measure_timeouts.py. STATE_DB reactions to presence/info/status/cmis/
# error settle in <4s; the DOM sensor and steady-state EEPROM reads are paced by
# xcvrd's ~60s poll cadence. Each tier sits a few x above the observed real max
# so a correct-but-slow xcvrd still passes, while a broken one fails quickly
# instead of burning 60-120s.
T_FAST = 15.0      # presence/info populate+clear, status 0/1, cmis READY, error set/clear, DOM removal (real max ~3.3s)
T_MULTI = 25.0     # a fast reaction aggregated across several ports at once
T_BURST = 25.0     # plug-triggered identity re-read burst; sfputil reset/lpmode monitor capture
T_DOM = 80.0       # DOM sensor appear/refresh/restore + steady-state EEPROM read cadence (~60s poll, real max ~59s)
T_BASELINE = 30.0  # flush TRANSCEIVER_* + restart xcvrd + repopulate INFO

# Poll cadence for every wait. STATE_DB probes shell out to sonic-db-cli
# (~75ms), so 0.5s keeps the duty cycle low while still detecting a change
# within ~0.5s -- the old 2-3s intervals added that much latency to every
# DOM / multi-port wait for no benefit. One knob for all waits.
POLL = 0.5


class WaitTimeout(AssertionError):
    pass


def eventually(fn, timeout=30.0, interval=POLL, msg=None):
    """Poll ``fn`` until it returns a truthy value, then return that value.

    Raises WaitTimeout (an AssertionError, so pytest reports it as a failure)
    if ``timeout`` seconds elapse first. ``fn`` should be cheap and side-effect
    free; the last value / exception is included in the failure message.
    """
    deadline = time.time() + timeout
    last = None
    last_exc = None
    while time.time() < deadline:
        try:
            last = fn()
            if last:
                return last
        except Exception as e:  # noqa: BLE001 - surfaced in the failure message
            last_exc = e
        time.sleep(interval)
    detail = msg or getattr(fn, "__name__", "condition")
    tail = f" (last={last!r})" if last_exc is None else f" (last exception={last_exc!r})"
    raise WaitTimeout(f"timed out after {timeout:.0f}s waiting for {detail}{tail}")


def wait_until(predicate, timeout=30.0, interval=POLL, msg=None):
    """Like eventually() but for a boolean predicate; returns True or raises."""
    return bool(eventually(lambda: bool(predicate()), timeout, interval, msg))


def stays(predicate, duration=5.0, interval=POLL):
    """Return True iff ``predicate`` stays truthy for the whole ``duration``.

    Used to assert a *non-event* — e.g. a port stays absent and is NOT
    re-added by a background task within the window.
    """
    deadline = time.time() + duration
    while time.time() < deadline:
        if not predicate():
            return False
        time.sleep(interval)
    return True
