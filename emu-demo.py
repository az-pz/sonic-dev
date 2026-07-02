"""End-to-end demo: drive the real SONiC CmisApi against an emulated CMIS module
through the sonic_platform bridge (gRPC -> xcvr-emu).

This deliberately uses ONLY the bridge's own Sfp methods -- get_xcvr_api(),
get_presence(), read_eeprom(), write_eeprom() -- never the raw gRPC stub, to
show the full SONiC transceiver stack running against the emulated optic.

Run inside the image with the bridge on PYTHONPATH; it starts its own xcvr-emud.
"""
import os
import time
import subprocess

import xcvr_emu  # for locating the bundled config.yaml

CFG = os.path.join(os.path.dirname(xcvr_emu.__file__), "config.yaml")

# A writable CMIS register to demonstrate a write/read round-trip through the
# bridge: Staged Control Set 0, upper memory of page 10h (host-written config).
# SONiC addresses the EEPROM by a flat "optoe linear" offset; for (page=0x10,
# bank=0, window_offset=128) that is (0*256 + 0x10)*128 + 128 = 2176.
WRITABLE_PAGE = 0x10
WRITABLE_LINEAR = WRITABLE_PAGE * 128 + 128  # 2176


def main():
    emud = subprocess.Popen(["xcvr-emud", "-c", CFG],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    try:
        # Imported after the daemon is up so List() discovery succeeds.
        from sonic_platform.platform import Platform

        chassis = Platform().get_chassis()
        print("num sfps:", chassis.get_num_sfps())

        sfp = chassis.get_sfp(0)
        print("sfp0 presence:", sfp.get_presence())

        # The whole CMIS stack runs for real; only the byte fetch is the emulator.
        api = sfp.get_xcvr_api()
        print("xcvr api class:", type(api).__name__)

        info = api.get_transceiver_info()
        keys = ["type", "type_abbrv_name", "manufacturer", "model",
                "vendor_rev", "vendor_oui", "vendor_date", "serial",
                "host_electrical_interface", "module_media_interface_id",
                "host_lane_count", "media_lane_count", "cmis_rev",
                "application_advertisement"]
        print("\n=== get_transceiver_info() (selected) ===")
        for k in keys:
            if isinstance(info, dict) and k in info:
                print(f"  {k}: {info[k]}")

        # --- read path via the bridge's own Sfp.read_eeprom -------------------
        # VendorName is CMIS page 00h bytes 129..144 (linear offset == 129).
        print("\n=== raw EEPROM read via Sfp.read_eeprom ===")
        ident = sfp.read_eeprom(0, 1)
        vendor = sfp.read_eeprom(129, 16)
        print("  identifier byte (offset 0):", ident.hex(), "->", ident[0])
        print("  vendor name (offset 129,16):",
              vendor.decode("ascii", "replace").rstrip("\x00 "))

        # --- write/read round-trip via Sfp.write_eeprom + Sfp.read_eeprom -----
        print("\n=== EEPROM write/read round-trip via Sfp.write_eeprom ===")
        before = sfp.read_eeprom(WRITABLE_LINEAR, 1)
        new_val = bytes([(before[0] ^ 0x5A) if before else 0x5A])
        wrote_ok = sfp.write_eeprom(WRITABLE_LINEAR, 1, new_val)
        after = sfp.read_eeprom(WRITABLE_LINEAR, 1)
        print(f"  offset {WRITABLE_LINEAR} (page 10h): "
              f"before={before.hex()} wrote={new_val.hex()} "
              f"after={after.hex()} write_ok={wrote_ok}")
        assert after == new_val, "write_eeprom did not round-trip through the emulator"
        # restore original value, again through the bridge
        sfp.write_eeprom(WRITABLE_LINEAR, 1, before)
        restored = sfp.read_eeprom(WRITABLE_LINEAR, 1)
        print(f"  restored={restored.hex()} (matches original: {restored == before})")

        print("\nOK: real CmisApi + Sfp.read_eeprom/write_eeprom drove the "
              "emulated module end-to-end.")
    finally:
        emud.terminate()
        try:
            emud.wait(timeout=5)
        except Exception:
            emud.kill()


if __name__ == "__main__":
    main()
