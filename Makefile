IMAGE  := daly-bms-exporter-cross-aarch64
TARGET := aarch64-unknown-linux-gnu
BIN    := daly-bms-exporter
REMOTE ?= ratzek

all: debug-build

clean:
	cargo clean

debug-build:
	cargo build

fastdev-build:
	cargo build --profile fastdev

release-build:
	cargo build --release

# Build the aarch64 cross-compile image (cached after the first run).
cross-image:
	docker build -t $(IMAGE) -f scripts/cross-aarch64.Dockerfile scripts

# Cross-compile for aarch64 (Debian 11 / glibc 2.31). Runs as the host user with
# a repo-local CARGO_HOME so target/ and the cache stay host-owned; incremental
# rebuilds are fast. Artifact: target/$(TARGET)/deploy/$(BIN).
cross-build: cross-image
	docker run --rm \
	  --user $$(id -u):$$(id -g) \
	  -e CARGO_HOME=/src/.cross-cargo \
	  -v $(PWD):/src -w /src \
	  $(IMAGE) \
	  cargo build --profile deploy --target $(TARGET)

# Package the cross-compiled aarch64 deploy binary into an arm64 .deb (with the
# systemd unit). Requires cargo-deb (`cargo install cargo-deb`). --no-build uses
# the artifact from cross-build; --no-strip because the deploy profile already
# strips (host strip cannot process the aarch64 binary). --target sets arm64.
# Output: target/debian/daly-bms-exporter_<version>_arm64.deb
deb: cross-build
	@command -v cargo-deb >/dev/null 2>&1 || { echo "cargo-deb not found: run 'cargo install cargo-deb'" >&2; exit 1; }
	cargo deb --no-build --no-strip --target $(TARGET) --profile deploy

# Build the .deb and install it on the production server (default: ratzek).
# Override the host with `make deploy REMOTE=other-host`.
deploy:
	scripts/deploy.sh $(REMOTE)

# Provision every Grafana dashboard in grafana/dashboards (validate JSON, scp
# provider + dashboards, restart grafana-server). Grafana then reloads the files
# every 30s without a restart.
#
# The loop matters: this target used to name daly-bms.json explicitly, so
# daly-bms-bank.json — where most of the bank-total energy panels live — was
# never deployed from the repo at all.
grafana:
	for f in grafana/dashboards/*.json; do jq -e . "$$f" >/dev/null || exit 1; done
	ssh -o LogLevel=ERROR $(REMOTE) 'install -d -o grafana -g grafana /var/lib/grafana/dashboards'
	scp -o LogLevel=ERROR grafana/provisioning/daly-bms.yaml $(REMOTE):/etc/grafana/provisioning/dashboards/daly-bms.yaml
	scp -o LogLevel=ERROR grafana/dashboards/*.json $(REMOTE):/var/lib/grafana/dashboards/
	ssh -o LogLevel=ERROR $(REMOTE) 'chown grafana:grafana /var/lib/grafana/dashboards/*.json && chmod 0644 /var/lib/grafana/dashboards/*.json && systemctl restart grafana-server && sleep 3 && systemctl is-active grafana-server'

# Sync the Prometheus alert rules to the host and reload Prometheus. The repo
# copy is the source of truth (see the header of doc/ratzek-bms.rules); nothing
# else deploys this file.
rules:
	scp -o LogLevel=ERROR doc/ratzek-bms.rules $(REMOTE):/etc/prometheus/rules/ratzek-bms.rules
	ssh -o LogLevel=ERROR $(REMOTE) 'promtool check rules /etc/prometheus/rules/ratzek-bms.rules && systemctl reload prometheus'

.PHONY: all clean debug-build fastdev-build release-build cross-image cross-build deb deploy grafana rules
