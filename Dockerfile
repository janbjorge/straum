# straum as one static binary on musl, in a distroless image that holds
# nothing else: no shell, no package manager, user 65532. The Rust version
# is the one rust-toolchain.toml pins. Images are pinned by digest; a
# platform other than the build machine's builds under emulation.
FROM rust:1.100.0-alpine@sha256:de1e0058543a81ed7b046629167db587c67a94bdf37cce14eb27f0ec83baaffb AS build
WORKDIR /src
# The version straum prints, from git; unset, it is Cargo.toml's.
ARG STRAUM_VERSION
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p straum \
    && cp target/release/straum /straum

FROM gcr.io/distroless/static-debian13:nonroot@sha256:25ac8f0c3b2334a9eb05452266bd4697864a8c8ef0ef71e5e6480d7f92868329
# GHCR links the package to the repository through this label, whoever
# pushes the image.
LABEL org.opencontainers.image.source=https://github.com/janbjorge/straum
COPY --from=build /straum /straum
EXPOSE 8080
ENTRYPOINT ["/straum"]
CMD ["help"]
