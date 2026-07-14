#!/usr/bin/env python3
"""Generate an xcvr-emu config.yaml with N transceivers all present, using the
same CMIS QSFP-DD defaults as the emulator's bundled config. Indices 0..N-1 are
created so the bridge's get_sfp(physical_index) always hits a present module
regardless of 0- or 1-based physical numbering on the DUT."""
import sys

N = int(sys.argv[1]) if len(sys.argv) > 1 else 33
out = sys.argv[2] if len(sys.argv) > 2 else "emu_config.yaml"

DEFAULTS = """    defaults: &defaults
      SFF8024Identifier: "QSFP_DD"
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
      MemoryModel: "PAGED"
      MciMaxSpeed: "UP_TO_400_KHZ"
      ApplicationDescriptor:
        - HostInterfaceID: 6      # XLAUI C2M (Annex 83B) - 40G, 4 host lanes (matches Force10-S6000 40G ports)
          MediaInterfaceID: 9     # 40GBASE-LR4 (Cl 87) - 40G SMF, 4 media lanes
          HostLaneCount: 4
          MediaLaneCount: 4
          HostLaneAssignmentOptions: 0b1
      MediaLaneAssignmentOptions:
      - 0b001
      MaxDurationDPInit: "BETWEEN_1_AND_5_S"
      OutputDisableTxSupported: "SUPPORTED"
      SteppedConfigOnly: "STEP_BY_STEP"
"""

with open(out, "w") as f:
    f.write("transceivers:\n")
    f.write("  0:\n")
    f.write("    present: true\n")
    f.write(DEFAULTS)
    for i in range(1, N):
        f.write("  {}:\n".format(i))
        f.write("    present: true\n")
        f.write("    defaults: *defaults\n")

print("wrote {} with {} present transceivers".format(out, N))
