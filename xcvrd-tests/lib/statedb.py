"""StateDB: thin wrapper over ``sonic-db-cli`` for black-box observation.

Tests read xcvrd's declared outputs (the TRANSCEIVER_* tables) from STATE_DB.
We shell out to sonic-db-cli rather than importing swsscommon so the harness has
zero binary dependencies on the DUT host (sonic-db-cli is always present, and it
reaches the same redis xcvrd writes to). Calls are ~50-100ms which is fine for
the polling cadence the black-box assertions use.
"""
import subprocess

SONIC_DB_CLI = "/usr/bin/sonic-db-cli"


class StateDB:
    def __init__(self, db="STATE_DB", binary=SONIC_DB_CLI):
        self.db = db
        self.binary = binary

    def _run(self, *args):
        out = subprocess.run(
            [self.binary, self.db, *[str(a) for a in args]],
            capture_output=True, text=True, timeout=20)
        if out.returncode != 0:
            raise RuntimeError(
                f"sonic-db-cli {self.db} {' '.join(map(str, args))} failed: "
                f"{out.stderr.strip()}")
        # CMIS string fields are fixed-width, null-padded by the emulator (real
        # modules space-pad). Those NUL bytes are padding artifacts, never real
        # data, and they break ast.literal_eval on HGETALL output -- strip them.
        return out.stdout.replace("\x00", "")

    # --- reads --------------------------------------------------------------
    def keys(self, pattern):
        out = self._run("KEYS", pattern).strip()
        return [k for k in out.splitlines() if k]

    def exists(self, key):
        return self._run("EXISTS", key).strip() == "1"

    def hget(self, key, field):
        val = self._run("HGET", key, field).rstrip("\n")
        return val if val != "" else None

    def hgetall(self, key):
        """Return the hash as a dict (empty dict if the key is absent)."""
        out = self._run("HGETALL", key)
        text = out.strip()
        # sonic-db-cli prints a Python-dict repr for HGETALL.
        if text.startswith("{") and text.endswith("}"):
            import ast
            try:
                return dict(ast.literal_eval(text))
            except (ValueError, SyntaxError):
                pass
        # Fallback: alternating key/value lines.
        toks = [t for t in text.split("\n") if t != ""]
        return {toks[i]: toks[i + 1] for i in range(0, len(toks) - 1, 2)}

    # --- writes (used only for test setup/teardown, e.g. flush / inject) -----
    def hset(self, key, field, value):
        return self._run("HSET", key, field, value)

    def hdel(self, key, field):
        return self._run("HDEL", key, field)

    def delete(self, key):
        return self._run("DEL", key)

    def delete_pattern(self, pattern):
        n = 0
        for k in self.keys(pattern):
            self.delete(k)
            n += 1
        return n
