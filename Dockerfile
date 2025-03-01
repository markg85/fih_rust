FROM rust:1.85.0-alpine3.21
WORKDIR /src
COPY . .
RUN cargo build --release
CMD cargo run --release
