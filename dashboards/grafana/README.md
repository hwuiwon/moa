# Grafana dashboards

This directory is the canonical source for MOA Grafana dashboards. Prometheus
panels use a datasource variable named `DS_PROMETHEUS`; Postgres panels use a
separate `DS_POSTGRES` variable. Keeping the variables distinct lets each
Grafana stack select its own datasource UIDs without rewriting dashboard JSON.

## Provisioning

The `sync-grafana-dashboards` GitHub workflow imports every JSON file in this
directory after dashboard changes land on `main`. It can also be started with
`workflow_dispatch`. Imports use each dashboard's stable `uid` and
`overwrite: true`, so rerunning the workflow updates the same dashboards rather
than creating copies.

The workflow requires these GitHub Actions secrets:

- `GRAFANA_URL`: the Grafana instance base URL, such as
  `https://example.grafana.net`.
- `GRAFANA_SERVICE_ACCOUNT_TOKEN`: a Grafana service-account token with
  permission to create and update dashboards in the destination folder.
- `GRAFANA_FOLDER_UID` (optional): the UID of the destination folder. When it is
  omitted, Grafana imports dashboards into the General folder.
