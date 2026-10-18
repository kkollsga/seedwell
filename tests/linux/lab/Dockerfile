# Lab image: Debian 13 with the distro packages of the clients and tracker under test, plus the
# freshly built Linux `seedwell` and `tracker-probe` (copied from the build context).
FROM debian:13
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl iproute2 procps python3 jq tcpdump netcat-openbsd iptables \
      transmission-daemon transmission-cli qbittorrent-nox opentracker \
    && rm -rf /var/lib/apt/lists/* \
    && (systemctl disable transmission-daemon 2>/dev/null || true)
COPY seedwell tracker-probe /usr/local/bin/
RUN chmod 755 /usr/local/bin/seedwell /usr/local/bin/tracker-probe
