{{/*
The chart's name, or nameOverride.
*/}}
{{- define "straum.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
The name of every resource: the release's name, followed by the chart's
unless the release's name holds it already. Kubernetes allows 63
characters.
*/}}
{{- define "straum.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := include "straum.name" . }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Labels that pick the release's pods. They never change across upgrades,
since a Deployment's selector can't.
*/}}
{{- define "straum.selectorLabels" -}}
app.kubernetes.io/name: {{ include "straum.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Labels for every resource.
*/}}
{{- define "straum.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "straum.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
The ServiceAccount the pod runs as.
*/}}
{{- define "straum.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "straum.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
The image, by digest when one is given.
*/}}
{{- define "straum.image" -}}
{{- $repository := required "image.repository: set the straum image, such as myregistry.azurecr.io/straum" .Values.image.repository }}
{{- if .Values.image.digest }}
{{- printf "%s@%s" $repository .Values.image.digest }}
{{- else }}
{{- printf "%s:%s" $repository (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}
