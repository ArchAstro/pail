# pail-web: the UI and its API, plus the pail binary it shells out to, built from this checkout
# so the CLI always has the flags the UI uses. Build from the repository root:
#   docker build -t pail-web -f web/Dockerfile .
#   docker run -p 4747:4747 -v $PWD/pail-web.config.json:/app/pail-web.config.json pail-web
FROM rust:1-slim-bookworm AS pail
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin pail

FROM node:22-slim AS web
WORKDIR /app
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web ./
RUN npm run build && npm prune --omit=dev

FROM node:22-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=pail /src/target/release/pail /usr/local/bin/pail
ENV NODE_ENV=production PAIL_WEB_HOST=0.0.0.0 PAIL_WEB_PORT=4747
WORKDIR /app
COPY --from=web /app/dist ./dist
COPY --from=web /app/node_modules ./node_modules
COPY --from=web /app/package.json ./
EXPOSE 4747
USER node
CMD ["node", "dist/server/index.js"]
