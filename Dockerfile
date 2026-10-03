# pdf-squeezer with every optional tool: PDF/A conversion needs veraPDF (Java),
# the Python helpers, fontconfig with metric-compatible fonts, and qpdf.
FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /pdf-squeezer .

FROM ubuntu:24.04
ARG VERAPDF_URL=https://software.verapdf.org/releases/1.30/verapdf-greenfield-1.30.2-installer.zip
ARG VERAPDF_SHA256=6cc6341cb1af644044054b81f00a6590a7918abb18f762243de115258bcad838
ENV DEBIAN_FRONTEND=noninteractive PDF_SQUEEZER_PYTHON=/opt/pdf-tools/bin/python
RUN apt-get update && apt-get install -y --no-install-recommends \
      qpdf ghostscript jbig2dec default-jre-headless fontconfig fonts-urw-base35 fonts-liberation \
      python3 python3-venv ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*
COPY tools/requirements.txt /tmp/requirements.txt
RUN python3 -m venv /opt/pdf-tools && /opt/pdf-tools/bin/pip install --no-cache-dir -r /tmp/requirements.txt
RUN cd /tmp && curl -fsSL -o verapdf.zip "$VERAPDF_URL" \
    && echo "$VERAPDF_SHA256  verapdf.zip" | sha256sum -c - \
    && unzip -q verapdf.zip && printf '%s\n' \
      '<AutomatedInstallation langpack="eng">' \
      '  <com.izforge.izpack.panels.htmlhello.HTMLHelloPanel id="welcome"/>' \
      '  <com.izforge.izpack.panels.target.TargetPanel id="install_dir"><installpath>/opt/verapdf</installpath></com.izforge.izpack.panels.target.TargetPanel>' \
      '  <com.izforge.izpack.panels.packs.PacksPanel id="sdk_pack_select">' \
      '    <pack index="0" name="veraPDF GUI" selected="true"/>' \
      '    <pack index="1" name="veraPDF Mac and *nix Scripts" selected="true"/>' \
      '    <pack index="2" name="veraPDF Validation model" selected="false"/>' \
      '    <pack index="3" name="veraPDF Documentation" selected="false"/>' \
      '    <pack index="4" name="veraPDF Sample Plugins" selected="false"/>' \
      '  </com.izforge.izpack.panels.packs.PacksPanel>' \
      '  <com.izforge.izpack.panels.install.InstallPanel id="install"/>' \
      '  <com.izforge.izpack.panels.finish.FinishPanel id="finish"/>' \
      '</AutomatedInstallation>' > /tmp/verapdf-auto.xml \
    && verapdf-*/verapdf-install /tmp/verapdf-auto.xml \
    && ln -s /opt/verapdf/verapdf /usr/local/bin/verapdf \
    && rm -rf /tmp/verapdf.zip /tmp/verapdf-* /tmp/verapdf-auto.xml
COPY --from=build /pdf-squeezer /usr/local/bin/pdf-squeezer
WORKDIR /data
ENTRYPOINT ["pdf-squeezer"]
