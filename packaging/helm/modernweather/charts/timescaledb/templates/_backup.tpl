{{/*
The environment both backup jobs share.

rclone is configured ENTIRELY through `RCLONE_CONFIG_<REMOTE>_*` with `RCLONE_CONFIG`
pointed at /dev/null, so no configuration file exists anywhere for a credential to be
assembled into -- which is `ci/db/offsite-backup.sh`'s documented expectation for the
in-cluster jobs, not a stylistic choice.

R2 is addressed as S3 with `provider: Cloudflare`, `region: auto` and an account-specific
endpoint. `no_check_bucket` because the backup credential is scoped to one bucket and has
no permission to ask whether it exists.
*/}}
{{- define "timescaledb.backupEnv" -}}
- name: PGPASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ .Values.auth.existingSecret | quote }}
      key: {{ .Values.auth.secretKey | quote }}
{{- /*
HOME and TMPDIR are /tmp because the containers run with a read-only root filesystem and
rclone and pg_dump both want somewhere to write.
*/}}
- name: HOME
  value: /tmp
- name: TMPDIR
  value: /tmp
- name: RCLONE_CONFIG
  value: /dev/null
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_TYPE
  value: s3
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_PROVIDER
  value: {{ .Values.backup.provider | quote }}
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_ENV_AUTH
  value: "false"
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_REGION
  value: {{ .Values.backup.region | quote }}
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_ACL
  value: private
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_NO_CHECK_BUCKET
  value: "true"
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_ACCESS_KEY_ID
  valueFrom:
    secretKeyRef:
      name: {{ .Values.backup.credentials.existingSecret | quote }}
      key: {{ .Values.backup.credentials.accessKeyIdKey | quote }}
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_SECRET_ACCESS_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.backup.credentials.existingSecret | quote }}
      key: {{ .Values.backup.credentials.secretAccessKeyKey | quote }}
- name: RCLONE_CONFIG_{{ .Values.backup.remoteName | upper }}_ENDPOINT
  valueFrom:
    secretKeyRef:
      name: {{ .Values.backup.credentials.existingSecret | quote }}
      key: {{ .Values.backup.credentials.endpointKey | quote }}
- name: MW_BACKUP_BUCKET
  value: {{ .Values.backup.bucket | quote }}
{{- end -}}

{{/*
`rclone` as an init container that copies its own binary into a shared emptyDir.

The backup runs in the TimescaleDB image, because that is where `pg_dump` of the right
major version lives, and that image has no rclone. One binary copied into an emptyDir is
cheaper and more legible than a derived image that has to be rebuilt whenever either half
moves.
*/}}
{{- define "timescaledb.rcloneInit" -}}
- name: rclone
  image: {{ .Values.backup.rcloneImage | quote }}
  imagePullPolicy: {{ .Values.image.pullPolicy }}
  command:
    - /bin/sh
    - -c
    - cp /usr/local/bin/rclone /shared/rclone
  securityContext:
    allowPrivilegeEscalation: false
    readOnlyRootFilesystem: true
    capabilities:
      drop:
        - ALL
  resources:
    {{- toYaml .Values.backup.rcloneResources | nindent 4 }}
  volumeMounts:
    - name: shared
      mountPath: /shared
{{- end -}}

{{/*
The writable endpoint, as a host:port. Both jobs reach the database over the Service
rather than over localhost, so they are not pinned to the same node as the pod.
*/}}
{{- define "timescaledb.rwHost" -}}
{{- printf "%s-rw.%s.svc.cluster.local:%v" (include "timescaledb.fullname" .) .Release.Namespace .Values.service.port -}}
{{- end -}}
