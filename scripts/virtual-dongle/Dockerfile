# A Linux box for scripts/virtual-dongle/run.sh: Rust, libusb with its udev half, and the USB/IP tools; and, for
# WITH_PLUGIN=1 (the whole plugin on the stand-in), what the plugin's own build asks of a Linux system.
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
      cmake wget gnupg libasound2-dev libssl-dev libglib2.0-dev libx11-dev libxext-dev \
    && rm -rf /var/lib/apt/lists/*
# The plugin links libwebrtc, which is built against a libc++ of its own that asks for clang 21 or later; Debian 12
# has 14. (The transport test alone does not need it.)
RUN wget -qO /etc/apt/trusted.gpg.d/apt.llvm.org.asc https://apt.llvm.org/llvm-snapshot.gpg.key \
    && echo "deb http://apt.llvm.org/bookworm/ llvm-toolchain-bookworm-21 main" > /etc/apt/sources.list.d/llvm21.list \
    && apt-get update && apt-get install -y --no-install-recommends clang-21 lld-21 libclang-21-dev \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_TARGET_DIR=/target CARGO_TERM_COLOR=never CC=clang-21 CXX=clang++-21 LIBCLANG_PATH=/usr/lib/llvm-21/lib
WORKDIR /src
CMD ["sh", "/src/scripts/virtual-dongle/run.sh"]
