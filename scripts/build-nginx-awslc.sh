#!/usr/bin/env bash
# nginx 1.28 を AWS-LC (+ pcre2 / zlib を静的リンク) でビルドする。出力: build/nginx-awslc
# Ubuntu 24.04 の OpenSSL 3.0 は小さな TLS レコードごとの provider 層のオーバーヘッドが大きいので差し替える。
set -euo pipefail
R=$(cd "$(dirname "$0")/.." && pwd)
cd "$R"
W=${WORK:-$R/build/nginx-src}
mkdir -p "$W" build
cd "$W"
[ -d nginx-1.28.0 ] || curl -sSL https://nginx.org/download/nginx-1.28.0.tar.gz | tar xz
[ -d pcre2-10.44 ] || curl -sSL https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.44/pcre2-10.44.tar.gz | tar xz
[ -d zlib-1.3.1 ] || curl -sSL https://zlib.net/fossils/zlib-1.3.1.tar.gz | tar xz
[ -d aws-lc ] || git clone -q --depth 1 --branch v1.62.0 https://github.com/aws/aws-lc.git
cmake -S aws-lc -B aws-lc/build -GNinja -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTING=OFF -DDISABLE_GO=ON \
  -DCMAKE_C_COMPILER=/usr/bin/gcc -DCMAKE_CXX_COMPILER=/usr/bin/g++ >/dev/null
ninja -C aws-lc/build ssl crypto >/dev/null
A=$PWD/aws-lc
cd nginx-1.28.0
# AWS-LC には SSL_SESSION_get0_hostname が無い (BoringSSL と同じ扱いにする)
sed -i 's/     \&\& !defined LIBRESSL_VERSION_NUMBER \&\& !defined OPENSSL_IS_BORINGSSL)/     \&\& !defined LIBRESSL_VERSION_NUMBER \&\& !defined OPENSSL_IS_BORINGSSL \&\& !defined OPENSSL_IS_AWSLC)/' src/http/ngx_http_request.c
python3 "$R/scripts/nginx-incoming-cpu.patch.py" "$PWD"
CC=/usr/bin/gcc ./configure --prefix=/usr/share/nginx --sbin-path=/usr/local/sbin/nginx-awslc --conf-path=/etc/nginx/nginx.conf \
  --error-log-path=/var/log/nginx/error.log --http-log-path=/var/log/nginx/access.log --pid-path=/run/nginx.pid --lock-path=/var/lock/nginx.lock \
  --modules-path=/usr/lib/nginx/modules --http-client-body-temp-path=/var/lib/nginx/body --http-fastcgi-temp-path=/var/lib/nginx/fastcgi \
  --http-proxy-temp-path=/var/lib/nginx/proxy --http-scgi-temp-path=/var/lib/nginx/scgi --http-uwsgi-temp-path=/var/lib/nginx/uwsgi \
  --with-threads --with-http_ssl_module --with-http_v2_module --with-http_gzip_static_module \
  --with-pcre=../pcre2-10.44 --with-pcre-jit --with-zlib=../zlib-1.3.1 \
  --without-http_fastcgi_module --without-http_uwsgi_module --without-http_scgi_module --without-http_memcached_module \
  --with-cc-opt="-O3 -march=skylake-avx512 -I$A/include" --with-ld-opt="-L$A/build/ssl -L$A/build/crypto -lstdc++ -lpthread" >/dev/null
make -j"$(nproc)" >/dev/null
command cp objs/nginx "$R/build/nginx-awslc"
echo "built build/nginx-awslc"
