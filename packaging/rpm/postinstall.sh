#!/bin/sh
set -e
mkdir -p /var/lib/modernweather
chown modernweather:modernweather /var/lib/modernweather
chmod 750 /var/lib/modernweather
# config.db is never created or overwritten here; see packaging/deb/postinst.
if [ -f /var/lib/modernweather/config.db ]; then
    chown modernweather:modernweather /var/lib/modernweather/config.db
    chmod 640 /var/lib/modernweather/config.db
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable modernweather.service >/dev/null 2>&1 || true
    systemctl try-restart modernweather.service >/dev/null 2>&1 || true
fi
exit 0
