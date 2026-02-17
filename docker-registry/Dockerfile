FROM registry:2

RUN apk add --no-cache gettext

COPY config.yml /etc/docker/registry/config.yml.template
COPY auth/htpasswd /etc/docker/registry/auth/htpasswd

EXPOSE 5000

CMD ["sh", "-c", "envsubst < /etc/docker/registry/config.yml.template > /etc/docker/registry/config.yml && registry serve /etc/docker/registry/config.yml"]