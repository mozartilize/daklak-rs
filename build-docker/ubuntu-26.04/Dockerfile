ARG features=kde,ibus,evdev_grab
FROM ubuntu:26.04 AS builder
ARG features=kde,ibus,evdev_grab

ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/root/.cargo/bin:$PATH

WORKDIR /opt/daklak

RUN apt-get update
RUN apt-get install -y curl build-essential git pkg-config libxkbcommon-dev libxcb1-dev zstd dpkg-dev
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path --profile minimal
RUN mkdir -pv /opt/daklak-out

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY vendors/vnkey/ vendors/vnkey/
COPY res/ res/
COPY scripts/ scripts/
COPY ci/ ci/
COPY meson.build meson_options.txt ./

RUN cargo fetch

RUN scripts/build.sh -r ${features:+-f "$features"} && \
    scripts/release-deb.sh /opt/daklak-out && \
    cp build/out/daklak /opt/daklak-out/

FROM scratch AS export
COPY --from=builder /opt/daklak-out/ /
