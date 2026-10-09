#!/bin/sh
# Entrypoint for the test Postgres: creates a throwaway CA and server
# certificate in /certs (once), then starts Postgres with TLS enabled.
# Plaintext connections stay allowed, so both modes can be benchmarked.
set -e
if [ ! -f /certs/ca.pem ]; then
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=wasm-server test CA" \
        -keyout /certs/ca.key -out /certs/ca.pem 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=postgres" \
        -keyout /certs/server.key -out /certs/server.csr 2>/dev/null
    printf 'subjectAltName=DNS:postgres,DNS:localhost,IP:127.0.0.1\n' > /certs/san.ext
    openssl x509 -req -in /certs/server.csr -CA /certs/ca.pem -CAkey /certs/ca.key -CAcreateserial \
        -days 3650 -extfile /certs/san.ext -out /certs/server.pem 2>/dev/null
fi
# Postgres refuses a key that other users can read, and bind mounts may not
# preserve ownership, so use private copies.
mkdir -p /tls
cp /certs/server.pem /certs/server.key /tls/
chown postgres:postgres /tls/server.pem /tls/server.key
chmod 600 /tls/server.key
exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.pem -c ssl_key_file=/tls/server.key "$@"
