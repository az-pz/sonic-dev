"""gRPC client to the xcvr-emu emulator (localhost:50051).

This is the test harness's *stimulus* surface: plug/unplug modules, read and
write raw EEPROM bytes, and query module state. It deliberately mirrors only
what the black-box tests need; xcvrd itself is never touched.
"""
import grpc

from .proto import pb, pb_grpc

DEFAULT_TARGET = "localhost:50051"


def port_to_index(port):
    """'Ethernet100' -> 25. The bridge names SFP i as Ethernet{i*4}."""
    return int(port.replace("Ethernet", "")) // 4


def index_to_port(index):
    """25 -> 'Ethernet100'."""
    return "Ethernet{}".format(index * 4)


class EmulatorClient:
    def __init__(self, target=DEFAULT_TARGET, timeout=10.0):
        self._channel = grpc.insecure_channel(target)
        self._stub = pb_grpc.SfpEmulatorServiceStub(self._channel)
        self._timeout = timeout

    def close(self):
        self._channel.close()

    # --- discovery ----------------------------------------------------------
    def list(self):
        """Return {index: present_bool} for every emulated module."""
        resp = self._stub.List(pb.ListRequest(), timeout=self._timeout)
        return {i.index: bool(i.present) for i in resp.infos}

    def indices(self):
        """Sorted list of module indices the emulator knows about."""
        return sorted(self.list().keys())

    def present(self, index):
        return bool(self._stub.GetInfo(
            pb.GetInfoRequest(index=index), timeout=self._timeout).present)

    def get_info(self, index):
        """Full GetInfoResponse (present, msm/module-state, dpsms)."""
        return self._stub.GetInfo(
            pb.GetInfoRequest(index=index), timeout=self._timeout)

    # --- presence (hot plug/unplug) ----------------------------------------
    def set_present(self, index, present):
        self._stub.UpdateInfo(
            pb.UpdateInfoRequest(index=index, present=bool(present)),
            timeout=self._timeout)

    def unplug(self, index):
        self.set_present(index, False)

    def plug(self, index):
        self.set_present(index, True)

    # --- raw EEPROM ---------------------------------------------------------
    def read(self, index, bank, page, offset, length, force=True):
        """Read raw EEPROM bytes. force=True reads even when absent (diagnostic)."""
        resp = self._stub.Read(pb.ReadRequest(
            index=index, bank=bank, page=page, offset=offset,
            length=length, force=force), timeout=self._timeout)
        return bytes(resp.data)

    def write(self, index, bank, page, offset, data):
        """Write raw EEPROM bytes at (bank, page, offset)."""
        data = bytes(data)
        self._stub.Write(pb.WriteRequest(
            index=index, bank=bank, page=page, offset=offset,
            data=data, length=len(data)), timeout=self._timeout)

    def read_field(self, index, field, force=True):
        """Read a (bank, page, offset, length) tuple (see lib.cmis)."""
        bank, page, offset, length = field
        return self.read(index, bank, page, offset, length, force=force)

    def write_field(self, index, field, data):
        bank, page, offset, _length = field
        self.write(index, bank, page, offset, data)
