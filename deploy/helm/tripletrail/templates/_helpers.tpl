{{/* The chart's name. */}}
{{- define "tripletrail.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* A fully qualified name for the release's objects. */}}
{{- define "tripletrail.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "tripletrail.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "tripletrail.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "tripletrail.selectorLabels" -}}
app.kubernetes.io/name: {{ include "tripletrail.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "tripletrail.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "tripletrail.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* The Secret holding the chart-made tokens. */}}
{{- define "tripletrail.secretName" -}}
{{- include "tripletrail.fullname" . }}-tokens
{{- end }}

{{/* Whether the chart makes a Secret for the tokens. */}}
{{- define "tripletrail.makesSecret" -}}
{{- if or (and .Values.adminToken.value (not .Values.adminToken.existingSecret)) (and .Values.updateToken.value (not .Values.updateToken.existingSecret)) }}true{{ end }}
{{- end }}

{{/* The env entry for a token: from an existing Secret, or the chart's. */}}
{{- define "tripletrail.tokenEnv" -}}
{{- $token := index . 0 }}{{- $envName := index . 1 }}{{- $root := index . 2 }}
{{- if or $token.existingSecret $token.value }}
- name: {{ $envName }}
  valueFrom:
    secretKeyRef:
      name: {{ $token.existingSecret | default (include "tripletrail.secretName" $root) }}
      key: {{ $token.key }}
{{- end }}
{{- end }}

{{/* The PersistentVolumeClaim for /data. */}}
{{- define "tripletrail.claimName" -}}
{{- .Values.persistence.existingClaim | default (printf "%s-data" (include "tripletrail.fullname" .)) }}
{{- end }}
