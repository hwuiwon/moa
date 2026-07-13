# MOA operator dashboards

These JSON files are the source of truth for the operator-facing Grafana
dashboards. They are bundled into a `ConfigMap` named
`moa-observability-dashboards` by `../kustomization.yaml`, labeled
`grafana_dashboard: "1"`.

## Import flow

- **In-cluster Grafana:** the Grafana sidecar (kiwigrid/k8s-sidecar) watches
  ConfigMaps carrying the `grafana_dashboard` label and imports each JSON value
  automatically. No further wiring is needed.
- **Grafana Cloud (current setup):** metrics/traces/logs ship to Grafana Cloud
  via Alloy (`../10-alloy-config.yaml`); Grafana itself is not in-cluster. Sync
  these dashboards with a CI step that posts each JSON to the Grafana Cloud
  dashboards API (`POST /api/dashboards/db`) — the ConfigMap remains the
  reviewed source of truth so the API push stays reproducible.

## Datasources

Panels reference Prometheus/Mimir by the datasource configured in the target
Grafana. The Prometheus-backed dashboards under this directory expect the
default Prometheus datasource; the Postgres/analytics dashboards under
`../../../dashboards/grafana/` carry a `DS_POSTGRES` (or `DS_PROMETHEUS`)
datasource template variable so the `${DS_*}` references resolve at load against
whichever datasource of that type the operator selects.
