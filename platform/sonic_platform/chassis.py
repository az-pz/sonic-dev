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


class Chassis(ChassisBase):
    def __init__(self):
        super().__init__()
        count = self._discover_count()
        # ChassisBase.get_sfp() indexes _sfp_list positionally; keep it 0..count-1
        # so _sfp_list[i] is the emulator module with index i.
        self._sfp_list = [Sfp(i) for i in range(count)]

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

    def get_change_event(self, timeout=0):
        """Report transceiver hotplug events to xcvrd's SfpStateUpdateTask.

        xcvrd calls this in a loop; if we raised NotImplementedError it would
        fall back to platform_sfputil.get_transceiver_change_event(), but on
        this emulated platform platform_sfputil is None -> AttributeError that
        kills every xcvrd thread (including DomInfoUpdateTask, so DOM never
        populates). The emulated plant is stable (no insert/remove after init),
        so we block up to `timeout` ms and report "no change".

        Returns (status, {'sfp': {port: status}, 'sfp_error': {}}).
        """
        # timeout is in milliseconds (0 == block "forever"; cap so xcvrd's loop
        # stays responsive without busy-spinning).
        time.sleep(min(timeout / 1000.0, 1.0) if timeout else 1.0)
        return True, {'sfp': {}, 'sfp_error': {}}
