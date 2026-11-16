{{/*
`<release>-db`, not Helm's usual `<release>-<chart>`.

The usual rule would give `modernweather-timescaledb`, and a StatefulSet's
volumeClaimTemplate derives its claim name from the StatefulSet's -- so renaming it would
leave `data-modernweather-db-0`, with the 350 GiB of observations in it, bound to nothing
and provision an empty volume beside it. The name is load-bearing.

Takes only `.Release` and `.Values.fullnameOverride`, so the parent chart can call it with
a constructed context (`modernweather.dbFullname`).
*/}}
{{- define "timescaledb.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-db" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "timescaledb.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "timescaledb.labels" -}}
helm.sh/chart: {{ include "timescaledb.chart" . }}
{{ include "timescaledb.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: modernweather
{{- end -}}

{{/*
`app.kubernetes.io/name` alone, matching what was applied by hand. A StatefulSet's
selector is immutable, so this set may not grow without a delete.
*/}}
{{- define "timescaledb.selectorLabels" -}}
app.kubernetes.io/name: {{ include "timescaledb.fullname" . }}
{{- end -}}

{{/*
What goes on the POD, which is less than what goes on the objects.

`helm.sh/chart` and `app.kubernetes.io/version` are deliberately absent: a label on a
StatefulSet's pod template is part of the pod spec, so bumping the chart's version would
roll the pod -- and rolling this pod means a single-replica database on a `local` volume
going down and coming back replaying WAL, for a change to a YAML comment. The daemon's
Deployment does carry them, because there a restart costs a few seconds and nothing else.
*/}}
{{- define "timescaledb.podLabels" -}}
{{ include "timescaledb.selectorLabels" . }}
app.kubernetes.io/part-of: modernweather
{{- end -}}

{{- define "timescaledb.pvName" -}}
{{- if .Values.persistence.local.persistentVolumeName -}}
{{- .Values.persistence.local.persistentVolumeName -}}
{{- else -}}
{{- printf "%s-%s" (include "timescaledb.fullname" .) .Values.persistence.local.node | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/*
`pg_isready` with the real database name. Shared by the three probes so they cannot drift.
*/}}
{{- define "timescaledb.isready" -}}
exec:
  command:
    - pg_isready
    - -U
    - {{ .Values.auth.username | quote }}
    - -d
    - {{ .Values.auth.database | quote }}
{{- end -}}
