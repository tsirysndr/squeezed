Name:           squeezed
Version:        0.1.0
Release:        1%{?dist}
Summary:        Serve a raw PCM audio stream to Squeezelite/Squeezebox clients over SlimProto

License:        MIT
URL:            https://github.com/tsirysndr/squeezed

BuildArch:      x86_64

Requires: glibc

%description
squeezed reads a raw PCM (S16LE by default) audio stream from stdin, a FIFO, a
unix socket, or a TCP socket and serves it over the SlimProto protocol so any
Squeezelite client can play it. It answers UDP service discovery for zero-config
playback and keeps multiple players sample-aligned for true multiroom sync.

%prep
# Nothing to prep — the binary is prebuilt.

%build
# Nothing to build — the binary is prebuilt.

%install
mkdir -p %{buildroot}/usr/local/bin
cp -r %{_sourcedir}/amd64/usr %{buildroot}/

%files
/usr/local/bin/squeezed

%post
if [ "$1" -eq 1 ]; then
    echo "squeezed: installed. Run 'squeezed --help' to get started."
fi
