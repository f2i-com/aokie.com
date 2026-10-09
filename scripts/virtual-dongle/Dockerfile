# A Linux box for scripts/virtual-dongle/run.sh: Rust, libusb with its udev half, and the USB/IP tools.
#
#     docker build -t aokie-virtual-dongle scripts/virtual-dongle
#     docker run --rm --privileged -v /lib/modules:/lib/modules:ro -v "$PWD":/src:ro aokie-virtual-dongle
#
# --privileged and the kernel's modules: the stand-in dongle is attached through the kernel's vhci-hcd, which the
# container loads into the kernel it shares with its host (Docker Desktop: its own Linux VM, not your computer's
# system). The repository is mounted read-only; the build goes to /target inside the container.
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
      libusb-1.0-0-dev libudev-dev pkg-config usbip kmod python3 \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_TARGET_DIR=/target CARGO_TERM_COLOR=never
WORKDIR /src
CMD ["sh", "/src/scripts/virtual-dongle/run.sh"]
