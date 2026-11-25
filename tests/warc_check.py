"""Checks McCrawl output with warcio, a WARC library that shares no code with the crawler.

    uv run --with warcio python tests/warc_check.py DIR [DIR ...]

Each DIR is one crawl's output (crawl-*.warc.gz, pages.jsonl, report.json). For each:
- the WARC files are numbered from 0 with no gaps, and every one parses with warcio;
- each file starts with a warcinfo record naming the file; every other record is a response;
- every record has a well-formed record ID, used once, and a WARC date;
- each response has a target URI, the application/http type, an HTTP status line, and an
  HTTP Content-Length equal to its stored body; WARC-Truncated is "length" or absent;
- the responses are exactly the archived rows of pages.jsonl (those with a simhash), with the
  same status and truncation, and no URL is archived twice;
- report.json's store counts match what is on disk.
Prints an OK line per directory, or FAIL and the first problem, and then exits with 1.
"""
import json
import re
import sys
from pathlib import Path

from warcio.archiveiterator import ArchiveIterator

DATE = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ")
# RFC 9562 version 8: the version nibble is 8 and the variant bits are 10.
RECORD_ID = re.compile(r"<urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}>")
HTTP_RESPONSE = "application/http;msgtype=response"


class Bad(Exception):
    pass


def need(ok, problem):
    if not ok:
        raise Bad(problem)


def read_warcs(files):
    """{url: (status, truncated)} for every response, after checking each record."""
    ids, responses = set(), {}
    for f in files:
        with f.open("rb") as stream:
            n = -1
            try:
                for n, rec in enumerate(ArchiveIterator(stream)):
                    read_record(f, n, rec, ids, responses)
            except Bad:
                raise
            except Exception as e:  # warcio itself refusing the file
                raise Bad(f"{f.name}: warcio can't read the record after #{n}: {type(e).__name__}: {e}") from e
    return responses


def read_record(f, n, rec, ids, responses):
    """Checks one record, adding a response to `responses`."""
    h, where = rec.rec_headers, f"{f.name} record {n}"
    rid = h.get_header("WARC-Record-ID") or ""
    need(RECORD_ID.fullmatch(rid), f"{where}: bad WARC-Record-ID {rid!r}")
    need(rid not in ids, f"{where}: record ID {rid} used twice")
    ids.add(rid)
    need(DATE.fullmatch(h.get_header("WARC-Date") or ""), f"{where}: bad WARC-Date")
    if n == 0:
        need(rec.rec_type == "warcinfo", f"{where}: starts with {rec.rec_type}, not warcinfo")
        named = h.get_header("WARC-Filename")
        need(named == f.name, f"{where}: warcinfo names {named!r}")
        info = rec.content_stream().read().decode()
        need("software: McCrawl/" in info, f"{where}: warcinfo has no software field")
        return
    need(rec.rec_type == "response", f"{where}: a {rec.rec_type} record after the warcinfo")
    url = h.get_header("WARC-Target-URI")
    need(url, f"{where}: no WARC-Target-URI")
    where += f" ({url})"
    need(h.get_header("Content-Type") == HTTP_RESPONSE, f"{where}: Content-Type is {h.get_header('Content-Type')!r}")
    need(rec.http_headers is not None, f"{where}: no HTTP response head")
    status = int(rec.http_headers.get_statuscode())
    body = rec.raw_stream.read()
    length = rec.http_headers.get_header("Content-Length")
    need(length is not None and int(length) == len(body),
         f"{where}: HTTP Content-Length {length} but the stored body is {len(body)} bytes")
    truncated = h.get_header("WARC-Truncated")
    need(truncated in (None, "length"), f"{where}: WARC-Truncated is {truncated!r}")
    need(url not in responses, f"{where}: archived twice")
    responses[url] = (status, truncated == "length")


def check(out):
    files = sorted(out.glob("crawl-*.warc.gz"))
    names = [f.name for f in files]
    need(files, "no crawl-*.warc.gz files")
    need(names == [f"crawl-{i:05}.warc.gz" for i in range(len(files))], f"WARC files aren't numbered 0, 1, 2 …: {names}")
    responses = read_warcs(files)

    with (out / "pages.jsonl").open() as lines:
        rows = [json.loads(line) for line in lines]
    archived = [r for r in rows if r["simhash"] is not None]
    rows_by_url = {r["url"]: (r["status"], r["truncated"]) for r in archived}
    need(len(rows_by_url) == len(archived), "pages.jsonl archives a URL twice")
    missing = sorted(rows_by_url.keys() - responses.keys())
    extra = sorted(responses.keys() - rows_by_url.keys())
    need(not missing, f"{len(missing)} archived rows have no WARC response, e.g. {missing[:3]}")
    need(not extra, f"{len(extra)} WARC responses have no pages.jsonl row, e.g. {extra[:3]}")
    differ = sorted(u for u in responses if responses[u] != rows_by_url[u])
    need(not differ, f"{len(differ)} responses differ from their rows (status, truncated), e.g. "
                     + ", ".join(f"{u}: WARC {responses[u]}, row {rows_by_url[u]}" for u in differ[:3]))

    store = json.loads((out / "report.json").read_text())["store"]
    on_disk = {"warc_files": len(files), "warc_records": len(responses), "records": len(rows)}
    reported = {k: store[k] for k in on_disk}
    need(reported == on_disk, f"report.json says {reported}, the files hold {on_disk}")
    truncated = sum(t for _, t in responses.values())
    return f"{len(files)} WARC files, {len(responses)} responses ({truncated} truncated), {len(rows)} rows"


def main(dirs):
    if not dirs:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    for d in map(Path, dirs):
        try:
            print(f"OK {d}: {check(d)}")
        except Bad as e:
            print(f"FAIL {d}: {e}", file=sys.stderr)
            return 1
        except Exception as e:  # warcio, gzip or JSON refusing the files is a failure too
            print(f"FAIL {d}: {type(e).__name__}: {e}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
