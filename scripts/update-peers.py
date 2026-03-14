#!/usr/bin/env python3
"""Fetch gossip peers from API and merge with hardcoded seed peers."""
import json
import subprocess
import sys

SEED_IPS = [
    '64.31.48.111','64.31.51.137','72.46.86.185','72.46.86.159',
    '13.230.78.76','52.195.133.97','52.68.71.160','13.114.116.44',
    '79.127.159.173','79.127.159.174','23.81.40.69','109.123.230.189',
    '31.223.196.172','31.223.196.238','67.213.123.85','199.254.199.12',
    '199.254.199.54','45.250.255.111','109.94.99.131','23.81.41.3',
    '199.254.199.48','64.34.83.57','180.189.55.18','180.189.55.19',
    '157.90.207.92','91.134.71.237','57.129.140.247','72.46.87.141',
    '15.235.231.247','15.235.232.101',
]

chain = sys.argv[1] if len(sys.argv) > 1 else 'Mainnet'

try:
    r = subprocess.run(
        ['curl', '-sf', '-m', '10', '-X', 'POST',
         '--header', 'Content-Type: application/json',
         '--data', '{"type":"gossipRootIps"}',
         'https://api.hyperliquid.xyz/info'],
        capture_output=True, text=True, check=True,
    )
    api_ips = json.loads(r.stdout)
except Exception as e:
    print(f'WARNING: API fetch failed ({e}), using seed peers only', file=sys.stderr)
    api_ips = []

all_ips = list(dict.fromkeys(api_ips + SEED_IPS))
config = {
    'root_node_ips': [{'Ip': ip} for ip in all_ips],
    'try_new_peers': True,
    'chain': chain,
}
print(json.dumps(config, indent=2))
