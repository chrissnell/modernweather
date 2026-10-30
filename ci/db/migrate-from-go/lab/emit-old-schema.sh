#!/usr/bin/env bash
# Print the OLD schema as SQL, extracted from the Go source's own string constants.
#
# Retyping a 215-column CREATE TABLE and four 399-column views to build a "faithful copy"
# would make the copy a measurement of the retyping. The constants are the schema.
#
# The four views are emitted WITH NO DATA. The Go DDL omits that, so creating a view there
# materialises the whole table synchronously in one uninterruptible statement — which is a
# property of the old provisioner worth knowing about and not one the lab wants.
set -euo pipefail
RW="${1:?usage: emit-old-schema.sh <path-to-remoteweather-checkout>}"
python3 - "$RW/internal/storage/timescaledb/sql.go" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
c = dict(re.findall(r'const\s+(\w+)\s*=\s*`(.*?)`', src, re.S))
order = ['createExtensionSQL', 'createTableSQL', 'createHypertableSQL',
         'createCircAvgStateTypeSQL', 'createCircAvgStateFunctionSQL',
         'createCircAvgCombinerFunctionSQL', 'createCircAvgFinalizerFunctionSQL',
         'createCircAvgAggregateFunctionSQL']
views = ['create1mViewSQL', 'create5mViewSQL', 'create1hViewSQL', 'create1dViewSQL']
missing = [k for k in order + views + ['createIndexesSQL'] if k not in c]
if missing:
    raise SystemExit('emit-old-schema: constants not found: %s' % ', '.join(missing))
for k in order:
    print('-- === %s ===' % k); print(c[k].strip()); print()
for k in views:
    print('-- === %s (WITH NO DATA; see the header) ===' % k)
    print(c[k].strip().rstrip(';') + '\nWITH NO DATA;'); print()
print('-- === createIndexesSQL ==='); print(c['createIndexesSQL'].strip())
PY
