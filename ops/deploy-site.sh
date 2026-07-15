#!/bin/sh
set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
site_root="/var/www/vmux.sh"
server_root="/usr/local/lib/vmux-site"

sudo install -d -o root -g root -m 0755 "$site_root" "$server_root"
sudo rsync -a --delete --chmod=D755,F644 \
  --exclude README.md \
  --exclude CNAME \
  --exclude .nojekyll \
  "$repo_root/website/" "$site_root/"
sudo install -o root -g root -m 0755 "$repo_root/ops/vmux-site-server.py" "$server_root/server.py"
sudo install -o root -g root -m 0644 "$repo_root/ops/vmux-site.service" /etc/systemd/system/vmux-site.service
sudo systemctl daemon-reload
sudo systemctl enable vmux-site.service >/dev/null
sudo systemctl restart vmux-site.service

for attempt in 1 2 3 4 5 6 7 8 9 10; do
  if curl -fs http://127.0.0.1:4173/ >/dev/null; then
    break
  fi
  if [ "$attempt" -eq 10 ]; then
    printf 'vmux.sh origin did not become ready\n' >&2
    exit 1
  fi
  sleep 0.3
done
printf 'Deployed vmux.sh origin from %s\n' "$repo_root/website"
