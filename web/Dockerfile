# pail-web: the UI and its API, plus the pail binary it shells out to.
#   docker build -t pail-web -f web/Dockerfile web
#   docker run -p 4747:4747 -e PAIL_WEB_HOST=0.0.0.0 -v $PWD/pail-web.config.json:/app/pail-web.config.json pail-web
FROM node:22-slim AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build && npm prune --omit=dev

FROM node:22-slim
# The pail CLI: install from the release script (or COPY a binary you built).
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && curl -fsSL https://pail.build/install.sh | sh \
    && rm -rf /var/lib/apt/lists/*
ENV PATH="/root/.local/bin:/usr/local/bin:${PATH}" \
    NODE_ENV=production PAIL_WEB_HOST=0.0.0.0 PAIL_WEB_PORT=4747
WORKDIR /app
COPY --from=build /app/dist ./dist
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/package.json ./
EXPOSE 4747
USER node
CMD ["node", "dist/server/index.js"]
