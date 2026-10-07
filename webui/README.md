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
- `GET/PUT /v1/settings` and `POST /v1/gateways/{name}/test` fall back the same way to
  `dev/settingsFallback.ts`: it starts from the real `config.json` / `models.json` of
  `NAVAI_CONFIG_DIR` (default `~/.config/navai`), keeps changes **in memory only** (never writes a
  file), validates models and agents against the real `/v1/models` and `/v1/agents`, and answers
  409 / 400 as the contract (`.scratch/settings/contract.md`) says.
- The Studio (`#/studio?ws=…&flow=…&loc=…`) uses the flows routes of contracts §5
  (`.scratch/instinct-flow/contracts.md`). While the server does not have them (a 404 or 405 on
  `GET /v1/flows/catalogue`), it falls back to an **in-memory mock** (`src/studio/mock/`), seeded with
  `guard-mutations` and one recorded run; add `&mock=1` to the route to force it. The mock writes
  nothing to disk and invents the instinct's answers for Run sample; the page says so.
- Against a `navai web` server (which wants its token), pass the token of its URL:
  `NAVAI_SERVER=http://127.0.0.1:<port> NAVAI_TOKEN=<token> npm run dev`. The dev server adds it as
  `Authorization: Bearer` to every proxied `/v1` call (and to the stand-ins' own calls). Dev only:
  it never reaches the built app.
- In dev there is no token in the page. In production `navai web` opens `http://127.0.0.1:<port>/#token=<token>`:
  the app reads the token, keeps it in `sessionStorage` for that tab, removes it from the address bar
  and sends it as `Authorization: Bearer` on every `/v1` call. A 401 shows "Session expired".

## Check

```sh
npm test            # vitest: SSE decoder, transcript projection (with recorded streams), compose routing, settings logic, studio graph logic and mock API
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
| `src/settings/` | The Settings page (`#/settings?ws=…&section=…`): sections, catalogue picker, immediate saves |
| `src/lib/settings.ts` | Settings field mapping, sidebar summaries, search, the save state (tested) |
| `src/lib/catalog.ts` | Catalogue grouping gateway › provider, shared by the model picker and Settings |
| `src/theme/` | Theme tokens (CSS custom properties) and the local theme choice; `tui.theme` from config.json wins |
| `src/studio/` | The InstinctFlow Studio (`#/studio`), loaded on demand: React Flow canvas derived from the flow definition (`graph.ts`, `canvasModel.ts`, tested), inspector, run bar, typed flows API client (`api.ts`) and its in-memory mock (`mock/`). Imports nothing from the chat pages |
| `src/router.ts` | Hash routes: `chat`, `settings`, `studio` |
| `dev/` | Dev-server-only code, never bundled |
