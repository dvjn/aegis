#!/usr/bin/env python3
"""Compare surrogate tool dimensions against the provisional cardinality candidate."""

import argparse
import json
from pathlib import Path
import sqlite3
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=Path(__file__).resolve().parents[1] / "data/analytics-validation")
    args = parser.parse_args()
    work = args.work.resolve()
    candidate, output = work / "candidate.db", work / "normalized.db"
    if not candidate.is_file() or output.exists():
        raise SystemExit("Need candidate.db and a fresh normalized.db output path")
    started = time.monotonic()
    db = sqlite3.connect(output.as_uri(), uri=True)
    db.execute("ATTACH DATABASE ? AS candidate", (candidate.as_uri() + "?mode=ro",))
    db.executescript("""
        CREATE TABLE tools (
            tool_id INTEGER PRIMARY KEY,
            tool_name TEXT, mcp_server TEXT, skill_name TEXT
        );
        INSERT INTO tools(tool_name,mcp_server,skill_name)
            SELECT DISTINCT tool_name,mcp_server,skill_name FROM candidate.contributions;
        CREATE UNIQUE INDEX tool_names ON tools(tool_name,mcp_server,skill_name);
        CREATE TABLE contributions (
            request_id INTEGER NOT NULL,
            tool_id INTEGER NOT NULL,
            observations INTEGER NOT NULL,
            definition_count INTEGER NOT NULL,
            PRIMARY KEY(request_id,tool_id)
        ) WITHOUT ROWID;
        INSERT INTO contributions
            SELECT c.request_id,t.tool_id,c.observations,c.definition_count
            FROM candidate.contributions c JOIN tools t
            ON t.tool_name IS c.tool_name AND t.mcp_server IS c.mcp_server
                AND t.skill_name IS c.skill_name;
        CREATE INDEX tool_requests ON contributions(tool_id,request_id);
    """)
    counts = {table: db.execute("SELECT count(*) FROM " + table).fetchone()[0] for table in ("tools", "contributions")}
    original_count = db.execute("SELECT count(*) FROM candidate.contributions").fetchone()[0]
    if counts["contributions"] != original_count:
        raise RuntimeError("Normalization lost contributions")
    mismatches = db.execute("""
        SELECT count(*) FROM candidate.contributions c
        JOIN tools t ON t.tool_name IS c.tool_name AND t.mcp_server IS c.mcp_server AND t.skill_name IS c.skill_name
        JOIN contributions n ON n.request_id=c.request_id AND n.tool_id=t.tool_id
        WHERE n.observations != c.observations OR n.definition_count != c.definition_count
    """).fetchone()[0]
    if mismatches:
        raise RuntimeError("Normalization changed counts")
    storage = [dict(zip(("object", "pages", "bytes"), row)) for row in db.execute("SELECT name,count(*),sum(pgsize) FROM dbstat GROUP BY name ORDER BY name")]
    db.close()
    result = {"rows": counts, "mismatches": mismatches, "storage": storage, "file_bytes": output.stat().st_size, "seconds": time.monotonic() - started}
    (work / "normalized-cardinality.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
