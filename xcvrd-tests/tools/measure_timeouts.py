"""Measure real-xcvrd reaction times for each black-box transition, so the test
timeouts in lib/waits.py can be calibrated (comfortably above the real max, but
not 60-120s).

Run on the DUT from the suite dir:  python3 tools/measure_timeouts.py
It drives the emulator and reads STATE_DB/Monitor exactly like the tests do, then
prints a min/mean/max table per transition. Use the maxima to set the T_* tiers.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from lib.emu import EmulatorClient, port_to_index  # noqa: E402
from lib.statedb import StateDB  # noqa: E402
from lib.inject import ErrorInjector  # noqa: E402
from lib.monitor import MonitorRecorder  # noqa: E402
from lib.xcvrd_ctl import XcvrdControl  # noqa: E402
from lib import cmis, errors  # noqa: E402

PORT = os.environ.get("XCVRD_TEST_PORT", "Ethernet100")
IDX = port_to_index(PORT)
db = StateDB("STATE_DB")
emu = EmulatorClient()
inj = ErrorInjector(db)

def info_present():
    return bool(db.hget("TRANSCEIVER_INFO|%s" % PORT, "manufacturer"))
def status_sw():
    return db.hgetall("TRANSCEIVER_STATUS_SW|%s" % PORT)
def dom_present():
    return "temperature" in db.hgetall("TRANSCEIVER_DOM_SENSOR|%s" % PORT)
def error_str():
    return status_sw().get("error") or ""

def measure(cond, cap=120, poll=0.2):
    t0 = time.time()
    while time.time() - t0 < cap:
        try:
            if cond():
                return time.time() - t0
        except Exception:
            pass
        time.sleep(poll)
    return None

results = {}
def rec(name, val):
    results.setdefault(name, []).append(val)
    print("  %-16s %s" % (name, "TIMEOUT" if val is None else "%.2fs" % val), flush=True)

snap_temp = emu.read_field(IDX, cmis.TEMP)
emu.plug(IDX)
measure(info_present, cap=120)
measure(dom_present, cap=120)

for i in range(3):
    print("plug/unplug cycle %d" % i, flush=True)
    emu.unplug(IDX)
    rec("clear_info", measure(lambda: not info_present(), cap=90))
    rec("status0", measure(lambda: status_sw().get("status") == "0", cap=90))
    emu.plug(IDX)
    rec("populate_info", measure(info_present, cap=90))
    rec("status1", measure(lambda: status_sw().get("status") == "1", cap=90))
    rec("cmis_ready", measure(lambda: status_sw().get("cmis_state") == "READY", cap=120))
    rec("dom_present", measure(dom_present, cap=120))

for i in range(2):
    print("error cycle %d" % i, flush=True)
    measure(dom_present, cap=120)
    inj.set(IDX, errors.I2C_STUCK_EVENT)
    rec("error_set", measure(lambda: "Bus stuck" in error_str(), cap=90))
    rec("dom_removed", measure(lambda: not dom_present(), cap=90))
    inj.clear(IDX)
    rec("error_clear", measure(lambda: "Bus stuck" not in error_str(), cap=90))
    rec("dom_restored", measure(dom_present, cap=120))

print("poll read (monitor)", flush=True)
mon = MonitorRecorder().start()
time.sleep(1)
mon.clear()
rec("poll_read", measure(lambda: len(mon.reads(index=IDX)) >= 1, cap=90))
mon.stop()

print("baseline restart", flush=True)
xc = XcvrdControl(statedb=db)
xc.flush_transceiver_tables()
xc.restart()
rec("baseline_populate", measure(info_present, cap=120))

print("dom value reflect (slow)", flush=True)
measure(dom_present, cap=120)
target = 42.5
emu.write_field(IDX, cmis.TEMP, cmis.encode_temperature(target))
def temp_ok():
    v = db.hgetall("TRANSCEIVER_DOM_SENSOR|%s" % PORT).get("temperature")
    try:
        return abs(float(v) - target) < 1.0
    except (TypeError, ValueError):
        return False
rec("dom_reflect", measure(temp_ok, cap=150))

print("\n=== SUMMARY (seconds) ===", flush=True)
for name, vals in results.items():
    good = [v for v in vals if v is not None]
    if good:
        print("%-16s n=%d min=%.2f mean=%.2f max=%.2f"
              % (name, len(good), min(good), sum(good) / len(good), max(good)), flush=True)
    else:
        print("%-16s TIMED OUT (all %d)" % (name, len(vals)), flush=True)

inj.clear_all()
emu.plug(IDX)
emu.write_field(IDX, cmis.TEMP, snap_temp)
print("done", flush=True)
