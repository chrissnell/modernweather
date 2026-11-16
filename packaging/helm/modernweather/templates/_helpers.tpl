{{/*
The release's name for the daemon.

The standard Helm idiom, and it is worth stating what it produces here: with the release
named `modernweather` the name is `modernweather` rather than `modernweather-modernweather`,
which is exactly what the hand-applied objects this chart replaces are called. That is what
lets `helm upgrade --install modernweather` adopt them instead of installing beside them.
*/}}
{{- define "modernweather.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "modernweather.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "modernweather.labels" -}}
helm.sh/chart: {{ include "modernweather.chart" . }}
{{ include "modernweather.selectorLabels" . }}
app.kubernetes.io/version: {{ include "modernweather.imageTag" . | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: modernweather
{{- end -}}

{{/*
`app.kubernetes.io/name` alone, matching the Deployment the hand-applied manifests used.
A selector is immutable on a Deployment, so this set may not grow without a delete.
*/}}
{{- define "modernweather.selectorLabels" -}}
app.kubernetes.io/name: {{ include "modernweather.fullname" . }}
{{- end -}}

{{- define "modernweather.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "modernweather.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "modernweather.imageTag" -}}
{{- default .Chart.AppVersion .Values.image.tag -}}
{{- end -}}

{{/*
A digest wins over a tag, because that is what asking for a digest means.
*/}}
{{- define "modernweather.image" -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository (include "modernweather.imageTag" .) -}}
{{- end -}}
{{- end -}}

{{- define "modernweather.pvcName" -}}
{{- default (include "modernweather.fullname" .) .Values.persistence.existingClaim -}}
{{- end -}}

{{/*
The database's name, as the subchart computes it.

Helm merges every chart's templates into one namespace, so the subchart's own helper is
callable from here -- it just has to be handed the context the subchart would have seen.
This is the one place that does that, so the naming rule is not restated.
*/}}
{{- define "modernweather.dbFullname" -}}
{{- include "timescaledb.fullname" (dict "Release" .Release "Values" .Values.timescaledb) -}}
{{- end -}}
