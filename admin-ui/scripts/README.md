# Account cache statistics checks

From `admin-ui`, run the calculation tests and build the normal application:

```sh
node --experimental-strip-types --test src/lib/cache-statistics.test.mjs
pnpm run build
```

`check-account-cache-dashboard.cjs` is an optional browser regression check. It
requires the `playwright` Node module and its Chromium browser to be available.
If Playwright is installed outside this project, set `NODE_PATH` to that
installation's `node_modules` directory. This script does not install dependencies.

```sh
node scripts/check-account-cache-dashboard.cjs
```

The script serves the normal `dist` build on an ephemeral loopback port. All API
responses use synthetic fixtures, external font requests are fulfilled locally,
and unexpected API calls, mutations or external requests fail the check. It does
not connect to a running gateway. The browser and local server close on exit.

Coverage includes empty credential filters (server and client pagination), no
credentials, initial and background request failures, range transitions, missing
cache fields, percentage boundaries, privacy, list/card modes, keyboard disclosure,
light/dark themes and narrow layouts. Results and screenshots are written to
`target/account-cache-review` at the repository root; `CACHE_REVIEW_OUTPUT` can
override that directory.
