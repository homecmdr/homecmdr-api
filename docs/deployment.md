# Deployment Guide

This guide covers three ways to run the smart-home server:

1. [Docker (recommended for home servers)](#docker)
2. [Bare metal / systemd](#bare-metal--systemd)
3. [Local dev instance](#local-dev-instance)

---

## Prerequisites

All deployments need:

- A `config/default.toml` (or a copy of it) with your adapter settings and a **strong** `auth.master_key`.
- Network access to whatever adapters you are using (MQTT broker for zigbee2mqtt, local IPs for Elgato lights, etc.).

---

## Docker

### Quick start

```bash
# Clone or copy the repository
git clone https://github.com/anomalyco/smart-home smart-home
cd smart-home

# Copy and edit the config
cp config/default.toml config/local.toml
$EDITOR config/local.toml   # set auth.master_key, adapter settings, etc.

# Build and start
docker compose up -d
```

The API listens on `http://localhost:3001` by default.  
Logs: `docker compose logs -f smart-home`  
Stop: `docker compose down`

### What the compose file does

| Setting | Value |
|---|---|
| Restart policy | `unless-stopped` — survives reboots |
| Config volume | `./config` bind-mounted read-only at `/config` |
| Data volume | named Docker volume `smart-home-data` at `/data` |
| Env vars | `SMART_HOME_CONFIG`, `SMART_HOME_DATA_DIR` |

### Overriding the master key without editing the config file

Create a `.env` file (never commit it):

```
SMART_HOME_MASTER_KEY=my-real-secret-key
```

Then uncomment the `SMART_HOME_MASTER_KEY` line in `docker-compose.yml`.

### Updating to a new image

```bash
docker compose pull   # if using a published image
# or
docker compose build  # rebuild from source
docker compose up -d
```

The SQLite database in the named volume is preserved across updates.

---

## Bare metal / systemd

### 1. Build the binary

```bash
cargo build --release -p api
sudo cp target/release/api /usr/local/bin/smart-home
```

### 2. Create the system user and directories

```bash
sudo useradd --system --no-create-home --shell /sbin/nologin smart-home
sudo mkdir -p /etc/smart-home /var/lib/smart-home
sudo chown smart-home:smart-home /var/lib/smart-home
```

### 3. Install the config

```bash
sudo cp config/default.toml /etc/smart-home/default.toml
sudo $EDITOR /etc/smart-home/default.toml   # set auth.master_key, adapters, etc.
sudo chmod 640 /etc/smart-home/default.toml
sudo chown root:smart-home /etc/smart-home/default.toml
```

Copy Lua asset directories if you use scenes, automations, or scripts:

```bash
sudo cp -r config/scenes      /etc/smart-home/scenes
sudo cp -r config/automations /etc/smart-home/automations
sudo cp -r config/scripts     /etc/smart-home/scripts
sudo chown -R smart-home:smart-home /etc/smart-home/scenes \
    /etc/smart-home/automations /etc/smart-home/scripts
```

Update the directory paths in `/etc/smart-home/default.toml`:

```toml
[scenes]
directory = "/etc/smart-home/scenes"

[automations]
directory = "/etc/smart-home/automations"

[scripts]
directory = "/etc/smart-home/scripts"
```

### 4. Store the master key securely (optional)

Instead of putting the key directly in the config, use an `EnvironmentFile`:

```bash
sudo tee /etc/smart-home/secrets.env > /dev/null <<'EOF'
SMART_HOME_MASTER_KEY=your-real-key-here
EOF
sudo chmod 600 /etc/smart-home/secrets.env
sudo chown root:smart-home /etc/smart-home/secrets.env
```

Uncomment the `EnvironmentFile` line in the unit file (step 5).

### 5. Install and start the systemd unit

```bash
sudo cp deploy/smart-home.service /etc/systemd/system/smart-home.service
sudo systemctl daemon-reload
sudo systemctl enable --now smart-home
```

Check status and logs:

```bash
sudo systemctl status smart-home
sudo journalctl -u smart-home -f
```

### Updating the binary

```bash
cargo build --release -p api
sudo cp target/release/api /usr/local/bin/smart-home
sudo systemctl restart smart-home
```

---

## Local dev instance

Run a second instance on a different port alongside the primary one — no code changes required, just different config values.

### Create a dev config

```bash
cp config/default.toml config/dev.toml
```

Edit `config/dev.toml`:

```toml
[api]
bind_address = "127.0.0.1:3002"   # different port

[persistence]
database_url = "sqlite://data/dev-smart-home.db"   # different DB file

[adapters.zigbee2mqtt]
client_id = "smart-home-dev"   # different MQTT client ID (avoids session conflicts)
```

### Run the dev instance

```bash
SMART_HOME_CONFIG=config/dev.toml cargo run -p api
```

Or set the env var once in your shell:

```bash
export SMART_HOME_CONFIG=config/dev.toml
cargo run -p api
```

The dev instance is fully independent: separate port, separate database, separate MQTT session.

---

## First login

Regardless of deployment method, authenticate using your master key:

```bash
curl -s http://localhost:3001/health

# All other endpoints require a Bearer token:
curl -s -H "Authorization: Bearer YOUR_MASTER_KEY" http://localhost:3001/devices

# Create a scoped API key (write role, for scripts/automations):
curl -s -X POST http://localhost:3001/auth/keys \
  -H "Authorization: Bearer YOUR_MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"label": "home-scripts", "role": "write"}'
```
