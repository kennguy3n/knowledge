{{/*
Expand the name of the chart.
*/}}
{{- define "knowledge.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name. Truncated to 63 chars for the DNS label limit;
the per-component suffixes (-gateway/-substrate) add a few more, so the
base is kept conservatively short.
*/}}
{{- define "knowledge.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 50 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 50 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 50 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "knowledge.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels applied to every object.
*/}}
{{- define "knowledge.labels" -}}
helm.sh/chart: {{ include "knowledge.chart" . }}
{{ include "knowledge.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels (stable across upgrades — do NOT add version here).
*/}}
{{- define "knowledge.selectorLabels" -}}
app.kubernetes.io/name: {{ include "knowledge.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Per-component selector labels.
*/}}
{{- define "knowledge.gateway.selectorLabels" -}}
{{ include "knowledge.selectorLabels" . }}
app.kubernetes.io/component: gateway
{{- end }}

{{- define "knowledge.substrate.selectorLabels" -}}
{{ include "knowledge.selectorLabels" . }}
app.kubernetes.io/component: substrate
{{- end }}

{{- define "knowledge.ui.selectorLabels" -}}
{{ include "knowledge.selectorLabels" . }}
app.kubernetes.io/component: ui
{{- end }}

{{- define "knowledge.llamaServer.selectorLabels" -}}
{{ include "knowledge.selectorLabels" . }}
app.kubernetes.io/component: llama-server
{{- end }}

{{/*
Resource names.
*/}}
{{- define "knowledge.gateway.fullname" -}}
{{- printf "%s-gateway" (include "knowledge.fullname" .) }}
{{- end }}

{{- define "knowledge.substrate.fullname" -}}
{{- printf "%s-substrate" (include "knowledge.fullname" .) }}
{{- end }}

{{- define "knowledge.ui.fullname" -}}
{{- printf "%s-ui" (include "knowledge.fullname" .) }}
{{- end }}

{{- define "knowledge.llamaServer.fullname" -}}
{{- printf "%s-llama-server" (include "knowledge.fullname" .) }}
{{- end }}

{{/*
ServiceAccount name.
*/}}
{{- define "knowledge.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "knowledge.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Validate that a master key source is configured, and fail the
install/upgrade fast (rather than letting the substrate crash-loop on an
empty or malformed key). Resolution order:

  - secrets.existingSecret set        -> key supplied out-of-band, OK.
  - secrets.create + secrets.masterKey-> must be 64 hex characters.
  - secrets.create + empty masterKey  -> fail (no key to render).
  - neither                           -> fail (no Secret will exist).

Call with the root context: {{ include "knowledge.validateMasterKey" . }}
*/}}
{{- define "knowledge.validateMasterKey" -}}
{{- if not .Values.secrets.existingSecret }}
{{- if .Values.secrets.create }}
{{- if not .Values.secrets.masterKey }}
{{- fail "secrets.masterKey is required: set a 64-hex-char key (generate with `openssl rand -hex 32`) or point secrets.existingSecret at a Secret containing KNOWLEDGE_MASTER_KEY" }}
{{- else if not (regexMatch "^[0-9a-fA-F]{64}$" .Values.secrets.masterKey) }}
{{- fail "secrets.masterKey must be 64 hexadecimal characters (generate with `openssl rand -hex 32`)" }}
{{- end }}
{{- else }}
{{- fail "no master key source configured: enable secrets.create with secrets.masterKey, or set secrets.existingSecret to a Secret containing KNOWLEDGE_MASTER_KEY" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Name of the Secret holding the master key (existing or chart-managed).
*/}}
{{- define "knowledge.secretName" -}}
{{- if .Values.secrets.existingSecret }}
{{- .Values.secrets.existingSecret }}
{{- else }}
{{- printf "%s-secrets" (include "knowledge.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Name of the PVC backing the substrate (existing or chart-managed).
*/}}
{{- define "knowledge.substrate.pvcName" -}}
{{- if .Values.substrate.persistence.existingClaim }}
{{- .Values.substrate.persistence.existingClaim }}
{{- else }}
{{- printf "%s-data" (include "knowledge.substrate.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Image references, defaulting the tag to the chart appVersion. Called with
the root context (".") so .Chart.AppVersion is in scope.
*/}}
{{- define "knowledge.gateway.image" -}}
{{- $img := .Values.gateway.image -}}
{{- printf "%s:%s" $img.repository ($img.tag | default .Chart.AppVersion) -}}
{{- end }}

{{- define "knowledge.substrate.image" -}}
{{- $img := .Values.substrate.image -}}
{{- printf "%s:%s" $img.repository ($img.tag | default .Chart.AppVersion) -}}
{{- end }}

{{- define "knowledge.ui.image" -}}
{{- $img := .Values.ui.image -}}
{{- printf "%s:%s" $img.repository ($img.tag | default .Chart.AppVersion) -}}
{{- end }}

{{- define "knowledge.llamaServer.image" -}}
{{- $img := .Values.llamaServer.image -}}
{{- printf "%s:%s" $img.repository ($img.tag | default .Chart.AppVersion) -}}
{{- end }}

{{/*
URL the substrate uses to reach the llama-server sidecar (in-cluster
Service DNS). Empty when the sidecar is disabled.
*/}}
{{- define "knowledge.llamaServer.url" -}}
{{- if .Values.llamaServer.enabled -}}
{{- printf "http://%s:%d" (include "knowledge.llamaServer.fullname" .) (int .Values.llamaServer.service.port) -}}
{{- end -}}
{{- end }}
