#!/bin/sh
set -e
getent group modernweather >/dev/null || groupadd --system modernweather
getent passwd modernweather >/dev/null || useradd --system --gid modernweather \
    --home-dir /var/lib/modernweather --no-create-home \
    --shell /usr/sbin/nologin --comment "ModernWeather daemon" modernweather
for g in dialout uucp; do
    getent group "$g" >/dev/null && usermod -a -G "$g" modernweather || true
done
exit 0
