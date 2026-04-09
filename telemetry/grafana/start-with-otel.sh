#!/bin/bash
# Wrapper entrypoint: starts the original DocumentDB entrypoint,
# then launches OTel Collector once the gateway is ready.

# Start OTel in the background after a delay to let PG + gateway start
(
    # Wait for gateway to be ready on port 10260
    for i in $(seq 1 120); do
        if nc -z localhost 10260 2>/dev/null; then
            echo "[OTEL] Gateway is ready, starting OTel Collector..."
            export PG_CONN_STRING="host=localhost port=9712 user=documentdb dbname=postgres sslmode=disable"
            otelcol-contrib \
                --config=file:/etc/otel/engine_metrics.yaml \
                --config=file:/etc/otel/host_metrics.yaml \
                --config=file:/etc/otel/base.yaml &
            echo "[OTEL] OTel Collector started (PID: $!)"
            exit 0
        fi
        sleep 2
    done
    echo "[OTEL] Gateway did not start within 240s, skipping OTel Collector"
) &

# Run the original entrypoint
exec /home/documentdb/gateway/scripts/emulator_entrypoint.sh "$@"
