# vmux push gateway

The gateway is a narrow bridge between self-hosted vmux relays and the Expo
Push Service. It stores no terminal content, user account, hostname, push token,
or notification secret. Only SHA-256 hashes of the per-device token and secret
are persisted.

Registration proves control of the target phone by sending a short-lived random
challenge through its push token. The phone confirms that challenge before the
token-secret pair becomes active. Relays must then present both values to send a
notification. Notification text is fixed by the gateway; relays can provide only
opaque navigation identifiers. An authenticated registration check lets the app
recover safely if gateway state is restored or replaced without repeating the
push challenge on every connection.

Expo enhanced push security must be enabled for the project. Its access token is
provided only through `EXPO_ACCESS_TOKEN` or a root-owned file named by
`EXPO_ACCESS_TOKEN_FILE` on the gateway host. It must never be committed or
distributed with vmux.

```sh
EXPO_ACCESS_TOKEN=... node services/push-gateway/server.mjs
```

Optional environment variables are `VMUX_PUSH_LISTEN` (default `127.0.0.1`),
`VMUX_PUSH_PORT` (default `4180`), and `VMUX_PUSH_STATE`.

Run the dependency-free tests with:

```sh
node --test services/push-gateway/server.test.mjs
```
