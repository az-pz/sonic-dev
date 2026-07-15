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
# a change dict value of '1' means "inserted", '0' means "removed".
SFP_STATUS_INSERTED = '1'
SFP_STATUS_REMOVED = '0'


class Chassis(ChassisBase):
    def __init__(self):
        super().__init__()
        count = self._discover_count()
        # ChassisBase.get_sfp() indexes _sfp_list positionally; keep it 0..count-1
        # so _sfp_list[i] is the emulator module with index i.
        self._sfp_list = [Sfp(i) for i in range(count)]
        # last-seen presence per physical port, lazily seeded on first
        # get_change_event() call so we only report *transitions* afterwards.
        self._presence_cache = None

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

    def _poll_presence(self):
        """Return {physical_port_index: bool_present} for every emulated SFP.

        The physical port index is the position in _sfp_list, which equals the
        emulator module index and the CONFIG_DB PORT 'index' (0-based) that
        xcvrd's port_mapping uses in get_physical_to_logical().
        """
        return {idx: bool(sfp.get_presence())
                for idx, sfp in enumerate(self._sfp_list)}

    def get_change_event(self, timeout=0):
        """Report transceiver hotplug (insert/remove) events to xcvrd.

        xcvrd's SfpStateUpdateTask calls this in a loop and, for each physical
        port in the returned dict, does get_physical_to_logical(int(port)) and
        then adds ('1') or deletes ('0') that port's TRANSCEIVER_* STATE_DB
        entries. We detect events by polling each emulated module's presence
        (GetInfo over gRPC) and diffing against the last-seen snapshot.

        We must NOT raise NotImplementedError: xcvrd would fall back to
        platform_sfputil.get_transceiver_change_event(), which is None on this
        emulated platform -> AttributeError that kills every xcvrd thread.

        timeout is in milliseconds (0 == block "forever"). We block up to
        `timeout`, returning early as soon as a change is seen.

        Returns (status, {'sfp': {port: '1'|'0'}, 'sfp_error': {}}).
        """
        poll_interval = 1.0
        # For timeout==0 ("block forever") still return within poll_interval so
        # xcvrd's loop stays responsive; otherwise honor the requested window.
        deadline = time.time() + (timeout / 1000.0 if timeout else poll_interval)

        if self._presence_cache is None:
            self._presence_cache = self._poll_presence()

        while True:
            current = self._poll_presence()
            changes = {
                str(idx): (SFP_STATUS_INSERTED if present else SFP_STATUS_REMOVED)
                for idx, present in current.items()
                if present != self._presence_cache.get(idx)
            }
            if changes or time.time() >= deadline:
                self._presence_cache = current
                return True, {'sfp': changes, 'sfp_error': {}}
            time.sleep(min(poll_interval, max(0.0, deadline - time.time())))
