#!/bin/sh
# wasm-server on a Linux server (Debian or Ubuntu with systemd), as a
# service with HTTPS. Usually run for you by `plumb server add root@HOST`;
# by hand, as root:
#
#   curl -fsSL https://raw.githubusercontent.com/semenov/wasm-server/main/install.sh | sh -s -- --domain example.com
#
#   --domain D        projects answer at PROJECT.D, the server's own API at
#                     platform.D: one DNS record, *.D → this server's address
#   --ip A.B.C.D      without --domain: use A-B-C-D.sslip.io, which needs no DNS
#   --email E         for Let's Encrypt's expiry warnings
#   --version vX.Y.Z  a release (the latest by default)
#   --binary FILE     install this binary instead of downloading one
#   --staging         Let's Encrypt's test CA (for trying a setup)
#   --acme-directory URL, --acme-ca-file FILE   another ACME CA (tests)
#   --print-token     end with KEY=VALUE lines for a program to read
#   --behind-caddy    next to a Caddy that already has ports 80 and 443 (say,
#                     homebase's): wasm-server listens on 127.0.0.1:18080
#                     (--listen to change it) and Caddy, given one more site
#                     block, serves *.D with a wildcard certificate (it needs
#                     Caddy's Cloudflare DNS module and token, as homebase sets
#                     them up) and passes the requests on
#
# Running it again upgrades the binary and keeps the data, the token and
# the settings (pass --domain again to change the domain). Everything it
# makes: /usr/local/bin/wasm-server, /etc/wasm-server/env (settings and the
# admin token, readable by root and the service), /var/lib/wasm-server
# (the data), the user wasm-server, and the systemd service wasm-server.
set -eu

REPO=semenov/wasm-server
DOMAIN="" IP="" EMAIL="" VERSION=latest BINARY="" ACME_DIR="" ACME_CA="" PRINT="" CADDY="" LISTEN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN=$2; shift 2 ;;
    --ip) IP=$2; shift 2 ;;
    --email) EMAIL=$2; shift 2 ;;
    --version) VERSION=$2; shift 2 ;;
    --binary) BINARY=$2; shift 2 ;;
    --staging) ACME_DIR=staging; shift ;;
    --acme-directory) ACME_DIR=$2; shift 2 ;;
    --acme-ca-file) ACME_CA=$2; shift 2 ;;
    --print-token) PRINT=1; shift ;;
    --behind-caddy) CADDY=1; shift ;;
    --listen) LISTEN=$2; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "error: unknown option $1 (see the top of install.sh)" >&2; exit 2 ;;
  esac
done

