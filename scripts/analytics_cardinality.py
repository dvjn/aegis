#!/usr/bin/env python3
"""Offline, provisional analytics cardinality experiment. Never reads payload bodies."""
import argparse
import json
from pathlib import Path
import shutil
import sqlite3
import time

ROOT = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--source', type=Path, default=ROOT / 'data/homelab-db-20260905/aegis.db')
p.add_argument('--work', type=Path, default=ROOT / 'data/analytics-validation')
p.add_argument('--reuse-copy', action='store_true', help='Use the existing source.db snapshot, without opening the original')
a = p.parse_args()
a.work = a.work.resolve()
a.work.mkdir(parents=True, exist_ok=True)
copy = a.work / 'source.db'
candidate = a.work / 'candidate.db'
if a.source.resolve() in (copy, candidate):
    raise SystemExit('Source must differ from outputs')
metrics = {'sqlite_version': sqlite3.sqlite_version, 'timings_seconds': {}}
start = time.monotonic()
if not a.reuse_copy:
    if copy.exists():
        raise SystemExit('source.db exists; choose --reuse-copy or a fresh --work directory')
    size = a.source.stat().st_size
    free = shutil.disk_usage(a.work).free
    metrics.update(source_file_bytes=size, free_bytes_before_backup=free)
    if free < size * 2 + 1024**3:
        raise SystemExit('Insufficient space for backup plus experiment reserve')
    source = sqlite3.connect(a.source.resolve().as_uri() + '?mode=ro', uri=True)
    source.execute('BEGIN')
    source.execute('SELECT count(*) FROM sqlite_schema').fetchone()
    target = sqlite3.connect(copy)
    source.backup(target)
    target.close()
    source.close()
    metrics['timings_seconds']['backup'] = time.monotonic() - start
if not copy.is_file():
    raise SystemExit('Missing snapshot copy')
if candidate.exists():
    raise SystemExit('candidate.db exists; remove only that generated file before rerunning')
if shutil.disk_usage(a.work).free < copy.stat().st_size + 1024**3:
    raise SystemExit('Insufficient experiment space')
c = sqlite3.connect(candidate.as_uri(), uri=True)
c.execute('PRAGMA page_size=4096')
c.execute('PRAGMA journal_mode=DELETE')
c.execute('ATTACH DATABASE ? AS src', (copy.as_uri() + '?mode=ro',))
# All derivation uses the copied snapshot. Integer surrogate IDs retain exact scoped grouping.
def timed(name, sql):
    t = time.monotonic()
    c.executescript(sql)
    metrics['timings_seconds'][name] = time.monotonic() - t

def scalar(sql):
    return c.execute(sql).fetchone()[0]

metrics['source_counts'] = {name: scalar('SELECT count(*) FROM src.' + name) for name in
    ['gateway_requests', 'gateway_payload_part_refs', 'gateway_payload_envelopes', 'gateway_payload_blobs', 'gateway_payload_blob_facts']}
