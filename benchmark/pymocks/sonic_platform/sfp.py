"""The mock transceiver slot -- the hot path of the whole harness.

Every payload is decoded from the fixture once in `__init__`; a call does one dict
copy and returns. `dict(self._x)` (~102ns) rather than returning `self._x` (~29ns)
is deliberate: the Rust bench edge must hand back an owned `serde_json::Value`
because `SfpHandle` is declared `-> Result<Value>`, so returning a shared reference
here would make the two edges semantically different, and would additionally let a
daemon mutate the fixture underneath itself -- a bug the real plant cannot have,
since it rebuilds its dict from EEPROM bytes on every call.
"""

from ._bench import RECORDER, TRACING


class Sfp(object):
    """Fast path: no tracing, no call log, no MagicMock."""

    def __init__(self, index, fixture):
        self.index = index
        # `sfp_type` is read as an ATTRIBUTE by the bridge
        # (platform-bridge/src/lib.rs:204 uses getattr, not call_method0), so it
        # must not be a method.
        self.sfp_type = fixture.get("sfp_type", "QSFP_DD")

        self._presence = bool(fixture.get("presence", True))
        self._replaceable = bool(fixture.get("replaceable", True))
        self._reset_status = bool(fixture.get("reset_status", False))
        self._error_description = fixture.get("error_description")
        self._lpmode = bool(fixture.get("lpmode", False))

        self._info = dict(fixture.get("info", {}))
        self._dom = dict(fixture.get("dom_real_value", {}))
        self._status = dict(fixture.get("status", {}))
        self._threshold = dict(fixture.get("threshold_info", {}))

        # Sparse offset -> byte map; absent offsets read as 0, like an erased page.
        self._eeprom = {int(k): int(v) for k, v in fixture.get("eeprom", {}).items()}
        self._eeprom_writes = []

        # Bind the fixture's extra no-arg getters (get_transceiver_status_flags,
        # get_transceiver_dom_flags, VDM/PM getters, ...) as real instance
        # attributes. Binding beats __getattr__ here: __getattr__ only fires on a
        # lookup MISS, so it would be hit on every one of these calls.
        for name, payload in fixture.get("json_calls", {}).items():
            self.__dict__[name] = self._make_json_call(name, payload)

    def _make_json_call(self, name, payload):
        frozen = dict(payload)

        def call():
            return dict(frozen)

        call.__name__ = name
        return call

    def get_presence(self):
        return self._presence

    def is_replaceable(self):
        return self._replaceable

    def get_reset_status(self):
        return self._reset_status

    def get_error_description(self):
        return self._error_description

    def get_transceiver_info(self):
        return dict(self._info)

    def get_transceiver_dom_real_value(self):
        return dict(self._dom)

    def get_transceiver_status(self):
        return dict(self._status)

    def get_transceiver_threshold_info(self):
        return dict(self._threshold)

    def get_lpmode(self):
        return self._lpmode

    def set_lpmode(self, on):
        self._lpmode = bool(on)
        return True

    def reset(self):
        return True

    def read_eeprom(self, offset, num_bytes):
        # List comprehension, not a generator: bytes() over a generator has to grow
        # its buffer incrementally and measured ~9x the Rust edge, which would have
        # been an artefact of this mock rather than of either daemon.
        eep = self._eeprom
        return bytes([eep.get(offset + i, 0) for i in range(num_bytes)])

    def write_eeprom(self, offset, num_bytes, buf):
        # Signature is (offset, num_bytes, buf) -- the bridge passes the length
        # explicitly (platform-bridge/src/lib.rs:279), matching SfpOptoeBase.
        self._eeprom_writes.append((offset, bytes(buf[:num_bytes])))
        for i in range(num_bytes):
            self._eeprom[offset + i] = buf[i]
        return True


class TracingSfp(Sfp):
    """Records every call for the equivalence gate.

    A separate subclass rather than an `if TRACING` inside each method: a module-global
    lookup plus branch is ~20ns, which is 20% of the 102ns call budget, so it would
    show up as measurement noise in exactly the numbers we care about. Selected once
    at construction instead.
    """

    def _hal(self, op):
        RECORDER.record(kind="hal", port=self.index, op=op)

    def get_presence(self):
        self._hal("get_presence")
        return Sfp.get_presence(self)

    def is_replaceable(self):
        self._hal("is_replaceable")
        return Sfp.is_replaceable(self)

    def get_reset_status(self):
        self._hal("get_reset_status")
        return Sfp.get_reset_status(self)

    def get_error_description(self):
        self._hal("get_error_description")
        return Sfp.get_error_description(self)

    def get_transceiver_info(self):
        self._hal("get_transceiver_info")
        return Sfp.get_transceiver_info(self)

    def get_transceiver_dom_real_value(self):
        self._hal("get_transceiver_dom_real_value")
        return Sfp.get_transceiver_dom_real_value(self)

    def get_transceiver_status(self):
        self._hal("get_transceiver_status")
        return Sfp.get_transceiver_status(self)

    def get_transceiver_threshold_info(self):
        self._hal("get_transceiver_threshold_info")
        return Sfp.get_transceiver_threshold_info(self)

    def get_lpmode(self):
        self._hal("get_lpmode")
        return Sfp.get_lpmode(self)

    def set_lpmode(self, on):
        self._hal("set_lpmode")
        return Sfp.set_lpmode(self, on)

    def reset(self):
        self._hal("reset")
        return Sfp.reset(self)

    def read_eeprom(self, offset, num_bytes):
        RECORDER.record(kind="eeprom_read", port=self.index, offset=offset, len=num_bytes)
        return Sfp.read_eeprom(self, offset, num_bytes)

    def write_eeprom(self, offset, num_bytes, buf):
        RECORDER.record(kind="eeprom_write", port=self.index, offset=offset, len=num_bytes)
        return Sfp.write_eeprom(self, offset, num_bytes, buf)

    def _make_json_call(self, name, payload):
        inner = Sfp._make_json_call(self, name, payload)

        def call():
            self._hal(name)
            return inner()

        call.__name__ = name
        return call


def make_sfp(index, fixture):
    return (TracingSfp if TRACING else Sfp)(index, fixture)
