"""Sfp implementation backed by the xcvr-emu emulator over gRPC.

The whole point of subclassing SfpOptoeBase is that the entire CMIS/SFF decode
stack (get_xcvr_api / get_transceiver_info / CmisApi …) is built on three hooks:
read_eeprom(offset, num_bytes), write_eeprom(offset, num_bytes, buf) and
get_presence(). On real hardware those touch the optoe sysfs EEPROM; here they
translate to the emulator's gRPC Read/Write/GetInfo.

Addressing
----------
SONiC computes a flat "optoe linear" offset for every field via
CmisPage.linear_offset(page, bank, offset):

    if page == 0 and offset < 128:        # lower memory
        linear = offset
    else:
        linear = (bank * 256 + page) * 128 + offset   # offset is 0..255 window

The emulator's Read/Write instead take (bank, page, offset) where offset is the
CMIS 0..255 page window (0..127 lower, 128..255 upper of `page`) — confirmed by
reading VendorName at (0, 0, 129). So we invert linear_offset below.
"""
import grpc

from sonic_platform_base.sonic_xcvr.sfp_optoe_base import SfpOptoeBase

from .emu_client import get_stub, pb

ARCH_PAGES = 256      # CMIS_ARCH_PAGES: pages per bank in the linear layout
PAGE_SIZE = 128       # bytes per (lower or upper) memory half


def linear_to_bpo(linear):
    """Invert CmisPage.linear_offset → (bank, page, window_offset).

    window_offset is the emulator's 0..255 page-window address.
    """
    if linear < PAGE_SIZE:
        return 0, 0, linear
    # linear = (bank*256 + page + 1) * 128 + (window_offset - 128)
    idx = (linear // PAGE_SIZE) - 1
    bank = idx // ARCH_PAGES
    page = idx % ARCH_PAGES
    window_offset = (linear % PAGE_SIZE) + PAGE_SIZE
    return bank, page, window_offset


def _iter_chunks(offset, num_bytes):
    """Yield (linear_start, length) chunks that never cross a 128-byte linear
    boundary, so each maps cleanly to one emulator (bank, page, window) read."""
    cur = offset
    remaining = num_bytes
    while remaining > 0:
        next_boundary = ((cur // PAGE_SIZE) + 1) * PAGE_SIZE
        chunk = min(remaining, next_boundary - cur)
        yield cur, chunk
        cur += chunk
        remaining -= chunk


class Sfp(SfpOptoeBase):
    def __init__(self, index):
        super().__init__()
        self.index = index          # emulator transceiver index
        self.sfp_type = "QSFP_DD"

    # --- the three hardware hooks, redirected to the emulator -----------------

    def read_eeprom(self, offset, num_bytes):
        stub = get_stub()
        out = bytearray()
        try:
            for lin, length in _iter_chunks(offset, num_bytes):
                bank, page, woff = linear_to_bpo(lin)
                resp = stub.Read(pb.ReadRequest(
                    index=self.index, bank=bank, page=page,
                    offset=woff, length=length))
                out += resp.data
            return out
        except grpc.RpcError:
            return None

    def write_eeprom(self, offset, num_bytes, write_buffer):
        stub = get_stub()
        buf = bytes(write_buffer)
        try:
            pos = 0
            for lin, length in _iter_chunks(offset, num_bytes):
                bank, page, woff = linear_to_bpo(lin)
                stub.Write(pb.WriteRequest(
                    index=self.index, bank=bank, page=page, offset=woff,
                    length=length, data=buf[pos:pos + length]))
                pos += length
            return True
        except grpc.RpcError:
            return False

    def get_presence(self):
        try:
            return bool(get_stub().GetInfo(pb.GetInfoRequest(index=self.index)).present)
        except grpc.RpcError:
            return False

    # --- misc SfpBase niceties ------------------------------------------------

    def get_name(self):
        return "Ethernet{}".format(self.index * 4)

    def get_position_in_parent(self):
        return self.index

    def is_replaceable(self):
        return True
