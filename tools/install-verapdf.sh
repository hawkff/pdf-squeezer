#!/bin/sh
# Install the pinned veraPDF command-line validator into an absolute DIR.
# The IzPack installer needs an answer file; this one selects the CLI pack only.
set -eu
case ${1-} in
/*) dir=$1 ;;
*)
	echo "usage: $0 /absolute/install/dir" >&2
	exit 2
	;;
esac
version=1.30.3
sha256=b512bf24945503a33fc10738ecffc1f2dccaaf8593a448e94ee9ac86a9559a59
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
curl -fsSL -o installer.zip "https://software.verapdf.org/releases/${version%.*}/verapdf-greenfield-$version-installer.zip"
if command -v sha256sum >/dev/null 2>&1; then
	echo "$sha256  installer.zip" | sha256sum -c -
else
	echo "$sha256  installer.zip" | shasum -a 256 -c -
fi
unzip -q installer.zip
cat >answers.xml <<EOF
<AutomatedInstallation langpack="eng">
  <com.izforge.izpack.panels.htmlhello.HTMLHelloPanel id="welcome"/>
  <com.izforge.izpack.panels.target.TargetPanel id="install_dir"><installpath>$dir</installpath></com.izforge.izpack.panels.target.TargetPanel>
  <com.izforge.izpack.panels.packs.PacksPanel id="sdk_pack_select">
    <pack index="0" name="veraPDF GUI" selected="false"/>
    <pack index="1" name="veraPDF CLI" selected="true"/>
    <pack index="2" name="veraPDF Documentation" selected="false"/>
    <pack index="3" name="veraPDF Sample Plugins" selected="false"/>
  </com.izforge.izpack.panels.packs.PacksPanel>
  <com.izforge.izpack.panels.install.InstallPanel id="install"/>
  <com.izforge.izpack.panels.finish.FinishPanel id="finish"/>
</AutomatedInstallation>
EOF
verapdf-*/verapdf-install "$work/answers.xml"
"$dir/verapdf" --version
