ARG features=kde,ibus,evdev_grab
FROM archlinux:base-devel AS builder
ARG features=kde,ibus,evdev_grab

WORKDIR /opt/daklak

RUN pacman -Syu --noconfirm
RUN pacman -S --noconfirm --needed rustup pkg-config libxkbcommon libxcb git zstd
RUN useradd -m builder && mkdir -pv /opt/daklak-out

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY vendors/vnkey/ vendors/vnkey/
COPY res/ res/
COPY scripts/ scripts/
COPY ci/ ci/
COPY meson.build meson_options.txt ./

RUN chown -R builder:builder /opt/daklak /opt/daklak-out
USER builder
ENV PATH="/home/builder/.cargo/bin:${PATH}"

RUN rustup default stable

RUN cargo fetch

RUN scripts/build.sh -r ${features:+-f "$features"} && \
    scripts/release-arch.sh /opt/daklak-out

FROM scratch AS export
COPY --from=builder /opt/daklak-out/ /
