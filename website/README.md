# vmux.sh

The vmux website is a dependency-free static site. It uses real vmux Remote
captures from the companion app repository and ships directly from this folder.

## Preview locally

```sh
python3 -m http.server 4173 --directory website
```

Then open `http://localhost:4173`.

`.github/workflows/pages.yml` is a manually triggered fallback for a future
off-machine GitHub Pages migration. It does not deploy on pushes.

The site contains no build step, analytics, cookies, or third-party JavaScript.
