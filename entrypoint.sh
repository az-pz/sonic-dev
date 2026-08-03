#!/bin/sh
# The sonic-xcvrd daemons build a sonic-py-common SysLogger, which attaches a
# logging.handlers.SysLogHandler to /dev/log. SONiC's CI runs inside a container
# that has a real syslog socket there; this slim image does not, so logging
# raises FileNotFoundError and crashes a handful of tests.
#
# To faithfully reproduce the CI environment WITHOUT modifying the repo, stand
# up a tiny datagram sink bound to /dev/log that simply drains messages, then
# exec the requested command (pytest by default).
python3 - <<'PY' &
import socket, os
path = "/dev/log"
try:
    os.unlink(path)
except OSError:
    pass
sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
sock.bind(path)
while True:
    try:
        sock.recv(65536)
    except Exception:
        pass
PY

# Wait until the socket exists before handing off to the test command.
i=0
while [ ! -S /dev/log ] && [ "$i" -lt 50 ]; do
    i=$((i + 1))
    sleep 0.1
done

exec "$@"
