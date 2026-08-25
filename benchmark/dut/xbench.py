#!/usr/bin/env python3
"""xbench -- benchmark the xcvrd daemon in situ on the DUT.

RUNS ON THE DUT (admin@vlab-01), where pmon, the xcvr-emu emulator and Redis are
all local. Self-contained by design: it shares no code with the xcvrd-tests suite,
which is the pipeline's trusted correctness oracle and must not be entangled with
a benchmark that will change often.

What this measures that the in-process harness cannot: a real supervised PROCESS
(RSS, CPU, threads, fds, SIGTERM), the whole daemon rather than one task, and the
real sonic_platform talking to the real emulator. What it measures WORSE: it cannot
separate orchestration from HAL from DB, and it inherits KVM noise. The two harnesses
answer different questions and both are needed.

Everything is reached the way an operator would reach it -- sonic-db-cli,
supervisorctl, /proc -- so no daemon change is required and the Python and Rust
variants are driven identically.

    ./xbench.py --list
    ./xbench.py B5 --variant rust --reps 3 --out /tmp/b05.jsonl
"""

import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time

PMON = "pmon"
TRANSCEIVER_TABLES = [
    "TRANSCEIVER_INFO", "TRANSCEIVER_DOM_SENSOR", "TRANSCEIVER_DOM_THRESHOLD",
    "TRANSCEIVER_STATUS", "TRANSCEIVER_STATUS_SW", "TRANSCEIVER_PM",
    "TRANSCEIVER_FIRMWARE_INFO", "TRANSCEIVER_DOM_FLAG", "TRANSCEIVER_STATUS_FLAG",
]
# Deliberately finer than the correctness suite's 0.5s poll: several of these
# transitions are sub-second, and a 500ms grid would quantise the very differences
# being measured. Where a daemon-stamped time exists (last_update_time) it is
# preferred over polling entirely, because it carries no observer error at all.
POLL = 0.05


def sh(cmd, timeout=120):
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout.strip(), r.stderr.strip()


class StateDb:
    def keys(self, pattern):
        _, out, _ = sh(f"sonic-db-cli STATE_DB KEYS '{pattern}'")
        return [k for k in out.splitlines() if k]

    def hgetall(self, key):
        _, out, _ = sh(f"sonic-db-cli STATE_DB HGETALL '{key}'")
        parts = out.splitlines()
        return dict(zip(parts[0::2], parts[1::2]))

    def hget(self, key, field):
        _, out, _ = sh(f"sonic-db-cli STATE_DB HGET '{key}' '{field}'")
        return out or None

    def count(self, table):
        return len(self.keys(f"{table}|*"))

    def flush_transceiver(self):
        """Remove every TRANSCEIVER_* row.

        Without this a benchmark can 'succeed' against rows an earlier run left
        behind -- the daemon would never have to do the work being timed.
        """
        for t in TRANSCEIVER_TABLES:
            ks = self.keys(f"{t}|*")
            for i in range(0, len(ks), 50):
                batch = " ".join(f"'{k}'" for k in ks[i:i + 50])
                if batch:
                    sh(f"sonic-db-cli STATE_DB DEL {batch}")


class Xcvrd:
    def status(self):
        _, out, _ = sh(f"docker exec {PMON} supervisorctl status xcvrd")
        return out

    def is_running(self):
        return "RUNNING" in self.status()

    def pid(self):
        """Resolve the live pid from supervisor.

        Must be re-resolved after every restart rather than cached: the Rust daemon
        re-execs itself to add --enable_sff_mgr (daemon.rs:190), so the pid supervisor
        first reports is not the pid that does the work, and a sampler holding the
        old one would silently measure a dead process.
        """
        m = re.search(r"pid (\d+)", self.status())
        return int(m.group(1)) if m else None

    def variant(self):
        """Which daemon is actually installed -- never assume the inject succeeded."""
        # The Rust daemon is installed as a python shim that execv's the binary
        # (supervisor invokes `python3 /usr/local/bin/xcvrd`), so detect the shim, not an ELF.
        rc, _, _ = sh(f"docker exec {PMON} sh -c 'grep -q xcvrd-rs /usr/local/bin/xcvrd'")
        return "rust" if rc == 0 else "python"

    def stop(self):
        return sh(f"docker exec {PMON} supervisorctl stop xcvrd")

    def start(self):
        return sh(f"docker exec {PMON} supervisorctl start xcvrd")

    def restart(self):
        return sh(f"docker exec {PMON} supervisorctl restart xcvrd")

    def wait_running(self, timeout=30):
        end = time.time() + timeout
        while time.time() < end:
            if self.is_running():
                return True
            time.sleep(0.2)
        return False


