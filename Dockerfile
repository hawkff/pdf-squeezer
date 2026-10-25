# pdf-squeezer with every optional tool: PDF/A conversion needs veraPDF (Java),
# the Python helpers, fontconfig with metric-compatible fonts, and qpdf.
FROM golang:trixie AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
# .dockerignore leaves out .git, so Go cannot stamp the version; CI passes it as VERSION.
ARG VERSION
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w -X main.buildVersion=${VERSION}" -o /pdf-squeezer . \
    && { [ -z "$VERSION" ] || [ "$(/pdf-squeezer --version)" = "$VERSION" ]; }

# jbig2enc provides the lossless JBIG2 encoder behind --mono-codecs jbig2.
FROM debian:trixie AS jbig2
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends git cmake g++ make pkg-config libleptonica-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN git init /tmp/jbig2enc && cd /tmp/jbig2enc \
    && git remote add origin https://github.com/agl/jbig2enc.git \
    && git fetch --depth 1 origin d0dfca46216c98f11312a9c9f15615ed490cd7b3 && git checkout --detach FETCH_HEAD \
    && cmake -S . -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build -j4 && cmake --install build

FROM debian:trixie
ENV DEBIAN_FRONTEND=noninteractive PDF_SQUEEZER_PYTHON=/opt/pdf-tools/bin/python
RUN apt-get update && apt-get install -y --no-install-recommends \
      qpdf ghostscript jbig2dec libleptonica6 default-jre-headless fontconfig fonts-urw-base35 fonts-liberation \
      python3 python3-venv ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*
COPY tools/requirements.txt tools/install-verapdf.sh /tmp/
RUN python3 -m venv /opt/pdf-tools && /opt/pdf-tools/bin/pip install --no-cache-dir -r /tmp/requirements.txt
RUN sh /tmp/install-verapdf.sh /opt/verapdf && ln -s /opt/verapdf/verapdf /usr/local/bin/verapdf \
    && rm /tmp/requirements.txt /tmp/install-verapdf.sh
COPY --from=jbig2 /usr/local/bin/jbig2 /usr/local/bin/jbig2
COPY --from=build /pdf-squeezer /usr/local/bin/pdf-squeezer
# PDF parsers run as an unprivileged user; mount documents at /data.
RUN useradd --system --create-home --uid 10001 squeezer && install -d -o squeezer -g squeezer /data
WORKDIR /data
USER squeezer
ENTRYPOINT ["pdf-squeezer"]
