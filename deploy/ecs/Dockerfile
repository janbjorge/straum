# The release's image with the spec, and the CA roots if the server
# needs them, built in: a Fargate task mounts no ConfigMap or file
# Secret. A new spec is a new image and a new task definition revision.
ARG STRAUM_VERSION
FROM ghcr.io/janbjorge/straum:${STRAUM_VERSION}
COPY spec.sql /etc/straum/spec.sql
# Amazon RDS's roots, for ?sslrootcert=/etc/straum-tls/ca.pem in the DSN.
COPY global-bundle.pem /etc/straum-tls/ca.pem
