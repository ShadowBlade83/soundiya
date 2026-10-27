#!/bin/sh
DIR=/data/opt/soundiya
PIDFILE=/run/soundiya.pid
if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null; then
    echo 'soundiya is already running'
    exit 0
fi
rm -f /run/soundiya.stop
nohup setsid "$DIR/run.sh" </dev/null >/dev/null 2>&1 &
# A cold start from slow flash can take a few seconds to write the pidfile.
n=0
while [ ! -f "$PIDFILE" ]; do
    n=$((n + 1))
    if [ "$n" -gt 20 ]; then
        echo 'soundiya failed to start' >&2
        exit 1
    fi
    sleep 1
done
echo 'soundiya started; log: /tmp/soundiya.log'
