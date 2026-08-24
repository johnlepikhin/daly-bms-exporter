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
make deploy                 # build the .deb and install it on the default host (ratzek)
make deploy REMOTE=other    # override the SSH host
```

`scripts/deploy.sh [host]` cross-builds the `.deb`, verifies the packaged
binary's glibc ceiling (≤ 2.31, matching the target), `scp`s it over, and installs it with `apt-get install` (dependency
resolution, `dpkg -i` fallback). On the first deploy it seeds
`/etc/daly-bms-exporter/config.yaml` from the example, then restarts the service
and reports its status. Requires Docker + `cargo-deb` locally and root SSH to the
target.

## Configuration

YAML, see `config.example.yaml` for the full field documentation. Key fields:
`listen` (bind address **and port**), `metrics_path`, `log_level`,
`allowed_serials`, `max_body_bytes`, `request_timeout_secs`,
`coulomb_max_gap_secs` (cap on the integration gap for the coulomb counter, in
seconds), and `max_devices` (hard cap on distinct tracked serials, to bound
metric cardinality; `0` = unlimited). A missing file uses defaults
(`listen: 0.0.0.0:8080`).

The coulomb/energy counters are persisted to `coulomb_state_path`. The write is
durable (temp file, fsync, rename, directory fsync) and happens *before* the
increment reaches the exported counter, so the file can never fall behind
`/metrics`: a counter that comes back lower after a restart is read by Prometheus
as a reset, which adds the whole accumulated total to `increase()`.
`coulomb_state_min_interval_secs` (default 5, clamped to `1..=3600`) rate-limits
those writes; deltas arriving inside the window are held in memory, not dropped.

`max_plausible_current_amperes`, `min_plausible_pack_volts`,
`max_plausible_pack_volts` and `max_frame_amp_hours` / `max_frame_watt_hours`
gate what may enter the counters. The register encoding reaches 3553 A and
6553 V, so without them one corrupt frame would add kilowatt-hours that cannot
be removed from a monotonic counter.

## Metrics & dashboard

`/metrics` exposes Prometheus text-format families, all labelled by `sn` (BMS
serial). Highlights:

- **Realtime telemetry** — `daly_bms_pack_voltage_volts`,
  `daly_bms_current_amperes`, `daly_bms_soc_percent`,
  `daly_bms_cell_voltage_volts{cell}`, `daly_bms_temperature_celsius{sensor}`,
  `daly_bms_charge_cycles`, `daly_bms_alarm{type}`, etc.
- **Config / protection thresholds** — `daly_bms_*_limit_*{level}`.
- **Coulomb counter** — `daly_bms_charge_amp_hours_total` /
  `daly_bms_discharge_amp_hours_total` (integral of current).
- **Energy counter** — `daly_bms_charge_watt_hours_total` /
  `daly_bms_discharge_watt_hours_total` (integral of measured power `V*I`).
- **Staleness** — `daly_bms_last_frame_timestamp_seconds`.
- **Self-observability** — `daly_bms_http_requests_total{endpoint,status}`,
  `daly_bms_frames_decoded_total{block}`,
  `daly_bms_frames_dropped_total{reason}` (frame never reached any metric),
  `daly_bms_coulomb_samples_rejected_total{reason}` (frame was exported as
  gauges but kept out of the counters by the plausibility gate),
  `daly_bms_state_write_errors_total{stage}` and
  `daly_bms_state_last_write_timestamp_seconds` (energy accounting stalls if the
  state file cannot be written — alert on the age of this gauge).
- **Device metadata** —
  `daly_bms_device_info{serial,machine_code,sw_version,hw_version}`.

Ready-made Grafana dashboards live in `grafana/dashboards/` (provider config in
`grafana/provisioning/daly-bms.yaml`): `daly-bms.json` is the fleet overview with
per-device repeat rows and a health section (per-cell imbalance / deviation, SOH
estimate, C-rate, coulomb energy), `daly-bms-bank.json` covers bank totals.
`make grafana REMOTE=<host>` deploys both.

Energy panels sum *clamped* 20-minute increments rather than calling
`increase()` over the whole window, so a counter reset cannot spike them —
including resets already recorded in history. Prometheus alert rules are in
`doc/ratzek-bms.rules`; `make rules REMOTE=<host>` syncs and reloads them.

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
     (`iptables -t nat -A PREROUTING -p tcp --dport 80 -j REDIRECT --to-port 8080`), or
   - if port 80 on the host is already served by a reverse proxy (e.g. nginx),
     skip the DNAT / `CAP_NET_BIND_SERVICE` dance entirely: keep the DNS override
     and add an nginx `server` block (`server_name www.databms.com;`) that
     `proxy_pass`es to the exporter on `:8080`. No iptables needed.

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
examples and formulas, and the decoder has been **empirically validated against a
live device** — the decoded values are physically consistent (average cell
voltage × N ≈ pack voltage; max / min / delta agree). What's still missing is a
committed regression fixture: no captured raw frame is yet bundled as a golden
test in `tests/fixtures/`. To close this: capture one real POST body (tcpdump /
iptables) into `tests/fixtures/telemetry_sample.json` and add a golden test.

## Releases

Pushing a `vX.Y.Z` tag triggers the GitHub Actions release workflow: it builds
the arm64 `.deb` (glibc-gated ≤ 2.31) and publishes it, together with its
`SHA256SUMS`, on a GitHub Release.
