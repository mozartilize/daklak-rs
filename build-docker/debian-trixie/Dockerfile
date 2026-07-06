ARG features=kde,ibus,evdev_grab
FROM rust:slim-trixie AS builder
ARG features=kde,ibus,evdev_grab

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /opt/daklak

RUN apt-get update
RUN apt-get install -y build-essential git pkg-config libxkbcommon-dev libxcb1-dev zstd dpkg-dev
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
