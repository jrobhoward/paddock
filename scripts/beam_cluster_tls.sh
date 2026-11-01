#!/usr/bin/env bash
# Certificates and an options file for scripts/beam_cluster.escript over
# TLS distribution: a CA and one node certificate signed by it (EC P-256),
# both nodes verifying the other against the CA, TLS 1.3 only. Copy DIR to
# the other machine at the same path, or point ssl_dist.conf's paths at
# where it goes.
#
#   scripts/beam_cluster_tls.sh DIR
#   ERL_FLAGS="+S 4:4 -proto_dist inet_tls -ssl_dist_optfile DIR/ssl_dist.conf" \
#     escript scripts/beam_cluster.escript ...
set -euo pipefail

dir=${1:?usage: beam_cluster_tls.sh DIR}
mkdir -p "$dir"
dir=$(cd "$dir" && pwd)
cd "$dir"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 \
  -subj /CN=bulkhead-bench-ca -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj /CN=bulkhead-bench -keyout node.key -out node.csr 2>/dev/null
openssl x509 -req -in node.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 \
  -out node.pem 2>/dev/null
chmod 600 ca.key node.key
cat >ssl_dist.conf <<CONF
[{server, [{certfile, "$dir/node.pem"}, {keyfile, "$dir/node.key"},
           {cacertfile, "$dir/ca.pem"}, {verify, verify_peer},
           {fail_if_no_peer_cert, true}, {versions, ['tlsv1.3']}]},
 {client, [{certfile, "$dir/node.pem"}, {keyfile, "$dir/node.key"},
           {cacertfile, "$dir/ca.pem"}, {verify, verify_peer},
           {server_name_indication, disable}, {versions, ['tlsv1.3']}]}].
CONF
echo "beam_cluster_tls: $dir/ssl_dist.conf"
