# Minimal stub distribution that registers the 'sonic-config-engine' package
# name so the sonic-platform-common build-time guard
# (pkg_resources.get_distribution) is satisfied without pulling in the heavy
# real sonic-config-engine (libyang, swsssdk, ...). The xcvrd unit tests do not
# exercise config-engine functionality.
from setuptools import setup

setup(
    name="sonic-config-engine",
    version="1.0",
    description="Stub sonic-config-engine to satisfy build-time dependency guards",
    py_modules=[],
)
