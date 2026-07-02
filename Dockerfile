# Dev image for running sonic-xcvrd tests with the REAL swss-common bindings.
#
# Installs the genuine SWIG/C++ swss-common Python bindings from the prebuilt
# SONiC Debian packages (dev/vendor/debs/trixie-<arch>/, fetched via
# dev/fetch-swsscommon.sh).
#
# Base = Debian trixie because SONiC master targets trixie and, crucially, trixie
# ships natively every runtime dependency the swss-common deb needs
# (libyang3 3.12.2, libboost-serialization1.83.0, libnl-3, libhiredis, libzmq5),
# so no extra SONiC dependency debs are required -- apt resolves them all.
#
# The repository source is NOT copied in; it is bind-mounted at runtime.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

# Base system: python, pip, git (for sonic-platform-common), redis (available
# for future integration testing), CA certs for https git/pip.
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip git redis-server ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Real swss-common bindings. apt resolves the runtime shared-library deps
# (libyang3, libboost-serialization1.83.0, libnl-*, libhiredis, libzmq5, ...)
# from the stock trixie repositories. DEB_DIR selects the arch-matched folder
# produced by dev/fetch-swsscommon.sh (default arm64; override for amd64).
ARG DEB_DIR=vendor/debs/trixie-arm64
COPY ${DEB_DIR}/ /tmp/debs/
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        /tmp/debs/libswsscommon_*.deb \
        /tmp/debs/python3-swsscommon_*.deb \
        /tmp/debs/sonic-db-cli_*.deb \
    && rm -rf /var/lib/apt/lists/* /tmp/debs

# Python test tooling + deps. Debian marks the system env as externally managed
# (PEP 668); --break-system-packages lets pip install into it for this dev image.
# setuptools is pinned < 81 because the vendored sonic-py-common / sonic-platform-
# common setup.py files `import pkg_resources`, which newer setuptools removed.
RUN pip3 install --no-cache-dir --break-system-packages \
        pytest pytest-cov mock natsort pyyaml packaging redis-dump-load \
        pytest-runner "setuptools<81" wheel

# sonic-py-common (vendored from sonic-buildimage; not on PyPI).
COPY vendor/sonic-py-common/ /opt/sonic-py-common/
RUN pip3 install --no-cache-dir --break-system-packages --no-build-isolation \
        /opt/sonic-py-common

# Stub sonic-config-engine distribution to satisfy sonic-platform-common's
# build-time dependency guard without pulling in the heavy real package.
COPY vendor/sonic-config-engine-stub/ /opt/sonic-config-engine-stub/
RUN pip3 install --no-cache-dir --break-system-packages --no-build-isolation \
        /opt/sonic-config-engine-stub

# sonic-platform-common (master) provides sonic_platform_base.* used by the tests.
RUN pip3 install --no-cache-dir --break-system-packages --no-deps --no-build-isolation \
        "git+https://github.com/sonic-net/sonic-platform-common.git"

# xcvr-emu: software CMIS transceiver emulator (gRPC SfpEmulatorService on :50051)
# + its `cmis` library and generated proto stubs. Upstream pins grpcio==1.51.1,
# which has no Python 3.13 wheels, so install modern grpc/protobuf ourselves and
# add xcvr-emu with --no-deps. Pinned to the reviewed commit for reproducibility.
ARG XCVR_EMU_REF=8e37fb1efa5916038152f2519b3e9d10d1897b01
RUN pip3 install --no-cache-dir --break-system-packages \
        grpcio grpcio-tools protobuf prompt-toolkit \
    && pip3 install --no-cache-dir --break-system-packages --no-deps \
        "git+https://github.com/ishidawataru/xcvr-emu.git@${XCVR_EMU_REF}"

# /dev/log syslog sink (entrypoint) so SONiC SysLogger's SysLogHandler can attach
# on this slim image; 'runtests' convenience command for the interactive shell.
# NOTE: the real bindings installed above are used (no swsscommon stub).
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY runtests /usr/local/bin/runtests
RUN sed -i 's/\r$//' /usr/local/bin/entrypoint.sh /usr/local/bin/runtests \
    && chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/runtests

WORKDIR /work/sonic-xcvrd

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["pytest"]
