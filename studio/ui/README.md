# Rusty Studio frontend

The default Studio experience is a typed React application. The legacy single-file console remains at
`/advanced/legacy` while specialist workflows move into the new product.

```bash
npm ci
npm run typecheck
npm test
npm run build
python3 ../serve.py --port 8000
```

During development, `npm run dev` serves the app on `http://127.0.0.1:8878` and proxies `/api` to a
local Rusty server on port 8100.