say() { echo "→ $*" >&2; }
die() { echo "error: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run it as root (sudo sh install.sh ...)"
[ "$(uname -s)" = Linux ] || die "wasm-server runs on Linux"
command -v systemctl >/dev/null 2>&1 || die "this needs systemd (Debian, Ubuntu)"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "no wasm-server build for $(uname -m)" ;;
esac

ENV=/etc/wasm-server/env
DATA=/var/lib/wasm-server
mkdir -p /etc/wasm-server
touch "$ENV"
get() { sed -n "s/^$1=//p" "$ENV" | tail -1; }
put() {
  grep -v "^$1=" "$ENV" > "$ENV.new" || true
  echo "$1=$2" >> "$ENV.new"
  mv "$ENV.new" "$ENV"
}

# the domain: given, kept from before, or this server's address on sslip.io
if [ -z "$DOMAIN" ]; then
  DOMAIN=$(get WASM_SERVER_APPS_DOMAIN)
fi
if [ -z "$DOMAIN" ]; then
  [ -n "$IP" ] || IP=$(hostname -I 2>/dev/null | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | grep -vE '^(10|127)\.|^192\.168\.|^172\.(1[6-9]|2[0-9]|3[01])\.' | head -1)
  [ -n "$IP" ] || die "give --domain example.com (with *.example.com pointing here), or --ip with this server's public address"
  DOMAIN=$(echo "$IP" | tr . -).sslip.io
fi
DOMAIN=$(echo "$DOMAIN" | sed 's/^\*\.//' | tr 'A-Z' 'a-z')

# the binary
say "installing wasm-server ($ARCH)"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
if [ -n "$BINARY" ]; then
  cp "$BINARY" "$TMP/wasm-server"
else
  command -v curl >/dev/null 2>&1 || die "curl is needed to download the server (apt-get install curl)"
  if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"; else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi
  ASSET=wasm-server-linux-$ARCH.gz
  curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET" || die "can't download $BASE/$ASSET"
  curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" || die "can't download the checksums"
  (cd "$TMP" && grep " $ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null) || die "the download doesn't match its checksum"
  gunzip -c "$TMP/$ASSET" > "$TMP/wasm-server"
fi
chmod 755 "$TMP/wasm-server"
"$TMP/wasm-server" --help >/dev/null 2>&1 || die "the binary doesn't run here (wrong architecture or C library?)"
mv "$TMP/wasm-server" /usr/local/bin/wasm-server

# the user, the data, the settings
id wasm-server >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin wasm-server
mkdir -p "$DATA"
chown wasm-server:wasm-server "$DATA"
chmod 750 "$DATA"
TOKEN=$(get WASM_SERVER_ADMIN_TOKEN)
if [ -z "$TOKEN" ]; then
  TOKEN=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
  put WASM_SERVER_ADMIN_TOKEN "$TOKEN"
fi
# where it listens: ports 80 and 443 itself, or behind Caddy
[ -n "$CADDY" ] || [ -z "$(get WASM_SERVER_LISTEN)" ] || [ -n "$LISTEN" ] || CADDY=1
if [ -n "$CADDY" ]; then
  [ -n "$LISTEN" ] || LISTEN=$(get WASM_SERVER_LISTEN)
  [ -n "$LISTEN" ] || LISTEN=127.0.0.1:18080
  put WASM_SERVER_LISTEN "$LISTEN"
  put WASM_SERVER_SERVE "--listen $LISTEN"
else
  put WASM_SERVER_LISTEN ""
  put WASM_SERVER_SERVE "--https --listen 0.0.0.0:443 --http-listen 0.0.0.0:80"
fi
put WASM_SERVER_APPS_DOMAIN "$DOMAIN"
put WASM_SERVER_NAME "platform.$DOMAIN"
EXTRA=$(get WASM_SERVER_EXTRA)
if [ -n "$EMAIL$ACME_DIR$ACME_CA" ]; then
  EXTRA=""
  [ -n "$EMAIL" ] && EXTRA="$EXTRA --acme-email $EMAIL"
  [ -n "$ACME_DIR" ] && EXTRA="$EXTRA --acme-directory $ACME_DIR"
  [ -n "$ACME_CA" ] && EXTRA="$EXTRA --acme-ca-file $ACME_CA"
fi
put WASM_SERVER_EXTRA "$EXTRA"
chown root:wasm-server "$ENV"
chmod 640 "$ENV"

# ports 80 and 443 must be free (or ours); behind Caddy, its port
if [ -n "$CADDY" ]; then
  command -v caddy >/dev/null 2>&1 && [ -f /etc/caddy/Caddyfile ] || die "--behind-caddy: there's no Caddy here (/etc/caddy/Caddyfile)"
  PORT=${LISTEN##*:}
  BUSY=$(ss -ltnpH 2>/dev/null | grep -E ":$PORT\s" | grep -v wasm-server || true)
  [ -z "$BUSY" ] || die "port $PORT is taken; pick another with --listen 127.0.0.1:PORT:
$BUSY"
elif command -v ss >/dev/null 2>&1; then
  BUSY=$(ss -ltnpH 2>/dev/null | grep -E ':(80|443)\s' | grep -v wasm-server || true)
  [ -z "$BUSY" ] || die "ports 80 or 443 are taken by another program; stop it first:
$BUSY"
fi

cat > /etc/systemd/system/wasm-server.service <<'UNIT'
[Unit]
Description=wasm-server: Plumb projects with HTTPS
After=network-online.target
Wants=network-online.target

[Service]
User=wasm-server
Group=wasm-server
EnvironmentFile=/etc/wasm-server/env
ExecStart=/usr/local/bin/wasm-server serve --data-dir /var/lib/wasm-server $WASM_SERVER_SERVE $WASM_SERVER_EXTRA
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=2
LimitNOFILE=1048576
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=/var/lib/wasm-server

[Install]
WantedBy=multi-user.target
UNIT

if [ -z "$CADDY" ] && command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
  say "opening ports 80 and 443 in ufw"
  ufw allow 80/tcp >/dev/null
  ufw allow 443/tcp >/dev/null
fi

systemctl daemon-reload
systemctl enable wasm-server >/dev/null 2>&1
systemctl restart wasm-server
say "starting"
# its port answers once it's up
if [ -n "$CADDY" ]; then PROBE=http://$LISTEN/; PORT=${LISTEN##*:}; else PROBE=http://127.0.0.1/; PORT=80; fi
up() {
  if command -v curl >/dev/null 2>&1; then curl -s -o /dev/null "$PROBE" 2>/dev/null
  else ss -ltnH 2>/dev/null | grep -qE ":$PORT\s"; fi
}
i=0
until up; do
  i=$((i + 1))
  if [ $i -gt 30 ] || ! systemctl is-active --quiet wasm-server; then
    journalctl -u wasm-server -n 20 --no-pager >&2 || true
    die "wasm-server didn't start (its log is above)"
  fi
  sleep 1
done

# Caddy: one site block for *.DOMAIN, passing requests on (a wildcard
# certificate through Cloudflare's DNS, as homebase sets it up). Applied
# as homebase does: if Caddy rejects it, the previous config stays.
if [ -n "$CADDY" ]; then
  caddy list-modules 2>/dev/null | grep -q '^dns.providers.cloudflare$' || die "Caddy has no Cloudflare DNS module: a wildcard certificate for *.$DOMAIN needs one (homebase server add --wildcard cloudflare sets it up)"
  [ -f /etc/caddy/dns.env ] && grep -q '^CLOUDFLARE_API_TOKEN=' /etc/caddy/dns.env || die "Caddy has no CLOUDFLARE_API_TOKEN (/etc/caddy/dns.env)"
  if [ -d /etc/caddy/ship ]; then
    SITE=/etc/caddy/ship/_wasm-server.caddy   # homebase leaves names with _ alone
  else
    SITE=/etc/caddy/wasm-server.caddy
    grep -qF "import $SITE" /etc/caddy/Caddyfile || die "add \"import $SITE\" to /etc/caddy/Caddyfile first"
  fi
  NEW="# wasm-server (installed by its install.sh, not by homebase): Plumb projects
*.$DOMAIN {
	tls {
		dns cloudflare {env.CLOUDFLARE_API_TOKEN}
	}
	reverse_proxy $LISTEN
}
"
  OLD=""
  [ -f "$SITE" ] && OLD=$(cat "$SITE")
  if [ "$OLD" != "$(printf '%s' "$NEW")" ]; then
    say "adding *.$DOMAIN to Caddy ($SITE)"
    printf '%s' "$NEW" > "$SITE"
    if ! (set -a; . /etc/caddy/dns.env; caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile) >"$TMP/caddy.log" 2>&1; then
      if [ -n "$OLD" ]; then printf '%s\n' "$OLD" > "$SITE"; else rm -f "$SITE"; fi
      (set -a; . /etc/caddy/dns.env; caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile) >/dev/null 2>&1 || true
      tail -20 "$TMP/caddy.log" >&2
      die "Caddy rejected the site block, so it was taken out again; nothing else changed in Caddy"
    fi
  fi
fi

if [ -n "$PRINT" ]; then
  echo "WASM_SERVER_URL=https://platform.$DOMAIN"
  echo "WASM_SERVER_APPS_DOMAIN=$DOMAIN"
  echo "WASM_SERVER_ADMIN_TOKEN=$TOKEN"
else
  cat <<DONE

wasm-server is running.

  Projects   https://PROJECT.$DOMAIN
  Server     https://platform.$DOMAIN
  Token      $TOKEN   (also in $ENV)

DNS: point *.$DOMAIN at this server (an A record); sslip.io names need nothing.
Then on your computer: plumb login https://platform.$DOMAIN
DONE
fi