metrics['references_by_direction'] = dict(c.execute('SELECT direction,count(*) FROM src.gateway_payload_part_refs GROUP BY direction'))
metrics['requests_without_owner'] = scalar('SELECT count(*) FROM src.gateway_requests r LEFT JOIN src.gateway_keys k ON k.id=r.key_id WHERE k.user_id IS NULL')
metrics['references_without_facts'] = scalar('SELECT count(*) FROM src.gateway_payload_part_refs p WHERE NOT EXISTS (SELECT 1 FROM src.gateway_payload_blob_facts f WHERE f.blob_id=p.part_id)')
timed('derive_metadata', """
CREATE TEMP TABLE observations AS
SELECT r.rowid request_id, r.started_at, COALESCE('user:'||k.user_id,'request:'||r.id) owner,
 r.provider, f.block_type, f.tool_name, f.mcp_server, f.skill_name,
 CASE WHEN NULLIF(f.tool_use_id,'') IS NOT NULL THEN 'call:'||f.tool_use_id ELSE 'blob:'||f.blob_id||':'||f.ordinal END identity_key,
 f.tool_use_id IS NULL OR f.tool_use_id='' fallback,
 f.blob_id, f.ordinal, b.original_bytes bytes
FROM src.gateway_payload_part_refs p JOIN src.gateway_requests r ON r.id=p.request_id
LEFT JOIN src.gateway_keys k ON k.id=r.key_id
JOIN src.gateway_payload_blob_facts f ON f.blob_id=p.part_id
JOIN src.gateway_payload_blobs b ON b.id=p.part_id
WHERE p.direction='request' AND f.block_type IN ('tool_definition','tool_use','tool_result');
CREATE INDEX temp.observation_identity ON observations(owner,provider,identity_key,block_type);
""")
metrics['tool_observations_by_type'] = dict(c.execute('SELECT block_type,count(*) FROM observations GROUP BY block_type'))
# Resolve only unambiguous attribution within owner/provider scope. Missing names remain NULL.
timed('candidate_tables', """
CREATE TABLE request_facts(request_id INTEGER PRIMARY KEY, started_at TEXT NOT NULL);
INSERT INTO request_facts SELECT rowid,started_at FROM src.gateway_requests;
CREATE INDEX request_time ON request_facts(started_at,request_id);
CREATE TABLE identities(identity_id INTEGER PRIMARY KEY, owner TEXT NOT NULL, provider TEXT NOT NULL,
 identity_key TEXT NOT NULL, block_type TEXT NOT NULL,
 UNIQUE(owner,provider,identity_key,block_type));
INSERT INTO identities(owner,provider,identity_key,block_type)
 SELECT DISTINCT owner,provider,identity_key,block_type FROM observations WHERE block_type!='tool_definition';
CREATE TABLE variants(variant_id INTEGER PRIMARY KEY,identity_id INTEGER NOT NULL,blob_id TEXT NOT NULL,
 ordinal INTEGER NOT NULL,bytes INTEGER NOT NULL,tool_name TEXT,mcp_server TEXT,skill_name TEXT,
 UNIQUE(identity_id,blob_id,ordinal));
INSERT INTO variants(identity_id,blob_id,ordinal,bytes,tool_name,mcp_server,skill_name)
 SELECT DISTINCT i.identity_id,o.blob_id,o.ordinal,o.bytes,o.tool_name,o.mcp_server,o.skill_name
 FROM observations o JOIN identities i USING(owner,provider,identity_key,block_type);
CREATE TABLE appearances(request_id INTEGER NOT NULL,variant_id INTEGER NOT NULL,
 PRIMARY KEY(request_id,variant_id)) WITHOUT ROWID;
INSERT INTO appearances SELECT DISTINCT o.request_id,v.variant_id FROM observations o
 JOIN identities i USING(owner,provider,identity_key,block_type)
 JOIN variants v ON v.identity_id=i.identity_id AND v.blob_id=o.blob_id AND v.ordinal=o.ordinal;
CREATE INDEX appearance_dependency ON appearances(variant_id,request_id);
CREATE TEMP TABLE attribution AS SELECT owner,provider,identity_key,
 CASE WHEN count(DISTINCT tool_name)=1 THEN max(tool_name) END tool_name,
 CASE WHEN count(DISTINCT mcp_server)=1 THEN max(mcp_server) END mcp_server,
 CASE WHEN count(DISTINCT skill_name)=1 THEN max(skill_name) END skill_name
 FROM observations WHERE block_type='tool_use' GROUP BY owner,provider,identity_key;
CREATE TABLE contributions(contribution_id INTEGER PRIMARY KEY,request_id INTEGER NOT NULL,
 tool_name TEXT,mcp_server TEXT,skill_name TEXT,observations INTEGER NOT NULL,definition_count INTEGER NOT NULL);
INSERT INTO contributions(request_id,tool_name,mcp_server,skill_name,observations,definition_count)
 SELECT o.request_id,COALESCE(o.tool_name,a.tool_name),COALESCE(o.mcp_server,a.mcp_server),
 COALESCE(o.skill_name,a.skill_name),count(*),sum(o.block_type='tool_definition')
 FROM observations o LEFT JOIN attribution a USING(owner,provider,identity_key)
 GROUP BY o.request_id,COALESCE(o.tool_name,a.tool_name),COALESCE(o.mcp_server,a.mcp_server),COALESCE(o.skill_name,a.skill_name);
CREATE INDEX contribution_request ON contributions(request_id);
CREATE INDEX contribution_tool ON contributions(tool_name,skill_name,request_id);
""")
metrics['candidate_rows'] = {t: scalar('SELECT count(*) FROM ' + t) for t in ['request_facts','contributions','identities','variants','appearances']}
metrics['request_identity_pairs'] = scalar('SELECT count(*) FROM (SELECT DISTINCT a.request_id,v.identity_id FROM appearances a JOIN variants v USING(variant_id))')
metrics['fallback_identities'] = scalar("SELECT count(*) FROM identities WHERE identity_key LIKE 'blob:%'")
metrics['identities_with_multiple_variants'] = scalar('SELECT count(*) FROM (SELECT identity_id FROM variants GROUP BY identity_id HAVING count(*)>1)')
metrics['identities_with_multiple_byte_lengths'] = scalar('SELECT count(*) FROM (SELECT identity_id FROM variants GROUP BY identity_id HAVING count(DISTINCT bytes)>1)')
metrics['ambiguous_call_names'] = scalar("SELECT count(*) FROM (SELECT owner,provider,identity_key FROM observations WHERE block_type='tool_use' GROUP BY owner,provider,identity_key HAVING count(DISTINCT tool_name)>1)")
metrics['unattributed_contributions'] = scalar('SELECT count(*) FROM contributions WHERE tool_name IS NULL')
metrics['contribution_rows_per_active_request'] = list(c.execute('SELECT min(n),avg(n),max(n) FROM (SELECT count(*) n FROM contributions GROUP BY request_id)').fetchone())
metrics['distinct_attributions'] = scalar('SELECT count(*) FROM (SELECT DISTINCT tool_name,mcp_server,skill_name FROM contributions)')
metrics['index_counts'] = dict(c.execute("SELECT tbl_name,count(*) FROM sqlite_schema WHERE type='index' GROUP BY tbl_name"))
c.commit()
try:
    metrics['storage_dbstat'] = [dict(zip(['object','pages','bytes','payload_bytes','unused_bytes'],r)) for r in c.execute('SELECT name,count(*),sum(pgsize),sum(payload),sum(unused) FROM dbstat GROUP BY name ORDER BY name')]
except sqlite3.OperationalError as e:
    metrics['dbstat_unavailable'] = str(e)
metrics['page_count'] = scalar('PRAGMA page_count')
metrics['freelist_count'] = scalar('PRAGMA freelist_count')
metrics['page_size'] = scalar('PRAGMA page_size')
c.close()
metrics['candidate_file_bytes'] = candidate.stat().st_size
metrics['snapshot_file_bytes'] = copy.stat().st_size
metrics['timings_seconds']['total'] = time.monotonic() - start
(a.work / 'cardinality.json').write_text(json.dumps(metrics, indent=2) + '\n')
print(json.dumps(metrics, indent=2))
