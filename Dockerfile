# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm AS build
WORKDIR /app

# Build the dependency graph against a stub first, so editing src/ doesn't
# recompile serenity on every image build.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
# Cargo keys off mtime; the stub's timestamp would otherwise look current.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
# TLS to Discord, OpenAI and GitHub needs the root store.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home bot

COPY --from=build /app/target/release/discord-issue-bot /usr/local/bin/discord-issue-bot

USER bot
ENV PORT=8080
EXPOSE 8080
CMD ["discord-issue-bot"]
