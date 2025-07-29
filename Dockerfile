FROM rust:1.88.0-alpine3.22
RUN apk add --no-cache musl-dev gcc binutils pkgconfig openssl-dev dav1d-dev openssl dav1d openssl-libs-static libc6-compat
ENV RUSTFLAGS='-C target-feature=-crt-static'
ENV OPENSSL_DIR=/usr
WORKDIR /src
COPY . .
RUN cargo build --release
CMD cargo run --release
