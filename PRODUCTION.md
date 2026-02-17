# Production Runbook

## 1. Host prerequisites

Install required build/runtime dependencies:

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake nasm pkg-config
```

Install Rust (if not installed), then build:

```bash
cargo build --release
```

## 2. Secrets and env

Create a dedicated runtime env file:

```bash
cp .env.example .env
```

Set required secrets:

- `POLYMARKET_PRIVATE_KEY`
- `POLYMARKET_PROXY_ADDRESS` (if proxy flow is used)
- `POLY_BUILDER_API_KEY`, `POLY_BUILDER_SECRET`, `POLY_BUILDER_PASSPHRASE` (if merge through relayer is used)

Security defaults:

- Keep `MERGE_ALLOW_UNSAFE_ENDPOINTS=0`
- Keep `RELAYER_URL` on trusted HTTPS endpoint

## 3. Preflight checks

Run:

```bash
./scripts/prod_preflight.sh .env
```

## 4. Systemd deployment

Copy repo to target path (default expected by service file: `/opt/poly_5min_bot`), then install:

```bash
sudo ./scripts/install_systemd_service.sh /opt/poly_5min_bot
```

Edit runtime env:

```bash
sudo nano /etc/poly_5min_bot/poly_5min_bot.env
```

Start and monitor:

```bash
sudo systemctl start poly_5min_bot
sudo systemctl status poly_5min_bot
sudo journalctl -u poly_5min_bot -f
```

## 5. CI gates

GitHub Actions workflow `ci-security.yml` runs:

- formatting check
- clippy with warnings as errors
- full cargo check
- dependency audit via `cargo audit`
