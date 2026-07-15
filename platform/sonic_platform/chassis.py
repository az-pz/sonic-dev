"""Chassis backed by the xcvr-emu emulator.

Builds one Sfp per emulated transceiver. The SFP list is discovered from the
emulator's List() RPC at construction time; if the emulator isn't reachable yet
it falls back to XCVR_EMU_NUM_SFPS (default 8) placeholder SFPs (the gRPC calls
just return absent/None until the emulator is up).
"""
import os
import time

import grpc

from sonic_platform_base.chassis_base import ChassisBase

from .emu_client import get_stub, pb
from .sfp import Sfp

# xcvrd's SfpStateUpdateTask event codes (see sonic_platform_base sfp_status_helper):
# a change dict value of '1' means "inserted", '0' means "removed"; any other
# (decimal string of an SfpBase error bitmap) is a hardware error event.
SFP_STATUS_INSERTED = '1'
SFP_STATUS_REMOVED = '0'

# Test-only error injection. A row  XCVR_EMU_INJECT|<physical_index>  in STATE_DB
# with field 'event' set to an sfp change-event value (an SfpBase error bitmap as
# a decimal string, e.g. '11' = INSERTED|BLOCKING|I2C_STUCK) makes
# get_change_event() surface that event for the port, exactly as a real platform
# would report a hardware error. No such table exists in production, so this hook
# is inert there; it only lets the black-box tests drive error paths.
INJECT_TABLE = 'XCVR_EMU_INJECT'


class Chassis(ChassisBase):
    def __init__(self):
        super().__init__()
        count = self._discover_count()
        # ChassisBase.get_sfp() indexes _sfp_list positionally; keep it 0..count-1
        # so _sfp_list[i] is the emulator module with index i.
        self._sfp_list = [Sfp(i) for i in range(count)]
        # last-reported change-event per physical port, lazily seeded on the first
        # get_change_event() call so we only report *transitions* afterwards.
        self._event_cache = None
        self._statedb = None

    @staticmethod
    def _discover_count():
        try:
            infos = get_stub().List(pb.ListRequest()).infos
            if infos:
                return max(i.index for i in infos) + 1
        except grpc.RpcError:
            pass
        return int(os.environ.get("XCVR_EMU_NUM_SFPS", "8"))

    def get_num_sfps(self):
        return len(self._sfp_list)

    def get_all_sfps(self):
        return self._sfp_list

    def get_sfp(self, index):
        # xcvrd uses 1-based physical port numbering in some call sites; accept
        # both by clamping into range.
        try:
            return self._sfp_list[index]
        except IndexError:
            return None

    def _get_statedb(self):
        """Lazily connect a STATE_DB reader; return None if unavailable."""
        if self._statedb is None:
            try:
                from swsscommon.swsscommon import SonicV2Connector
                db = SonicV2Connector(use_unix_socket_path=True)
                db.connect('STATE_DB')
                self._statedb = db
            except Exception:  # pragma: no cover - redis/swsscommon unavailable
                self._statedb = False
        return self._statedb or None

    def _read_injections(self):
        """Return {physical_index: event_str} from the STATE_DB inject table.

        Failures (redis down, swsscommon missing) degrade to "no injection" so a
        test hook can never take down xcvrd.
        """
        db = self._get_statedb()
        if db is None:
            return {}
        out = {}
        try:
            for key in (db.keys('STATE_DB', f'{INJECT_TABLE}|*') or []):
                event = db.get('STATE_DB', key, 'event')
                if not event:
                    continue
                try:
                    out[int(key.split('|', 1)[1])] = str(event)
                except (ValueError, IndexError):
                    continue
        except Exception:  # pragma: no cover
            return {}
        return out

    def _desired_events(self):
        """{physical_index: event_str}: injected error if any, else presence.

        The physical index is the position in _sfp_list, which equals the
        emulator module index and the CONFIG_DB PORT 'index' (0-based) that
        xcvrd's port_mapping uses in get_physical_to_logical().
        """
        inj = self._read_injections()
        out = {}
        for idx, sfp in enumerate(self._sfp_list):
            if idx in inj:
                out[idx] = inj[idx]
            else:
                out[idx] = SFP_STATUS_INSERTED if sfp.get_presence() else SFP_STATUS_REMOVED
        return out

    def get_change_event(self, timeout=0):
        """Report transceiver change events (insert / remove / error) to xcvrd.

        xcvrd's SfpStateUpdateTask calls this in a loop and, for each physical
        port in the returned dict, does get_physical_to_logical(int(port)) and
        acts on the value: '1' inserts + repopulates, '0' removes, and any other
        value is parsed as an SfpBase error bitmap (sets TRANSCEIVER_STATUS_SW.
        error and, for a blocking error, removes DOM). We compute each port's
        desired event (injected error, else live presence) and report only the
        transitions.

        We must NOT raise NotImplementedError: xcvrd would fall back to
        platform_sfputil.get_transceiver_change_event(), which is None on this
        emulated platform -> AttributeError that kills every xcvrd thread.

        timeout is in milliseconds (0 == block "forever"). We block up to
        `timeout`, returning early as soon as a change is seen.

        Returns (status, {'sfp': {port: value}, 'sfp_error': {}}).
        """
        poll_interval = 1.0
        # For timeout==0 ("block forever") still return within poll_interval so
        # xcvrd's loop stays responsive; otherwise honor the requested window.
        deadline = time.time() + (timeout / 1000.0 if timeout else poll_interval)

        if self._event_cache is None:
            self._event_cache = self._desired_events()

        while True:
            current = self._desired_events()
            changes = {
                str(idx): event
                for idx, event in current.items()
                if event != self._event_cache.get(idx)
            }
            if changes or time.time() >= deadline:
                self._event_cache = current
                return True, {'sfp': changes, 'sfp_error': {}}
            time.sleep(min(poll_interval, max(0.0, deadline - time.time())))
