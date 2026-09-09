# Deployment

Aegis is a single process backed by SQLite. Persist `/data`, keep the configuration file outside the image, and place TLS at a reverse proxy when clients connect over a network.

- [Container](container.md) - one Podman or Docker container.

The container runs as UID and GID `65532`, listens on port `8765`, and stores the database and root key under `/data`.
