#!/usr/bin/env python3
"""Generate an xcvr-emu config.yaml with N transceivers all present, using the
same CMIS QSFP-DD defaults as the emulator's bundled config. Indices 0..N-1 are
created so the bridge's get_sfp(physical_index) always hits a present module
regardless of 0- or 1-based physical numbering on the DUT.

A few indices are provisioned as SPECIAL modules, because the xcvrd-tests suite
needs transceivers that differ from the uniform CMIS default. These used to be
patched in after deployment by provision_special_modules.sh; they are now part
of the generated config so every emulator deploy has them and the tests that
depend on them can never silently skip:

  idx10 (Ethernet40)  type: sff8636       SFF-8636 / QSFP28    -> tests/test_sff8636.py
  idx11 (Ethernet44)  MediaInterfaceID 77 400GBASE-ZR (C-CMIS) -> tests/test_pm.py
  idx13 (Ethernet52)  MemoryModel FLAT    flat memory          -> tests/test_flat_memory.py
  idx14 (Ethernet56)  2 apps (40G + 100G) app selection        -> tests/test_app_select.py

Only config-level properties live here (the emulator re-applies them from config
on every plug). The raw byte images -- the SFF-8636 EEPROM page and the C-CMIS
PM/VDM stimulus -- are still written by the tests themselves at runtime.

Set EMU_NO_SPECIAL=1 to emit a uniform config with no special modules.

Usage: gen_emu_config.py [N] [OUT]
"""
import os
import sys

N = int(sys.argv[1]) if len(sys.argv) > 1 else 33
out = sys.argv[2] if len(sys.argv) > 2 else "emu_config.yaml"

# The default CMIS application: XLAUI C2M (Annex 83B) 40G / 40GBASE-LR4 (Cl 87),
# 4 host + 4 media lanes -- matches the Force10-S6000 40G ports.
APP_40G = {
    "HostInterfaceID": 6,
    "MediaInterfaceID": 9,
    "HostLaneCount": 4,
    "MediaLaneCount": 4,
    "HostLaneAssignmentOptions": "0b1",
}
# Second application for the multi-app module: CAUI-4 C2M (Annex 83E) -> 100G,
# also 4-lane, so the app-selection test can flip the port speed 40G<->100G and
# watch xcvrd pick AppSelCode 1 vs 2.
APP_100G = dict(APP_40G, HostInterfaceID=11)


def defaults_block(anchor=False, memory_model="PAGED", apps=(APP_40G,),
                   media_lane_opts=("0b001",)):
    """Emit the per-transceiver `defaults:` mapping as YAML text.

    Everything except the parameters below is identical for every module, so a
    special module differs from the baseline by exactly one property -- keeping
    the emulator's behaviour for the other modules untouched.
    """
    head = "    defaults: &defaults\n" if anchor else "    defaults:\n"
    body = """      SFF8024Identifier: "QSFP_DD"
      SFF8024IdentifierCopy: "QSFP_DD"
      VendorName: "xcvr-emu"
      VendorPN: "EMU-40G-LR4"
      VendorRev: "01"
      VendorOUI: 0x010203
      VendorSN: "0123456789"
      DateCode:
        Year: "24"
        Month: "12"
        DayOfMonth: "14"
      LengthMultiplier: "MULTIPLIER_100"
      BaseLength: 1
      CmisRevision:
        Major: 5
        Minor: 2
      MediaType: "OPTICAL_SMF"
      ModulePowerClass: "CLASS_8"
      MaxPower: 40
      BanksSupported: "BANKS_0_3_SUPPORTED"
      ConnectorType: "MPO_1X16"
      ModuleActiveFirmwareMajorRevision: 1
      ModuleActiveFirmwareMinorRevision: 2
      ModuleInactiveFirmwareMajorRevision: 1
      ModuleInactiveFirmwareMinorRevision: 1
      MemoryModel: "{memory_model}"
      MciMaxSpeed: "UP_TO_400_KHZ"
      ApplicationDescriptor:
""".format(memory_model=memory_model)
    for app in apps:
        body += (
            "        - HostInterfaceID: {HostInterfaceID}\n"
            "          MediaInterfaceID: {MediaInterfaceID}\n"
            "          HostLaneCount: {HostLaneCount}\n"
            "          MediaLaneCount: {MediaLaneCount}\n"
            "          HostLaneAssignmentOptions: {HostLaneAssignmentOptions}\n"
        ).format(**app)
    body += "      MediaLaneAssignmentOptions:\n"
    for opt in media_lane_opts:
        body += "      - {}\n".format(opt)
    body += """      MaxDurationDPInit: "BETWEEN_1_AND_5_S"
      OutputDisableTxSupported: "SUPPORTED"
      SteppedConfigOnly: "STEP_BY_STEP"
"""
    return head + body


# index -> (comment, top-level extra keys, defaults_block kwargs).
# Empty kwargs means the module needs no nested override, so it can reuse the
# *defaults anchor.
SPECIAL = {
    10: ("SFF-8636 (QSFP28) -> routed to SffManagerTask (tests/test_sff8636.py)",
         {"type": "sff8636"}, {}),
    11: ("coherent C-CMIS: 400GBASE-ZR media interface, code 77 (tests/test_pm.py)",
         {}, {"apps": (dict(APP_40G, MediaInterfaceID=77),)}),
    13: ("flat memory -> CmisManagerTask short-circuits to READY (tests/test_flat_memory.py)",
         {}, {"memory_model": "FLAT"}),
    14: ("multi-application: 40G + 100G, app selection across speeds "
         "(tests/test_app_select.py)",
         {}, {"apps": (APP_40G, APP_100G), "media_lane_opts": ("0b001", "0b001")}),
}

if os.environ.get("EMU_NO_SPECIAL") == "1":
    SPECIAL = {}

with open(out, "w") as f:
    f.write("transceivers:\n")
    f.write("  0:\n")
    f.write("    present: true\n")
    f.write(defaults_block(anchor=True))
    for i in range(1, N):
        special = SPECIAL.get(i)
        if special is None:
            f.write("  {}:\n".format(i))
            f.write("    present: true\n")
            f.write("    defaults: *defaults\n")
            continue
        comment, extra, kwargs = special
        f.write("  # idx{}: {}\n".format(i, comment))
        f.write("  {}:\n".format(i))
        f.write("    present: true\n")
        for k, v in extra.items():
            f.write("    {}: {}\n".format(k, v))
        f.write("    defaults: *defaults\n" if not kwargs else defaults_block(**kwargs))

missing = sorted(i for i in SPECIAL if i >= N)
if missing:
    print("WARNING: N={} is too small for special module(s) {} -- the tests that "
          "need them will skip".format(N, missing))
print("wrote {} with {} present transceivers ({} special: {})".format(
    out, N, len(SPECIAL) - len(missing), sorted(i for i in SPECIAL if i < N)))
