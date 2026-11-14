# pdf-squeezer with every optional tool: PDF/A conversion needs veraPDF (Java),
# the Python helpers, fontconfig with metric-compatible fonts, and qpdf.
# The binary compiles on the build machine for the target platform, because an emulated
# compiler takes many times longer.
FROM --platform=$BUILDPLATFORM rust:trixie AS build
ARG BUILDARCH TARGETARCH
RUN case "$TARGETARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; *) echo "unsupported platform $TARGETARCH" >&2; exit 1 ;; esac \
    && if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
         apt-get update && apt-get install -y --no-install-recommends "gcc-$(echo "$arch" | tr _ -)-linux-gnu" "libc6-dev-$TARGETARCH-cross" \
         && rm -rf /var/lib/apt/lists/*; \
       fi \
    && rustup target add "$arch-unknown-linux-gnu" \
    && echo "$arch-unknown-linux-gnu" > /target
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
WORKDIR /src
COPY . .
# CI passes the release name or commit as VERSION, and the binary must report it.
ARG VERSION
RUN target=$(cat /target) && PDF_SQUEEZER_VERSION="${VERSION}" cargo build --release --locked --target "$target" \
    && cp "target/$target/release/pdf-squeezer" /pdf-squeezer

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
ARG VERSION
RUN [ -z "$VERSION" ] || [ "$(pdf-squeezer --version)" = "$VERSION" ]
# PDF parsers run as an unprivileged user; mount documents at /data.
RUN useradd --system --create-home --uid 10001 squeezer && install -d -o squeezer -g squeezer /data
WORKDIR /data
USER squeezer
ENTRYPOINT ["pdf-squeezer"]
