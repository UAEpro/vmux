# vmux.sh

The vmux website is a dependency-free static site. It uses real vmux Remote
captures from the companion app repository and ships directly from this folder.

## Preview locally

```sh
python3 -m http.server 4173 --directory website
```

Then open `http://localhost:4173`.

## Production deployment

The production site is copied to `/var/www/vmux.sh` and served on
`127.0.0.1:4173` by `vmux-site.service`. The existing system
`cloudflared.service` publishes that origin at `https://vmux.sh`.

Deploy an update from the repository root:

```sh
./ops/deploy-site.sh
```

The script copies only public website files, installs the hardened static server,
and restarts the origin. `journalctl -u vmux-site` shows origin logs and
`journalctl -u cloudflared` shows tunnel logs.

`.github/workflows/pages.yml` is a manually triggered fallback for a future
off-machine GitHub Pages migration. It does not deploy on pushes.

The site contains no build step, analytics, cookies, or third-party JavaScript.
