# The release's image with the spec and the CA roots built in: a Fargate
# task mounts no ConfigMap or file Secret. A new spec is a new image and a
# new task definition revision.
ARG STRAUM_VERSION
FROM ghcr.io/janbjorge/straum:${STRAUM_VERSION}
COPY spec.sql /etc/straum/spec.sql
# Amazon RDS's roots, for ?sslrootcert=/etc/straum-tls/ca.pem in the DSN.
# The build needs global-bundle.pem beside this file. Download it from
# https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem. For a
# server that the built-in roots check, remove this COPY line.
COPY global-bundle.pem /etc/straum-tls/ca.pem