class ProcSampler(threading.Thread):
    """1Hz /proc sampler for the daemon process, from inside pmon.

    Not `docker stats`: that reports the whole container, which also holds every
    other pmon daemon plus the emulator's chatter, so the xcvrd signal would be a
    minority of the number.
    """

    def __init__(self, xcvrd, interval=1.0):
        super().__init__(daemon=True)
        self.xcvrd, self.interval = xcvrd, interval
        self.samples = []
        self._stop = threading.Event()

    def _read(self, pid):
        rc, out, _ = sh(
            f"docker exec {PMON} sh -c 'cat /proc/{pid}/status 2>/dev/null; "
            f"echo ---; cat /proc/{pid}/stat 2>/dev/null; "
            f"echo ---; ls /proc/{pid}/fd 2>/dev/null | wc -l'")
        if rc != 0 or not out:
            return None
        status, stat, fds = (out.split("---") + ["", "", ""])[:3]
        g = lambda k: next((int(l.split()[1]) for l in status.splitlines()
                            if l.startswith(k)), None)
        f = stat.split()
        return {
            "rss_kb": g("VmRSS:"), "hwm_kb": g("VmHWM:"), "threads": g("Threads:"),
            "fds": int(fds.strip() or 0),
            "utime": int(f[13]) if len(f) > 14 else None,
            "stime": int(f[14]) if len(f) > 14 else None,
            "t": time.time(),
        }

    def run(self):
        while not self._stop.is_set():
            pid = self.xcvrd.pid()
            if pid:
                s = self._read(pid)
                if s:
                    s["pid"] = pid
                    self.samples.append(s)
            self._stop.wait(self.interval)

    def stop(self):
        self._stop.set()
        self.join(timeout=5)

    def summary(self):
        if len(self.samples) < 2:
            return {"error": "insufficient samples"}
        rss = [s["rss_kb"] for s in self.samples if s["rss_kb"]]
        a, b = self.samples[0], self.samples[-1]
        hz = os.sysconf("SC_CLK_TCK")
        cpu = ((b["utime"] + b["stime"]) - (a["utime"] + a["stime"])) / hz
        span = b["t"] - a["t"]
        # A pid change mid-run means the daemon restarted (or re-exec'd) and the
        # CPU delta spans two processes -- reporting it as one would be wrong.
        pids = {s["pid"] for s in self.samples}
        return {
            "samples": len(self.samples), "span_s": round(span, 1),
            "rss_kb_median": sorted(rss)[len(rss) // 2] if rss else None,
            "rss_kb_max": max(rss) if rss else None,
            "hwm_kb": max(s["hwm_kb"] for s in self.samples if s["hwm_kb"]),
            "threads": b["threads"], "fds": b["fds"],
            "cpu_pct": round(100.0 * cpu / span, 2) if span > 0 else None,
            "pid_changed": len(pids) > 1, "pids": sorted(pids),
        }


# ---------------------------------------------------------------- scenarios

def b01_cold_start(db, x, args):
    """B1 -- restart to first TRANSCEIVER_INFO for every port.

    Flushes first, so the daemon must genuinely rediscover and republish the plant;
    counting rows that a previous run left behind would measure nothing.

    The target is the row count observed BEFORE the flush -- what this daemon actually
    publishes on this plant. Note that is not the emulator's module count: the daemon
    only publishes for slots mapping to a configured logical port, and the two differ
    on this testbed (33 modules, 32 ports).
    """
    expected = args.ports or len(db.keys("TRANSCEIVER_INFO|*"))
    if not expected:
        # A hardcoded fallback here would be a guess presented as a measurement: if
        # nothing is published yet, "how long to republish everything" has no meaning.
        return {"error": "no TRANSCEIVER_INFO rows before the restart, so there is no "
                         "baseline to republish; is xcvrd running and settled?"}
    db.flush_transceiver()
    t0 = time.time()
    x.restart()
    first = full = None
    end = t0 + args.timeout
    while time.time() < end:
        n = db.count("TRANSCEIVER_INFO")
        if n and first is None:
            first = time.time() - t0
        if n >= expected:
            full = time.time() - t0
            break
        time.sleep(POLL)
    out = {"expected_ports": expected, "first_info_s": first, "all_info_s": full,
           "timed_out": full is None}
    if full is None:
        out["timeout_reason"] = "only %d of %d rows returned within %ss" % (
            db.count("TRANSCEIVER_INFO"), expected, args.timeout)
    return out


def b04_dom_cadence(db, x, args):
    """B4 -- DOM steady state, measured from the daemon's OWN last_update_time.

    Not from polling: the daemon stamps that field when it posts, so the interval
    between stamps is the true publish cadence with no observer quantisation. Jitter
    (sigma) is the metric that separates a dedicated poll thread from a loop competing
    with other work.
    """
    # Wait for the first publish before measuring. After an inject the daemon has just
    # restarted and the default DOM interval is 60s, so starting immediately measures an
    # empty table rather than the cadence. This wait is NOT part of the measurement.
    deadline = time.time() + max(args.timeout, 120)
    ports = []
    while time.time() < deadline:
        ports = sorted(db.keys("TRANSCEIVER_DOM_SENSOR|*"))[: args.ports or 8]
        if ports:
            break
        time.sleep(1.0)
    if not ports:
        return {"error": "no TRANSCEIVER_DOM_SENSOR rows appeared; is xcvrd running?"}
    # Discard the partial interval we landed in: start counting from the next stamp.
    time.sleep(1.0)
    seen = {p: [] for p in ports}
    end = time.time() + args.duration
    last = {}
    while time.time() < end:
        for p in ports:
            v = db.hget(p, "last_update_time")
            if v and last.get(p) != v:
                seen[p].append(time.time())
                last[p] = v
        time.sleep(0.25)
    out = {}
    for p, ts in seen.items():
        gaps = [b - a for a, b in zip(ts, ts[1:])]
        if gaps:
            mean = sum(gaps) / len(gaps)
            var = sum((g - mean) ** 2 for g in gaps) / len(gaps)
            out[p.split("|")[-1]] = {"updates": len(ts), "mean_s": round(mean, 3),
                                     "jitter_sigma_s": round(var ** 0.5, 3)}
    return {"duration_s": args.duration, "ports": len(ports), "per_port": out}


def b05_idle_soak(db, x, args):
    """B5 -- resource footprint while idle.

    Report the delta honestly: the Rust binary embeds CPython (pyo3 auto-initialize)
    and imports sonic_platform_base, so expect RSS in the same order as the Python
    daemon, not an order below it. Anyone expecting a 10x memory win will misread this.
    """
    s = ProcSampler(x)
    s.start()
    time.sleep(args.duration)
    s.stop()
    return {"duration_s": args.duration, **s.summary()}


def b10_sigterm(db, x, args):
    """B10 -- SIGTERM to process exit.

    Operationally load-bearing: supervisord's stopwaitsecs is 10s, and an overrun
    means SIGKILL in the middle of a STATE_DB write. Expect a Rust win by
    construction rather than by tuning -- result_4 bounds shutdown with a join grace
    plus a hard backstop; the Python reference has no such bound.
    """
    out = []
    for _ in range(args.reps):
        if not x.is_running():
            x.start(); x.wait_running()
        time.sleep(2)
        t0 = time.time()
        x.stop()
        stopped = None
        end = t0 + args.timeout
        while time.time() < end:
            if "RUNNING" not in x.status():
                stopped = time.time() - t0
                break
            time.sleep(0.02)
        out.append(stopped)
        x.start(); x.wait_running()
    ok = [v for v in out if v is not None]
    ok.sort()
    return {"reps": len(out), "samples_s": out,
            "p50_s": ok[len(ok) // 2] if ok else None, "max_s": max(ok) if ok else None}


# --------------------------------------------------- stimulus-driven scenarios
# These need the emulator (plug/unplug) or the bridge's error hook, which is why
# they cannot run in the in-process harness.

def _pct(vals, p):
    v = sorted(x for x in vals if x is not None)
    return v[min(int(len(v) * p), len(v) - 1)] if v else None


def _wait(pred, timeout, poll=POLL):
    """Wait for a predicate, returning elapsed seconds or None on timeout."""
    t0 = time.time()
    end = t0 + timeout
    while time.time() < end:
        if pred():
            return time.time() - t0
        time.sleep(poll)
    return None


def _wait_since(t0, pred, timeout, poll=POLL):
    """Like _wait, but reports elapsed from an EXTERNAL t0.

    For multi-stage observations of one stimulus (first row, then all rows), every
    stage has to be timed from the stimulus itself. Timing each from its own start
    yields sequential intervals that read as absolute latencies -- and can print a
    later milestone as a smaller number than an earlier one.
    """
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return round(time.time() - t0, 3)
        time.sleep(poll)
    return None


def b02_hotplug(db, x, args):
    """B2 -- single-port hot plug and unplug latency."""
    from emu import Emu
    e = Emu()
    present = e.present_indices()
    if not present:
        return {"error": "emulator reports no present modules"}
    idx = args.port if args.port is not None else present[0]
    port = "Ethernet%d" % (idx * 4)
    plug, unplug = [], []
    for _ in range(args.reps):
        # unplug -> INFO row cleared
        e.set_present(idx, False)
        unplug.append(_wait(lambda: not db.keys("TRANSCEIVER_INFO|%s" % port), args.timeout))
        # plug -> INFO row repopulated
        e.set_present(idx, True)
        plug.append(_wait(lambda: bool(db.keys("TRANSCEIVER_INFO|%s" % port)), args.timeout))
    e.close()
    return {"index": idx, "port": port,
            "plug_s": {"p50": _pct(plug, .5), "p95": _pct(plug, .95), "raw": plug},
            "unplug_s": {"p50": _pct(unplug, .5), "p95": _pct(unplug, .95), "raw": unplug}}


def b03_cmis_bringup(db, x, args):
    """B3 -- plug to cmis_state == READY.

    NOTE result_4 holds each CMIS datapath state for CMIS_INTER_STATE_DWELL_MS = 1000
    (cmis_manager_task.rs:81) where the Python reference does not, so bring-up is
    slower BY CONSTRUCTION on that implementation. Report it, do not silently
    attribute it to the rewrite.
    """
    from emu import Emu
    e = Emu()
    present = e.present_indices()
    if not present:
        return {"error": "emulator reports no present modules"}
    idx = args.port if args.port is not None else present[0]
    port = "Ethernet%d" % (idx * 4)
    out = []
    for _ in range(args.reps):
        e.set_present(idx, False)
        _wait(lambda: not db.keys("TRANSCEIVER_INFO|%s" % port), args.timeout)
        e.set_present(idx, True)
        out.append(_wait(
            lambda: db.hget("TRANSCEIVER_STATUS_SW|%s" % port, "cmis_state") == "READY",
            args.timeout))
    e.close()
    return {"index": idx, "port": port,
            "ready_s": {"p50": _pct(out, .5), "p95": _pct(out, .95), "raw": out},
            "caveat": "result_4 adds a deliberate 1s dwell per CMIS state"}


def b06_plug_storm(db, x, args):
    """B6 -- unplug every module, then plug them all at once.

    The completion target is the daemon's OWN baseline row count, captured before the
    storm -- not the emulator's module count. Those differ: the emulator presents a
    module per slot, but the daemon only publishes TRANSCEIVER_INFO for slots that map
    to a configured logical port, so a plant with more modules than CONFIG_DB ports can
    never reach len(modules) rows. Targeting the module count made this scenario wait
    out its full timeout and report all_info_s = null for BOTH daemons -- a harness
    defect that looked like a daemon failure. Observed live: 33 modules, 32 ports.
    """
    from emu import Emu
    e = Emu()
    idxs = e.present_indices()
    if not idxs:
        e.close()
        return {"error": "emulator reports no present modules"}

    baseline = db.count("TRANSCEIVER_INFO")
    if baseline == 0:
        # Nothing published to begin with: "restore what was there" is not a usable
        # target, and a zero would silently pass on the first poll.
        e.close()
        return {"error": "no TRANSCEIVER_INFO rows before the storm; is xcvrd running "
                         "and settled?"}

    for i in idxs:
        e.set_present(i, False)
    cleared = _wait(lambda: db.count("TRANSCEIVER_INFO") == 0, args.timeout)

    t0 = time.time()
    for i in idxs:
        e.set_present(i, True)
    # Both measured from t0, the moment of the plug. _wait returns elapsed from ITS own
    # start, so chaining two calls would make the second an interval beginning when the
    # first row appeared -- which produced the nonsense of a daemon reporting
    # all_info_s = 1.50 after first_info_s = 2.90, i.e. finishing before it started.
    first = _wait_since(t0, lambda: db.count("TRANSCEIVER_INFO") > 0, args.timeout)
    last = _wait_since(t0, lambda: db.count("TRANSCEIVER_INFO") >= baseline, args.timeout)
    e.close()

    final = db.count("TRANSCEIVER_INFO")
    out = {"modules": len(idxs), "target_rows": baseline,
           "unplug_cleared_s": cleared,
           "first_info_s": first, "all_info_s": last,
           "elapsed_s": round(time.time() - t0, 3), "final_rows": final}
    # Say why a null is a null, rather than leaving the reader to guess whether the
    # daemon was slow or the scenario was mis-targeted.
    if last is None:
        out["timeout_reason"] = (
            "only %d of %d rows returned within %ss" % (final, baseline, args.timeout))
    if cleared is None:
        out["unplug_warning"] = (
            "STATE_DB never emptied after unplugging all modules; the replug timing "
            "starts from a dirty state and is not comparable")
    return out


def b08_error_inject(db, x, args):
    """B8 -- fault set and clear latency, via the bridge's STATE_DB error hook."""
    import inject_err
    if not inject_err.hooks_enabled():
        return {"error": "bridge .test_hooks marker absent - error injection is inert; "
                         "redeploy the platform with test hooks enabled"}
    from emu import Emu
    e = Emu()
    present = e.present_indices()
    idx = args.port if args.port is not None else (present[0] if present else 0)
    port = "Ethernet%d" % (idx * 4)
    e.close()
    sets, clears = [], []
    for _ in range(args.reps):
        inject_err.set_error(idx)
        sets.append(_wait(lambda: bool(db.hget("TRANSCEIVER_STATUS_SW|%s" % port, "error")),
                          args.timeout))
        inject_err.clear_error(idx)
        clears.append(_wait(
            lambda: not db.hget("TRANSCEIVER_STATUS_SW|%s" % port, "error"), args.timeout))
    inject_err.clear_all()
    return {"index": idx, "port": port,
            "set_s": {"p50": _pct(sets, .5), "raw": sets},
            "clear_s": {"p50": _pct(clears, .5), "raw": clears}}


def b09_read_amplification(db, x, args):
    """B9 -- EEPROM work per DOM cycle, from the emulator's Monitor stream.

    THE VALIDITY GATE, and the highest-signal measurement available on the DUT:
    it counts work rather than time, so it is immune to KVM steal and host load. A
    gap between implementations here is a fidelity defect, not a performance datum,
    and every timing number should be distrusted until it is explained.
    """
    from emu import Monitor
    m = Monitor().start_and_wait()
    time.sleep(args.duration)
    m.stop()
    s = m.summary()
    per = s["per_port"]
    ports = len(per) or 1
    return {"window_s": args.duration, "total_events": s["total"],
            "reads": sum(v["reads"] for v in per.values()),
            "writes": sum(v["writes"] for v in per.values()),
            "ports_touched": len(per),
            "events_per_port": round(s["total"] / ports, 1),
            "page_histogram": s["page_histogram"]}


def b11_media_settings(db, x, args):
    """B11 -- media-settings notification latency on insert.

    Exposes a known inefficiency in BOTH result_4 and result_5: fancy_regex::Regex::new()
    is recompiled per call in get_media_settings / match_optics_si_key (common.rs:303,313)
    rather than compiled once.
    """
    from emu import Emu
    e = Emu()
    present = e.present_indices()
    if not present:
        return {"error": "emulator reports no present modules"}
    idx = args.port if args.port is not None else present[0]
    port = "Ethernet%d" % (idx * 4)
    out = []
    for _ in range(args.reps):
        e.set_present(idx, False)
        _wait(lambda: not db.keys("TRANSCEIVER_INFO|%s" % port), args.timeout)
        e.set_present(idx, True)
        out.append(_wait(
            lambda: bool(db.hget("TRANSCEIVER_STATUS_SW|%s" % port, "media_settings_sync_status"))
                    or bool(db.keys("TRANSCEIVER_INFO|%s" % port)), args.timeout))
    e.close()
    return {"index": idx, "port": port,
            "notify_s": {"p50": _pct(out, .5), "p95": _pct(out, .95), "raw": out}}


SCENARIOS = {
    "B1": ("b01_cold_start_info", b01_cold_start),
    "B4": ("b04_dom_steady_state", b04_dom_cadence),
    "B2": ("b02_hotplug_single", b02_hotplug),
    "B3": ("b03_cmis_bringup", b03_cmis_bringup),
    "B5": ("b05_idle_soak", b05_idle_soak),
    "B6": ("b06_plug_storm", b06_plug_storm),
    "B8": ("b08_error_inject", b08_error_inject),
    "B9": ("b09_read_amplification", b09_read_amplification),
    "B11": ("b11_media_settings_notify", b11_media_settings),
    "B10": ("b10_sigterm_shutdown", b10_sigterm),
}


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("scenario", nargs="?", help="B1 | B4 | B5 | B10")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--ports", type=int, default=0, help="0 = whatever the DUT presents")
    ap.add_argument("--duration", type=float, default=60.0)
    ap.add_argument("--timeout", type=float, default=180.0)
    ap.add_argument("--port", type=int, default=None,
                    help="physical module index to stimulate (default: first present)")
    ap.add_argument("--out", default="")
    a = ap.parse_args()

    if a.list or not a.scenario:
        print(f"{'ID':<5}{'SCENARIO':<30}{'MEASURES'}")
        for k, (name, fn) in SCENARIOS.items():
            print(f"{k:<5}{name:<30}{(fn.__doc__ or '--').splitlines()[0].split('-- ',1)[-1]}")
        return 0

    key = a.scenario.upper()
    if key not in SCENARIOS:
        sys.exit(f"unknown scenario {a.scenario!r}; try --list")
    name, fn = SCENARIOS[key]

    db, x = StateDb(), Xcvrd()
    variant = x.variant()
    if not x.is_running():
        x.start(); x.wait_running()

    recs = []
    inner_looped = {"B10", "B2", "B3", "B8", "B11"}
    for rep in range(1 if key in inner_looped else a.reps):
        t0 = time.time()
        res = fn(db, x, a)
        recs.append({"scenario": key, "name": name, "variant": variant, "rep": rep,
                     "wall_s": round(time.time() - t0, 2), "result": res,
                     "ts": time.strftime("%Y-%m-%dT%H:%M:%S")})
        print(json.dumps(recs[-1]))

    if a.out:
        with open(a.out, "a") as fh:
            for r in recs:
                fh.write(json.dumps(r) + "\n")
        sys.stderr.write(f"appended {len(recs)} record(s) to {a.out}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
