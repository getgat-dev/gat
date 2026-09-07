# Used only for acceptance testing; release compilation runs on the host.
FROM rockylinux:8@sha256:9794037624aaa6212aeada1d28861ef5e0a935adaf93e4ef79837119f2a2d04c AS gnu
RUN dnf install -y git bash ca-certificates && dnf clean all

FROM alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce AS musl
RUN apk add --no-cache git bash ca-certificates
