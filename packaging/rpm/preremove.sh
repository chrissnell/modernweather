#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl stop modernweather.service >/dev/null 2>&1 || true
    systemctl disable modernweather.service >/dev/null 2>&1 || true
fi
exit 0
