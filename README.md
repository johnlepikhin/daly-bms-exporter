# daly-bms-exporter

Prometheus exporter for **Daly R24TK** BMS telemetry delivered over the Hlktech
WiFi module. The module periodically POSTs raw Modbus frames (in cleartext JSON)
to the cloud service `www.databms.com`. This exporter impersonates that endpoint:
you redirect the module's traffic to it, it decodes the frames and exposes the
telemetry on `/metrics`.

See `doc/daly-bms-protocol.md` for the full wire protocol.

## Build & run

```bash
cargo build --release
./target/release/daly-bms-exporter --config config.example.yaml
```

For the aarch64 (Raspberry Pi / Debian 11) target: `make cross-build`.

## Debian package (aarch64)

```bash
make deb          # cross-compiles, then packages -> target/debian/*_arm64.deb
```

Requires Docker (for the cross build) and `cargo-deb` (`cargo install cargo-deb`).
The package (`arm64`, `Depends: libc6`) installs:

- `/usr/bin/daly-bms-exporter`
- `/etc/daly-bms-exporter/config.example.yaml` (copy to `config.yaml` and edit)
- `/usr/lib/systemd/system/daly-bms-exporter.service` — enabled on install but
  **not started** (create `/etc/daly-bms-exporter/config.yaml` first, then
  `systemctl start daly-bms-exporter`).

The unit runs under `DynamicUser` with `CAP_NET_BIND_SERVICE`, so `listen` may be
set to a privileged port (e.g. `:80`, which the Hlktech module targets).

## Deploy

```bash
make deploy                 # build the .deb and install it on ratzek
make deploy REMOTE=other    # or a different SSH host
```

`scripts/deploy.sh [host]` cross-builds the `.deb`, verifies the packaged binary
stays within the target's glibc ceiling (2.31, same safety net as the reference
project), `scp`s it over, and installs it with `apt-get install` (dependency
resolution, `dpkg -i` fallback). On the first deploy it seeds
`/etc/daly-bms-exporter/config.yaml` from the example, then restarts the service
and reports its status. Requires Docker + `cargo-deb` locally and root SSH to the
target.

## Configuration

YAML, see `config.example.yaml`. Key fields: `listen` (bind address **and port**),
`metrics_path`, `log_level`, `allowed_serials`, `max_body_bytes`,
`request_timeout_secs`. A missing file uses defaults (`0.0.0.0:8080`).

## Redirecting device traffic

The module sends `POST http://www.databms.com:80/api/v2/http2/SaveThingInfo1`.
To make it reach this exporter you must intercept **both the hostname and port 80**:

1. **DNS override** — point `www.databms.com` at the exporter host on your LAN
   (dnsmasq / Pi-hole / router static A-record).
2. **Port 80** — the module always uses port 80, but the exporter defaults to
   `:8080`. Either:
   - set `listen: "0.0.0.0:80"` and grant the service the capability to bind a
     privileged port (systemd: `AmbientCapabilities=CAP_NET_BIND_SERVICE`), or
   - keep `:8080` and DNAT `:80 → :8080` on the host
     (`iptables -t nat -A PREROUTING -p tcp --dport 80 -j REDIRECT --to-port 8080`).

Verify with `curl -XPOST http://<host>/api/v2/http2/SaveThingInfo1 -d @body.json`
and scrape `http://<host>/metrics`.

## Terminate vs forward

This exporter **terminates** the databms.com HTTP channel: it answers `200 OK`
and does not forward the POST upstream. The vendor mobile app's "Remote
connection" reads telemetry through the independent MQTT/Aliyun channel (protocol
doc §1), so terminating does not break the app. The trade-off: the history shown
on the `databms.com` web portal stops updating. Forwarding onward is a possible
future extension (it needs the real upstream IP, since DNS is hijacked).

## Security

The ingest endpoint is plain HTTP with **no authentication** — the firmware's
`Signature` header can't be verified (the signing secret is unknown, doc §2.3).
Anyone able to reach the port can inject metrics. Mitigations:

- Firewall the ingest port to the LAN / the device only.
- Set `allowed_serials` to restrict which BMS serials produce series (bounds
  metric cardinality against a hostile or misconfigured sender).
- In production, firewall the ingest port to the device/LAN **and** set
  `allowed_serials` to the known device serials. `max_devices` is a hard cap
  with no eviction and slots are claimed first-come, so without an allowlist an
  unauthenticated flood of distinct serials can exhaust the cap and lock out
  real devices.
- `/metrics` exposes device serials and firmware versions — restrict scraping to
  your Prometheus.

## Status / known limitation

Register-offset decoding is validated against the protocol doc's per-register
examples and formulas, but **no real captured frame is bundled yet**, so the
offset→field mapping is unverified against live hardware. To close this: capture
one real POST body (tcpdump / iptables) into `tests/fixtures/telemetry_sample.json`
and add a golden test.
