# Rusty Studio frontend

Rusty Studio is a typed React application.

```bash
npm ci
npm run typecheck
npm test
npm run build
python3 ../serve.py --port 8000
```

During development, `npm run dev` serves the app on `http://127.0.0.1:8878` and proxies `/api` to a
local Rusty server on port 8100.
