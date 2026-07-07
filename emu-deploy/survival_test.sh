#!/bin/bash
# Survival test: restarting pmon must NOT touch the standalone emulator container,
# and supervisord must bring xcvrd back so INFO/DOM repopulate.
set -uo pipefail
echo "=== BEFORE restart ==="
docker ps --filter 'name=^/xcvr-emu$' --format 'EMU: {{.Names}} {{.Status}}'
docker exec pmon supervisorctl status xcvrd 2>&1 | sed 's/^/XCVRD: /'
emu_started_before=$(docker inspect -f '{{.State.StartedAt}}' xcvr-emu)
echo "EMU StartedAt(before)=$emu_started_before"

echo "=== restarting pmon (simulates the container churn a config reload causes) ==="
docker restart pmon >/dev/null
echo "pmon restarted; waiting for supervisord to bring xcvrd back (up to 150s)..."

for i in $(seq 1 30); do
  sleep 5
  st=$(docker exec pmon supervisorctl status xcvrd 2>/dev/null | awk '{print $2}')
  ni=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l)
  nd=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)
  echo "  [$((i*5))s] xcvrd=$st INFO=$ni DOM=$nd"
  [ "$st" = "RUNNING" ] && [ "$ni" -ge 28 ] && [ "$nd" -ge 28 ] && break
done

echo "=== AFTER restart ==="
docker ps --filter 'name=^/xcvr-emu$' --format 'EMU: {{.Names}} {{.Status}}'
emu_started_after=$(docker inspect -f '{{.State.StartedAt}}' xcvr-emu)
echo "EMU StartedAt(after) =$emu_started_after"
if [ "$emu_started_before" = "$emu_started_after" ]; then
  echo "RESULT: EMULATOR SURVIVED pmon restart (StartedAt unchanged) ✅"
else
  echo "RESULT: emulator was RESTARTED (StartedAt changed) ❌"
fi
echo "FINAL INFO=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_INFO|*' 2>/dev/null | wc -l) DOM=$(sonic-db-cli STATE_DB KEYS 'TRANSCEIVER_DOM_SENSOR|*' 2>/dev/null | wc -l)"
