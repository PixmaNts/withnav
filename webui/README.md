# Navai WebUI

A browser client of the Navai session server: the same HTTP/SSE API the terminal client and the
VS Code extension use (`src/server/routes.rs`). Vite + React + TypeScript, plain CSS with the four
themes of `docs/design/README.md`. Fonts and icons are bundled; nothing loads from a CDN.

## Develop

```sh
# 1. A scratch server, so the real history stays untouched (port 7311 is what Vite proxies to):
XDG_DATA_HOME=/tmp/opencode/navai-webui-data navai serve --bind 127.0.0.1:7311

# 2. The app, with hot reload, on http://localhost:5173
cd webui
npm install
npm run dev
```

- `NAVAI_SERVER` changes the proxied server (default `http://127.0.0.1:7311`).
- `/v1/*` is proxied as is, event streams included.
- `GET /v1/workspaces` and `GET /v1/intent` fall back to a dev-only stand-in (`dev/fallbacks.ts`)
  when the server answers 404. The stand-in lists the folders of `NAVAI_WORKSPACES`
  (colon-separated, default `/tmp/opencode/navai-webui-ws`), with their git branch and changed files,
  and reads `.navigator/intent.md` from disk. A server that has these routes is always used instead.
- In dev there is no token. In production `navai web` opens `http://127.0.0.1:<port>/#token=<token>`:
  the app reads the token, keeps it in `sessionStorage` for that tab, removes it from the address bar
  and sends it as `Authorization: Bearer` on every `/v1` call. A 401 shows "Session expired".

## Check

```sh
npm test            # vitest: SSE decoder, transcript projection (with recorded streams), compose routing
npx tsc --noEmit    # strict type check
npm run build       # type check + production build into webui/dist/
```

## Build and commit `dist/`

The server serves `webui/dist/` as it is in the repository, so **after any change under `webui/`,
run `npm run build` and commit the new `dist/` with it.** Asset paths are absolute from `/`
(`base: "/"`), and routes live in the URL hash (`#/chat?ws=…&s=…`), so the server only needs to
answer `/` with `index.html` and `/assets/*` with the files.

## Layout

| Path | What it holds |
| --- | --- |
| `src/api/` | HTTP client with the bearer token, SSE reader (`fetch` + stream, `Last-Event-ID` resume, backoff, `resync`), TanStack Query hooks |
| `src/session/` | The event reducer: one projection for history replay and live events; tool-row text, wake parsing, command outcomes |
| `src/components/` | Shell pieces: sidebar, transcript, composer, model picker, command palette, right panel, subagent view |
| `src/theme/` | Theme tokens (CSS custom properties) and the saved theme choice |
| `src/router.ts` | Hash routes; later pages (Settings, InstinctFlow) add a `page` value |
| `dev/` | Dev-server-only code, never bundled |
