# Remote Service

The `remote` crate contains the hosted API and web app.

## Local Setup

Create `crates/remote/.env.remote`:

```env
# Required
VIBEKANBAN_REMOTE_JWT_SECRET=replace_with_openssl_rand_base64_48
ELECTRIC_ROLE_PASSWORD=replace_with_secure_password

# Configure at least one auth option
GITHUB_OAUTH_CLIENT_ID=
GITHUB_OAUTH_CLIENT_SECRET=
GOOGLE_OAUTH_CLIENT_ID=
GOOGLE_OAUTH_CLIENT_SECRET=

# Or use bootstrap local auth for self-hosting
SELF_HOST_LOCAL_AUTH_EMAIL=
SELF_HOST_LOCAL_AUTH_PASSWORD=

# Optional
PUBLIC_BASE_URL=http://localhost:3000
VITE_RELAY_API_BASE_URL=http://localhost:8082
VITE_PUBLIC_REACT_VIRTUOSO_LICENSE_KEY=
LOOPS_EMAIL_API_KEY=
```

Generate the JWT secret once:

```bash
openssl rand -base64 48
```

## Run

From the repo root:

```bash
pnpm run remote:dev
```

Full stack with relay and local attachment storage:

```bash
pnpm run remote:dev:full
```

Equivalent manual command:

```bash
cd crates/remote
docker compose --env-file .env.remote up --build
```

This starts:

- `remote-db`
- `remote-server`
- `electric`

Default endpoints:

- Remote web UI/API: `http://localhost:3000`
- Postgres: `postgres://remote:remote@localhost:5433/remote`

## Optional Profiles

Enable relay support:

```bash
cd crates/remote
docker compose --env-file .env.remote --profile relay up --build
```

Enable local attachment storage with Azurite:

```bash
cd crates/remote
docker compose --env-file .env.remote --profile attachments up --build
```

Enable both:

```bash
cd crates/remote
docker compose --env-file .env.remote --profile relay --profile attachments up --build
```

Additional endpoint with the `relay` profile:

- Relay API: `http://localhost:8082`

## Desktop App

The Docker Compose stack no longer runs `local-server`.

If you want to run the desktop/local app against this remote stack, run it separately on the host and point it at the remote API:

```bash
export VK_SHARED_API_BASE=http://localhost:3000
pnpm run dev
```
