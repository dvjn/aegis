# aegis

> personal llm gateway

Hourly usage migrations create schema only. After the server starts, a background
worker rebuilds reporting buckets from the oldest pending hour, up to 32 requests
per batch. It reads tool payloads outside SQLite's write lock and commits each
request separately. Request checkpoints survive restarts; logs report the hour,
processed requests, and batch duration. Tool reports lag behind incoming traffic
and historical reports remain partial while a rebuild runs. Historical price
updates queue another rebuild. Initialization clears derived buckets and request
checkpoints in one transaction before the incremental scan begins.
