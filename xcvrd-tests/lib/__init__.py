"""xcvrd black-box test harness library.

Modules:
  emu        - EmulatorClient: gRPC client to the xcvr-emu emulator (:50051)
  monitor    - MonitorRecorder: background subscriber to the emulator Monitor
               stream, capturing every EEPROM read/write xcvrd performs
  statedb    - StateDB: thin sonic-db-cli wrapper (STATE_DB/CONFIG_DB/APPL_DB)
  xcvrd_ctl  - XcvrdControl: restart/stop/start xcvrd + flush TRANSCEIVER_* tables
  cmis       - CMIS field/offset helpers (temperature, voltage, ...)
  waits      - eventually()/wait_until() polling helpers
"""
