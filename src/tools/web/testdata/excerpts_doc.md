# Deploying the Service

## Overview

This guide covers installing, configuring, and deploying the widget service to a
production cluster. It assumes you already have cluster credentials and a working
container registry available for pushing images.

## Configuration

The service reads its settings from environment variables at startup. The table
below lists the variables the deployment honors and their default values.

| Variable | Default | Purpose |
| --- | --- | --- |
| PORT | 8080 | TCP port the HTTP server binds to |
| LOG_LEVEL | info | Minimum log level emitted |
| CACHE_TTL | 300 | Cache entry lifetime in seconds |

## Running Migrations

Before the first deploy you must apply the database migrations. Run the
migration command against the target database and wait for it to report success:

```bash
widgetctl migrate --database "$DATABASE_URL" --wait
```

The command is idempotent, so re-running it after a partial failure is safe and
will only apply the migrations that have not yet been recorded.

## Monitoring

Once deployed, scrape the metrics endpoint and alert on the saturation and error
-rate signals. The dashboards ship with sensible defaults for both.
